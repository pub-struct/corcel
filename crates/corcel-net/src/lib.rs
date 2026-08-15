//! Call media over iroh: already-encoded RTP packets ride QUIC datagrams
//! on the [`corcel_signal::relay::MEDIA_ALPN`] of the server's relay
//! endpoint — the same connection machinery (and the same NAT traversal)
//! that chat uses, so voice works everywhere chat does.
//!
//! Topology is host-relay (SFU-lite, PROJECT.md decision 6): each
//! participant uploads its tracks once to the host, and the host forwards
//! them to every other participant in the same channel ([`relay`] is the
//! host side, [`participant`] the joining side). Datagrams are the right
//! QUIC primitive for RTP: unreliable and unordered, so a lost packet
//! costs a moment of audio (Opus conceals it) instead of stalling
//! everything behind a retransmit.
//!
//! There is no SDP, no ICE, and no negotiation: both ends stamp and parse
//! packets with the fixed payload types below, and streams are told apart
//! by their RTP SSRC (the host rewrites SSRCs so no two forwarded streams
//! can collide).

mod rtp;

pub mod participant;
pub mod relay;

use serde::{Deserialize, Serialize};

/// The RTP payload type corcel uses for H264. Fixed (rather than
/// negotiated) because both ends of every connection stamp packets from
/// this same constant — it's also how a receiver knows a stream is video.
pub const H264_PAYLOAD_TYPE: u8 = 102;

/// The RTP payload type corcel uses for Opus, for the same reason as
/// [`H264_PAYLOAD_TYPE`].
pub const OPUS_PAYLOAD_TYPE: u8 = 111;

/// The first (and only) message on a media connection's control stream,
/// sent by the participant: which voice channel it's joining. The host
/// acks with one (content-free) line once the participant is registered
/// for forwarding.
#[derive(Serialize, Deserialize)]
pub(crate) struct Hello {
    pub channel: corcel_signal::ChannelId,
}

pub use participant::{
    CallHandle, OutgoingTrack, Participant, RemoteTrack, TrackKind, join, publish_audio_track,
    publish_video_track,
};
pub use relay::HostRelay;
