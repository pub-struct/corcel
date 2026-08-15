//! Converts between wire-format RTP bytes (what QUIC datagrams carry) and
//! `rtc::rtp::Packet` (what `corcel-media`'s pipelines produce and
//! consume). Mirror of `corcel-media`'s own helper of the same name — the
//! dependency points the other way, so it can't be shared.

use bytes::Bytes;
use rtc::shared::marshal::{Marshal, Unmarshal};

pub fn unmarshal(bytes: &[u8]) -> anyhow::Result<rtc::rtp::Packet> {
    let mut buf = Bytes::copy_from_slice(bytes);
    Ok(rtc::rtp::Packet::unmarshal(&mut buf)?)
}

pub fn marshal(packet: &rtc::rtp::Packet) -> anyhow::Result<Bytes> {
    Ok(packet.marshal()?.freeze())
}
