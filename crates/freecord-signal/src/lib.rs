//! Ephemeral wss:// signaling: carries SDP/ICE handshake messages only,
//! hosted by whichever peer created the server (see PROJECT.md, decision 1).
//!
//! [`relay`] is run by the host; [`client`] is used by everyone connecting
//! to it (including the host connecting to itself, if that ends up being
//! convenient). Trust is fingerprint-pinning off the invite link rather
//! than a CA — see [`client::connect`] for why.

pub mod client;
pub mod protocol;
pub mod relay;

pub use protocol::{ChannelId, ClientMessage, PeerId, ServerMessage, SignalPayload};
pub use relay::RelayIdentity;
