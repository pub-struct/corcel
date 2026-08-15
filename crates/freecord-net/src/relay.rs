//! Host-relay (SFU-lite) forwarding: each participant uploads once to the
//! host, and the host forwards already-encoded RTP packets to every other
//! participant — not a full mesh (PROJECT.md decision 6). This is the host
//! side only; see [`crate::participant`] for the joining side.
//!
//! The host holds one [`PeerConnection`] per participant. Signaling for all
//! of them is multiplexed over a single websocket: the host connects to its
//! own local `freecord-signal` relay as [`ClientMessage::Host`], and routes
//! each [`ServerMessage::Relay`] by its `from` peer to that peer's
//! connection (see [`crate::signal::handle`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use freecord_signal::client::Connection;
use freecord_signal::{ClientMessage, PeerId, ServerMessage};
use rtc::media_stream::MediaStreamTrack;
use rtc::rtp;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::{Mutex, broadcast, mpsc};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionEventHandler, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};

use crate::{pc, signal};

/// Packets a slow subscriber can fall behind by before it starts dropping
/// them, per forwarded track.
const FORWARD_CHANNEL_CAPACITY: usize = 256;

/// One participant's connection into the host relay.
struct Endpoint {
    peer: PeerId,
    pc: Arc<dyn PeerConnection>,
}

#[derive(Clone)]
struct PublishedTrack {
    kind: RtpCodecKind,
    codec: RTCRtpCodec,
    tx: broadcast::Sender<rtp::Packet>,
}

pub struct HostRelay {
    outbound: mpsc::UnboundedSender<ClientMessage>,
    endpoints: Mutex<HashMap<PeerId, Arc<Endpoint>>>,
    published: Mutex<HashMap<PeerId, Vec<PublishedTrack>>>,
}

impl HostRelay {
    /// Drives a host's relay for the lifetime of `host_conn` (the host's own
    /// connection to its local `freecord-signal` relay, opened with
    /// [`ClientMessage::Host`]). Returns once that connection's inbound
    /// stream closes.
    pub async fn run(host_conn: Connection) -> anyhow::Result<()> {
        let relay = Arc::new(HostRelay {
            outbound: host_conn.outbound,
            endpoints: Mutex::new(HashMap::new()),
            published: Mutex::new(HashMap::new()),
        });

        let mut inbound = host_conn.inbound;
        while let Some(msg) = inbound.recv().await {
            match msg {
                ServerMessage::Relay { from, payload } => {
                    let endpoint = match relay.endpoint_for(from).await {
                        Ok(endpoint) => endpoint,
                        Err(err) => {
                            eprintln!("freecord-net: failed to set up endpoint for {from}: {err:#}");
                            continue;
                        }
                    };
                    // Host is the impolite side of every connection (see
                    // `signal`'s module docs) — arbitrary but consistent, and
                    // it means the host's own track-forwarding offers always
                    // win a collision rather than getting rolled back.
                    if let Err(err) = signal::handle(&endpoint.pc, &relay.outbound, from, payload, false).await {
                        eprintln!("freecord-net: signaling error for {from}: {err:#}");
                    }
                }
                ServerMessage::PeerLeft { peer } => relay.remove_endpoint(peer).await,
                ServerMessage::PeerJoined { .. }
                | ServerMessage::Welcome { .. }
                | ServerMessage::Presence { .. }
                // Room traffic never reaches a Host/Join connection — these
                // arms exist only so this match stays exhaustive.
                | ServerMessage::RoomWelcome { .. }
                | ServerMessage::Published { .. }
                | ServerMessage::Direct { .. }
                | ServerMessage::Error { .. } => {}
            }
        }

        Ok(())
    }

    async fn endpoint_for(self: &Arc<Self>, peer: PeerId) -> anyhow::Result<Arc<Endpoint>> {
        if let Some(existing) = self.endpoints.lock().await.get(&peer).cloned() {
            return Ok(existing);
        }

        let handler = Arc::new(RelayHandler {
            relay: Arc::clone(self),
            peer,
            negotiating: AtomicBool::new(false),
        });
        let peer_connection = pc::build(handler).await?;
        let endpoint = Arc::new(Endpoint {
            peer,
            pc: peer_connection,
        });
        self.endpoints.lock().await.insert(peer, Arc::clone(&endpoint));

        // Late joiner: subscribe it to every track already being published
        // by other peers, so it doesn't miss streams already in progress.
        let already_published: Vec<(PeerId, PublishedTrack)> = self
            .published
            .lock()
            .await
            .iter()
            .filter(|(publisher, _)| **publisher != peer)
            .flat_map(|(publisher, tracks)| tracks.iter().cloned().map(|t| (*publisher, t)))
            .collect();
        for (publisher, track) in already_published {
            self.add_forwarded_track(&endpoint, publisher, track).await;
        }

        Ok(endpoint)
    }

    async fn remove_endpoint(&self, peer: PeerId) {
        self.endpoints.lock().await.remove(&peer);
        self.published.lock().await.remove(&peer);
    }

    /// Registers a newly received remote track as published by `publisher`
    /// and starts forwarding it to every other current (and future)
    /// endpoint.
    async fn publish(self: &Arc<Self>, publisher: PeerId, track: Arc<dyn TrackRemote>) {
        let Some(ssrc) = track.ssrcs().await.into_iter().next() else {
            return;
        };
        let Some(codec) = track.codec(ssrc).await else {
            return;
        };
        let kind = track.kind().await;

        let (tx, _rx) = broadcast::channel(FORWARD_CHANNEL_CAPACITY);
        let published = PublishedTrack {
            kind,
            codec,
            tx: tx.clone(),
        };
        self.published
            .lock()
            .await
            .entry(publisher)
            .or_default()
            .push(published.clone());

        // Drain this publisher's incoming RTP into the broadcast channel
        // that every forwarder task below reads from.
        tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                    let _ = tx.send(packet);
                }
            }
        });

        let targets: Vec<Arc<Endpoint>> = self
            .endpoints
            .lock()
            .await
            .values()
            .filter(|e| e.peer != publisher)
            .cloned()
            .collect();
        for target in targets {
            self.add_forwarded_track(&target, publisher, published.clone()).await;
        }
    }

    /// Adds a track to `target`'s connection that forwards `published`'s RTP
    /// stream, rewritten onto a fresh SSRC scoped to this one forwarding
    /// hop. Triggers renegotiation on `target` via `on_negotiation_needed`.
    async fn add_forwarded_track(&self, target: &Arc<Endpoint>, publisher: PeerId, published: PublishedTrack) {
        let ssrc = next_ssrc();
        let local_track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            format!("freecord-{publisher}"),
            format!("freecord-{publisher}-{:?}-{ssrc}", published.kind),
            format!("freecord-{publisher}-{:?}", published.kind),
            published.kind,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: published.codec,
                ..Default::default()
            }],
        )));

        if let Err(err) = target
            .pc
            .add_track(Arc::clone(&local_track) as Arc<dyn TrackLocal>)
            .await
        {
            eprintln!(
                "freecord-net: failed to add forwarded track from {publisher} to {}: {err:#}",
                target.peer
            );
            return;
        }

        let mut rx = published.tx.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(mut packet) => {
                        packet.header.ssrc = ssrc;
                        if local_track.write_rtp(packet).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

/// Scoped to one forwarding hop's SSRC, not the original publisher's — a
/// process-wide monotonic counter is sufficient uniqueness for that.
fn next_ssrc() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

struct RelayHandler {
    relay: Arc<HostRelay>,
    peer: PeerId,
    /// Coalesces concurrent `on_negotiation_needed` firings (e.g. two tracks
    /// forwarded to the same endpoint back to back) into a single
    /// offer/answer round trip. A collision here is dropped rather than
    /// queued — acceptable for now since each `add_track` call still landed
    /// before the offer was created, so nothing is lost, only batched.
    negotiating: AtomicBool,
}

impl RelayHandler {
    async fn renegotiate(&self) -> anyhow::Result<()> {
        let endpoint = self.relay.endpoints.lock().await.get(&self.peer).cloned();
        let Some(endpoint) = endpoint else {
            return Ok(());
        };
        let offer = endpoint.pc.create_offer(None).await?;
        endpoint.pc.set_local_description(offer.clone()).await?;
        self.relay.outbound.send(ClientMessage::Relay {
            to: self.peer,
            payload: freecord_signal::SignalPayload::Offer { sdp: offer.sdp },
        })?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for RelayHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let Ok(init) = event.candidate.to_json() else {
            return;
        };
        let _ = self.relay.outbound.send(ClientMessage::Relay {
            to: self.peer,
            payload: freecord_signal::SignalPayload::IceCandidate {
                candidate: init.candidate,
                sdp_mid: init.sdp_mid,
                sdp_mline_index: init.sdp_mline_index,
            },
        });
    }

    async fn on_negotiation_needed(&self) {
        if self.negotiating.swap(true, Ordering::AcqRel) {
            return;
        }
        let result = self.renegotiate().await;
        self.negotiating.store(false, Ordering::Release);
        if let Err(err) = result {
            eprintln!("freecord-net: renegotiation with {} failed: {err:#}", self.peer);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
            self.relay.remove_endpoint(self.peer).await;
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        self.relay.publish(self.peer, track).await;
    }
}
