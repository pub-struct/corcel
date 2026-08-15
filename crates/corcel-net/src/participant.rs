//! The joining side of a call: one QUIC connection to the host's relay
//! endpoint, which both uploads this participant's own tracks and receives
//! every other participant's tracks forwarded by the host (PROJECT.md
//! decision 6).
//!
//! Counterpart to [`crate::relay`], which is the host side of the same
//! exchange.

use std::collections::HashMap;

use anyhow::Context;
use corcel_signal::relay::MEDIA_ALPN;
use corcel_signal::{ChannelId, EndpointId};
use iroh::endpoint::{Connection, SendDatagramError};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::{H264_PAYLOAD_TYPE, Hello, OPUS_PAYLOAD_TYPE, rtp};

/// An opaque handle to a participant's media connection — lets the app
/// hold onto and pass around "the connection to publish more tracks on"
/// (e.g. to start screen sharing after the call is already under way)
/// without naming any transport type directly.
#[derive(Clone)]
pub struct CallHandle(Connection);

impl CallHandle {
    /// Closes the connection to the host: its forwarding loop notices and
    /// drops this participant from the call. Doesn't stop any local
    /// capture/playback — callers that spawned tasks around this call are
    /// still responsible for stopping those themselves.
    pub fn close(&self) {
        self.0.close(0u32.into(), b"hang up");
    }
}

/// What kind of media a [`RemoteTrack`] carries, told apart by the RTP
/// payload type its packets are stamped with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Video,
}

/// One remote media stream the host is forwarding to this participant —
/// each distinct SSRC seen on the connection becomes one of these.
pub struct RemoteTrack {
    pub kind: TrackKind,
    pub packets: mpsc::UnboundedReceiver<rtc::rtp::Packet>,
}

/// A live connection to a call's host.
pub struct Participant {
    pub pc: CallHandle,
    /// Streams the host forwards as they first appear, from any other
    /// participant.
    pub tracks: mpsc::UnboundedReceiver<RemoteTrack>,
}

/// Joins the voice channel `channel` on the server whose relay is `relay`:
/// dials the media ALPN, declares the channel, waits for the host's ack,
/// and spawns the demux task that turns incoming datagrams into
/// [`RemoteTrack`]s. The task ends (closing every track's channel with it)
/// when the connection does.
pub async fn join(relay: EndpointId, channel: ChannelId) -> anyhow::Result<Participant> {
    let conn = corcel_signal::client::dial(relay, MEDIA_ALPN).await?;

    let (mut writer, reader) = conn.open_bi().await?;
    let mut hello = serde_json::to_string(&Hello { channel })?;
    hello.push('\n');
    writer.write_all(hello.as_bytes()).await?;
    // The ack means the host has registered us for forwarding — anything
    // published from here on reaches the rest of the channel.
    BufReader::new(reader)
        .lines()
        .next_line()
        .await?
        .context("media connection closed before the host acknowledged the join")?;

    let (track_tx, track_rx) = mpsc::unbounded_channel();
    let demux_conn = conn.clone();
    tokio::spawn(async move {
        let mut tracks: HashMap<u32, mpsc::UnboundedSender<rtc::rtp::Packet>> = HashMap::new();
        // A read error is the connection closing — normal end of a call.
        while let Ok(datagram) = demux_conn.read_datagram().await {
            let Ok(packet) = rtp::unmarshal(&datagram) else { continue };
            let kind = match packet.header.payload_type {
                OPUS_PAYLOAD_TYPE => TrackKind::Audio,
                H264_PAYLOAD_TYPE => TrackKind::Video,
                _ => continue, // not a corcel stream; drop
            };
            let sender = tracks.entry(packet.header.ssrc).or_insert_with(|| {
                let (tx, rx) = mpsc::unbounded_channel();
                let _ = track_tx.send(RemoteTrack { kind, packets: rx });
                tx
            });
            let _ = sender.send(packet);
        }
    });

    Ok(Participant { pc: CallHandle(conn), tracks: track_rx })
}

/// A local media stream being published into the call via
/// [`publish_audio_track`] or [`publish_video_track`].
pub struct OutgoingTrack {
    conn: Connection,
    ssrc: u32,
}

impl OutgoingTrack {
    /// Sends one RTP packet (e.g. from `corcel_media::capture`'s output).
    /// Returns `false` once the underlying connection is gone — the caller
    /// should stop pushing at that point.
    ///
    /// GStreamer's `rtpopuspay`/`rtph264pay` stamp each packet with their
    /// own pipeline-local (random) SSRC; rewriting it to this track's
    /// locally-unique one guarantees the host can tell this participant's
    /// own streams apart.
    pub fn write_rtp(&self, mut packet: rtc::rtp::Packet) -> bool {
        packet.header.ssrc = self.ssrc;
        let Ok(bytes) = rtp::marshal(&packet) else {
            return true; // malformed packet; skip it, keep the stream
        };
        match self.conn.send_datagram(bytes) {
            Ok(()) => true,
            // Doesn't fit the path's MTU right now — dropped, exactly like
            // any lossy network drops a too-big packet. The capture
            // pipelines payload well under QUIC's minimum MTU, so this is
            // a transient, not a systematically dead stream.
            Err(SendDatagramError::TooLarge) => true,
            Err(_) => false,
        }
    }
}

/// Declares a new audio stream on `pc`, ready to receive packets via the
/// returned [`OutgoingTrack`]. Infallible and instant — with no SDP there
/// is nothing to negotiate; the stream simply exists once packets flow.
pub fn publish_audio_track(pc: &CallHandle) -> OutgoingTrack {
    publish_track(pc)
}

/// Same as [`publish_audio_track`], for video (camera or screen share).
pub fn publish_video_track(pc: &CallHandle) -> OutgoingTrack {
    publish_track(pc)
}

fn publish_track(pc: &CallHandle) -> OutgoingTrack {
    OutgoingTrack { conn: pc.0.clone(), ssrc: next_ssrc() }
}

/// Scoped to one published track's SSRC — a process-wide monotonic counter
/// is sufficient uniqueness for that (mirrors `relay::next_ssrc`).
fn next_ssrc() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
