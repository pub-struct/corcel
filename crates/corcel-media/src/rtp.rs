//! Converts between wire-format RTP bytes (what GStreamer's `appsink`/
//! `appsrc` deal in) and `rtc::rtp::Packet` (what `corcel-net` deals in).

use bytes::Bytes;
use rtc::shared::marshal::{Marshal, Unmarshal};

pub fn unmarshal(bytes: &[u8]) -> anyhow::Result<rtc::rtp::Packet> {
    let mut buf = Bytes::copy_from_slice(bytes);
    Ok(rtc::rtp::Packet::unmarshal(&mut buf)?)
}

/// Returns the marshaled packet as `Bytes` rather than `Vec<u8>`: the only
/// consumers hand it straight to `gstreamer::Buffer::from_slice`, which
/// accepts any `AsRef<[u8]>` owner — so the extra `.to_vec()` copy this used
/// to do per packet bought nothing.
pub fn marshal(packet: &rtc::rtp::Packet) -> anyhow::Result<Bytes> {
    Ok(packet.marshal()?.freeze())
}
