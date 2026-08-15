//! Shared WebRTC configuration: the codec set and ICE server list every peer
//! connection in corcel-net is built with (PROJECT.md decisions 4 and 5 —
//! H264-only video, STUN-only NAT traversal).

use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_RTX};
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind,
};
use webrtc::peer_connection::{MediaEngine, RTCConfiguration, RTCConfigurationBuilder, RTCIceServer};

/// The RTP payload type corcel uses for H264. Fixed (rather than
/// negotiated) because both sides of every connection are built from this
/// same `media_engine()`.
pub const H264_PAYLOAD_TYPE: u8 = 102;

/// The RTP payload type corcel uses for Opus, for the same reason as
/// [`H264_PAYLOAD_TYPE`].
pub const OPUS_PAYLOAD_TYPE: u8 = 111;

/// The Opus parameters every corcel audio track (published or forwarded)
/// is built with — shared between codec registration here and the actual
/// outgoing tracks in [`crate::media`]/[`crate::relay`], since a mismatch
/// between what's registered and what's advertised on a track breaks
/// negotiation.
pub fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

/// The H264 parameters every corcel video track is built with — see
/// [`opus_codec`].
pub fn h264_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"
            .to_owned(),
        rtcp_feedback: vec![
            RTCPFeedback {
                typ: "goog-remb".to_owned(),
                parameter: "".to_owned(),
            },
            RTCPFeedback {
                typ: "ccm".to_owned(),
                parameter: "fir".to_owned(),
            },
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: "".to_owned(),
            },
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: "pli".to_owned(),
            },
        ],
    }
}

/// Builds a [`MediaEngine`] that only knows the codecs corcel speaks: Opus
/// for audio, H264 (plus its RTX retransmission pair) for video. Deliberately
/// narrower than [`MediaEngine::register_default_codecs`], which also
/// registers VP8/VP9/AV1 — SDP negotiation should never be able to land on
/// those.
pub fn media_engine() -> anyhow::Result<MediaEngine> {
    let mut engine = MediaEngine::default();

    engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: opus_codec(),
            payload_type: OPUS_PAYLOAD_TYPE,
        },
        RtpCodecKind::Audio,
    )?;

    engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: h264_codec(),
            payload_type: H264_PAYLOAD_TYPE,
        },
        RtpCodecKind::Video,
    )?;
    engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_RTX.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: format!("apt={H264_PAYLOAD_TYPE}"),
                rtcp_feedback: vec![],
            },
            payload_type: 103,
        },
        RtpCodecKind::Video,
    )?;

    Ok(engine)
}

/// STUN-only ICE configuration — no TURN relay yet (PROJECT.md decision 5).
pub fn ice_configuration() -> RTCConfiguration {
    RTCConfigurationBuilder::new()
        .with_ice_servers(vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Default::default()
        }])
        .build()
}
