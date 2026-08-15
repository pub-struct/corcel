//! Each server's rendezvous point over iroh: the relay every member's chat
//! room connection and call-media connection dials, hosted by whichever
//! peer created the server (see PROJECT.md, decision 1).
//!
//! [`relay`] is run by the host; [`client`] is used by everyone connecting
//! to it (including the host connecting to itself — see
//! [`client::connect`]). Invite links carry the relay's iroh
//! [`iroh::EndpointId`]; iroh's public relay/DNS infrastructure handles
//! rendezvous and hole-punching, and the QUIC handshake against that id
//! *is* the trust model — no CA, no certificate pinning.

pub mod client;
pub mod protocol;
pub mod relay;

pub use iroh::{EndpointAddr, EndpointId, TransportAddr};
pub use protocol::{ChannelId, ClientMessage, PeerId, ServerMessage};
pub use relay::{Reach, RelayIdentity};
