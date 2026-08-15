//! Bridges `freecord-media`'s RTP packet streams to a [`crate::Participant`]'s
//! peer connection, so the webrtc-rs specifics (building a
//! [`TrackLocalStaticRTP`], matching the registered codecs, filtering
//! [`TrackRemote`] by kind) stay behind this crate's boundary rather than
//! leaking into `freecord-app`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rtc::media_stream::MediaStreamTrack;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::mpsc;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::PeerConnection;

use crate::config;

/// An opaque handle to a [`crate::Participant`]'s peer connection — lets
/// `freecord-app` hold onto and pass around "the connection to publish more
/// tracks on" (e.g. to start screen sharing after the call is already under
/// way) without naming any webrtc-rs type directly.
#[derive(Clone)]
pub struct CallHandle(pub(crate) Arc<dyn PeerConnection>);

impl CallHandle {
    /// Closes the underlying connection: ICE/DTLS tears down, and the far
    /// side notices via `on_connection_state_change` (for the host, that
    /// removes this peer's relay endpoint too). Doesn't stop any local
    /// capture/playback — callers that spawned tasks around this connection
    /// (e.g. [`crate::Participant::pc`]'s owner) are still responsible for
    /// stopping those themselves.
    pub async fn close(&self) -> anyhow::Result<()> {
        self.0.close().await.map_err(Into::into)
    }
}

/// A local track added to a peer connection via [`publish_audio_track`] or
/// [`publish_video_track`].
pub struct OutgoingTrack {
    track: Arc<TrackLocalStaticRTP>,
    ssrc: u32,
}

impl OutgoingTrack {
    /// Writes one RTP packet (e.g. from `freecord_media::capture`'s output).
    /// Returns `false` once the underlying connection can no longer accept
    /// packets — the caller should stop pushing at that point.
    ///
    /// GStreamer's `rtpopuspay`/`rtph264pay` stamp each packet with their own
    /// pipeline-local SSRC, which has nothing to do with the SSRC this track
    /// was registered under in the SDP (`ssrc`, below). webrtc-rs's sender
    /// rejects any packet whose SSRC it doesn't recognize
    /// (`ErrSenderWithNoSSRCs`), so it must be rewritten to match before
    /// writing — same fix `relay.rs`'s forwarding path already applies.
    pub async fn write_rtp(&self, mut packet: rtc::rtp::Packet) -> bool {
        packet.header.ssrc = self.ssrc;
        self.track.write_rtp(packet).await.is_ok()
    }
}

/// Scoped to one track's SSRC — a process-wide monotonic counter is
/// sufficient uniqueness for that (mirrors `relay::next_ssrc`).
fn next_ssrc() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

async fn publish_track(
    pc: &CallHandle,
    kind: RtpCodecKind,
    codec: RTCRtpCodec,
    label: &str,
) -> anyhow::Result<OutgoingTrack> {
    let ssrc = next_ssrc();
    let local_track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
        format!("freecord-{label}"),
        format!("freecord-{label}-{ssrc}"),
        label.to_string(),
        kind,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec,
            ..Default::default()
        }],
    )));
    pc.0.add_track(Arc::clone(&local_track) as Arc<dyn TrackLocal>).await?;
    Ok(OutgoingTrack { track: local_track, ssrc })
}

/// Adds a local audio track to `pc`, ready to receive packets via the
/// returned [`OutgoingTrack`].
pub async fn publish_audio_track(pc: &CallHandle) -> anyhow::Result<OutgoingTrack> {
    publish_track(pc, RtpCodecKind::Audio, config::opus_codec(), "microphone").await
}

/// Adds a local video track to `pc` (e.g. for camera or screen share),
/// ready to receive packets via the returned [`OutgoingTrack`].
pub async fn publish_video_track(pc: &CallHandle) -> anyhow::Result<OutgoingTrack> {
    publish_track(pc, RtpCodecKind::Video, config::h264_codec(), "video").await
}

async fn subscribe(track: Arc<dyn TrackRemote>, kind: RtpCodecKind) -> Option<mpsc::UnboundedReceiver<rtc::rtp::Packet>> {
    if track.kind().await != kind {
        return None;
    }

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = track.poll().await {
            if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                if tx.send(packet).is_err() {
                    break;
                }
            }
        }
    });
    Some(rx)
}

/// If `track` is an audio track, spawns a task draining its incoming RTP
/// packets into the returned channel and returns `Some`. Returns `None`
/// (and drains nothing) for any other kind.
pub async fn subscribe_audio(track: Arc<dyn TrackRemote>) -> Option<mpsc::UnboundedReceiver<rtc::rtp::Packet>> {
    subscribe(track, RtpCodecKind::Audio).await
}

/// Same as [`subscribe_audio`], for video tracks.
pub async fn subscribe_video(track: Arc<dyn TrackRemote>) -> Option<mpsc::UnboundedReceiver<rtc::rtp::Packet>> {
    subscribe(track, RtpCodecKind::Video).await
}
