//! webrtc-rs based P2P connections (ICE/DTLS/SRTP/data channels) plus
//! host-relay (SFU-lite) packet forwarding. See PROJECT.md, decisions 3 and 6.

mod media;
mod pc;
mod signal;

pub mod config;
pub mod participant;
pub mod relay;

pub use config::{H264_PAYLOAD_TYPE, OPUS_PAYLOAD_TYPE};
pub use media::{
    CallHandle, OutgoingTrack, publish_audio_track, publish_video_track, subscribe_audio, subscribe_video,
};
pub use participant::{Participant, join};
pub use relay::HostRelay;
