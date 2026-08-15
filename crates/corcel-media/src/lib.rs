//! GStreamer capture (PipeWire/xdg-desktop-portal ScreenCast, camera, mic)
//! and VAAPI-accelerated H264 encode/decode. See PROJECT.md, decisions 2-4.
//!
//! Every capture and playback pipeline here exchanges already RTP-
//! packetized media (`rtc::rtp::Packet`) with the caller — GStreamer's
//! `rtpXpay`/`rtpXdepay` elements do the packetizing/depacketizing, so what
//! crosses the boundary into `corcel-net` is exactly what
//! `TrackLocal::write_rtp` produces and `TrackRemoteEvent::OnRtpPacket`
//! delivers, with no separate framing layer of our own.

mod codec;
mod pipeline;
mod rtp;

pub mod capture;
pub mod playback;
pub mod player;

pub use capture::Capture;
pub use playback::{AudioPlayback, VideoFrame, VideoPlayback};
pub use player::{PlayerEvent, UrlPlayer};

/// Initializes GStreamer. Must be called once before any pipeline is built;
/// safe to call more than once.
pub fn init() -> anyhow::Result<()> {
    gstreamer::init().map_err(Into::into)
}
