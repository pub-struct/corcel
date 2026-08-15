//! The joining side of a call: one [`PeerConnection`] to the host, which
//! both uploads this participant's own tracks and receives every other
//! participant's tracks forwarded by the host (PROJECT.md decision 6).
//!
//! Counterpart to [`crate::relay`], which is the host side of the same
//! exchange.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, bail};
use freecord_signal::client::Connection;
use freecord_signal::{ClientMessage, PeerId, ServerMessage, SignalPayload};
use tokio::sync::mpsc;
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionEventHandler, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};

use crate::media::CallHandle;
use crate::{pc, signal};

/// A live connection to the call's host.
pub struct Participant {
    pub pc: CallHandle,
    /// The host's id, for completeness — signaling is already routed for
    /// the caller by the background task `join` spawns.
    pub host: PeerId,
    /// Tracks the host has forwarded, from this participant or any other.
    pub tracks: mpsc::UnboundedReceiver<Arc<dyn TrackRemote>>,
}

/// Joins the call reachable over `conn` (a [`Connection`] opened with
/// [`ClientMessage::Join`]): waits for the host's [`ServerMessage::Welcome`],
/// builds a [`PeerConnection`] to it, sends the initial offer, and spawns a
/// task that keeps driving signaling (renegotiation, trickled ICE) for the
/// lifetime of the connection.
pub async fn join(mut conn: Connection) -> anyhow::Result<Participant> {
    let host = match conn.inbound.recv().await.context("signaling connection closed before Welcome")? {
        ServerMessage::Welcome { host: Some(host), .. } => host,
        ServerMessage::Welcome { host: None, .. } => bail!("channel has no host yet"),
        ServerMessage::Error { message } => bail!("signaling error: {message}"),
        other => bail!("expected Welcome, got {other:?}"),
    };

    let (track_tx, track_rx) = mpsc::unbounded_channel();
    let handler = Arc::new(ParticipantHandler {
        outbound: conn.outbound.clone(),
        host,
        negotiating: AtomicBool::new(false),
        pc: OnceLock::new(),
        tracks: track_tx,
    });
    let peer_connection = pc::build(handler.clone()).await?;
    let _ = handler.pc.set(Arc::clone(&peer_connection));

    // A `PeerConnection` with no tracks and no data channels produces an
    // offer with zero `m=` sections, which has nowhere to carry ICE
    // credentials at all — the host's `set_remote_description` then fails
    // with "no ice-ufrag". Creating this (otherwise unused) placeholder
    // channel triggers `on_negotiation_needed` below, which creates and
    // sends the actual initial offer — the *only* place that happens, so it
    // can't race a second, manually-triggered offer for the same session.
    peer_connection
        .create_data_channel("freecord-init", None)
        .await?;

    let driven_pc = Arc::clone(&peer_connection);
    let outbound = conn.outbound.clone();
    tokio::spawn(async move {
        while let Some(msg) = conn.inbound.recv().await {
            match msg {
                ServerMessage::Relay { from, payload } if from == host => {
                    // Participant is the polite side (see `signal`'s module
                    // docs) — yields to the host's offer on a collision.
                    if let Err(err) = signal::handle(&driven_pc, &outbound, from, payload, true).await {
                        eprintln!("freecord-net: signaling error with host: {err:#}");
                    }
                }
                ServerMessage::Relay { .. }
                | ServerMessage::PeerJoined { .. }
                | ServerMessage::PeerLeft { .. }
                | ServerMessage::Presence { .. }
                // Room traffic never reaches a Join connection.
                | ServerMessage::RoomWelcome { .. }
                | ServerMessage::Published { .. }
                | ServerMessage::Direct { .. } => {
                    // Only the host relays anything to a participant in the
                    // host-relay topology; nothing else to react to yet.
                }
                ServerMessage::Welcome { .. } => {}
                ServerMessage::Error { message } => {
                    eprintln!("freecord-net: signaling error: {message}");
                }
            }
        }
    });

    Ok(Participant {
        pc: CallHandle(peer_connection),
        host,
        tracks: track_rx,
    })
}

struct ParticipantHandler {
    outbound: mpsc::UnboundedSender<ClientMessage>,
    host: PeerId,
    /// See [`crate::relay::RelayHandler::negotiating`] — same simplification.
    negotiating: AtomicBool,
    /// Set once, right after [`pc::build`] returns, so callbacks (which are
    /// registered before the connection they belong to exists) can still
    /// reach it.
    pc: OnceLock<Arc<dyn PeerConnection>>,
    tracks: mpsc::UnboundedSender<Arc<dyn TrackRemote>>,
}

impl ParticipantHandler {
    async fn renegotiate(&self) -> anyhow::Result<()> {
        let Some(pc) = self.pc.get() else {
            return Ok(());
        };
        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer.clone()).await?;
        self.outbound.send(ClientMessage::Relay {
            to: self.host,
            payload: SignalPayload::Offer { sdp: offer.sdp },
        })?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for ParticipantHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let Ok(init) = event.candidate.to_json() else {
            return;
        };
        let _ = self.outbound.send(ClientMessage::Relay {
            to: self.host,
            payload: SignalPayload::IceCandidate {
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
            eprintln!("freecord-net: renegotiation with host failed: {err:#}");
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
            eprintln!("freecord-net: connection to host {} is {state:?}", self.host);
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.tracks.send(track);
    }
}
