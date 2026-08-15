use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type PeerId = Uuid;
pub type ChannelId = Uuid;

/// An opaque WebRTC handshake payload. The relay never inspects these —
/// it only routes them to the addressed peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignalPayload {
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
}

/// Sent by a client immediately after the WebSocket connection opens.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Claim a channel this connection will act as host for.
    Host { channel: ChannelId },
    /// Join a channel that's already claimed by a host.
    Join { channel: ChannelId },
    /// Subscribe to a channel's live occupant count without becoming a host
    /// or participant — used to show "N in this call" before joining. Never
    /// media-relayed to and invisible to real occupants (no `PeerJoined` is
    /// emitted for a watcher).
    Watch { channel: ChannelId },
    /// Route a signaling payload to another peer in the same channel.
    Relay { to: PeerId, payload: SignalPayload },
    /// Enter a room: a hostless membership set keyed like a channel (in
    /// practice the *server's* id, so one room spans all its text channels).
    /// Unlike Host/Join there's no host to wait for — a room exists as soon
    /// as anyone is in it. Used for the chat mesh: every member of a server
    /// sits in its room for as long as their app is open.
    Room { channel: ChannelId },
    /// Send an opaque payload to every other member of the room this
    /// connection entered. The relay never inspects it — chat semantics
    /// (messages, history sync, ...) live entirely in the peers, keeping the
    /// relay the same dumb router it is for WebRTC signaling.
    Publish { payload: serde_json::Value },
    /// Send an opaque payload to one specific member of this connection's
    /// room — used for pairwise exchanges like history backfill, where
    /// broadcasting a full message log to everyone would be noise.
    Direct { to: PeerId, payload: serde_json::Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Acknowledges Host/Join and reports the connection's assigned id, plus
    /// the channel's current host (so a joining peer knows who to address
    /// its offer to). `None` only alongside an `Error` for "no host yet".
    Welcome {
        your_peer: PeerId,
        host: Option<PeerId>,
    },
    /// A new participant joined the channel.
    PeerJoined { peer: PeerId },
    /// A participant disconnected.
    PeerLeft { peer: PeerId },
    /// A signaling payload routed from another peer in the channel.
    Relay { from: PeerId, payload: SignalPayload },
    /// Sent to a `Watch`ing connection immediately on subscribing, and again
    /// every time the channel's host/participant count changes.
    Presence { count: usize },
    /// Acknowledges `Room`: this connection's assigned id plus everyone
    /// already in the room — enough for the joiner to immediately pick a
    /// peer to backfill history from. Membership changes after this arrive
    /// as the same `PeerJoined`/`PeerLeft` channels use.
    RoomWelcome { your_peer: PeerId, peers: Vec<PeerId> },
    /// A room broadcast (`Publish`) from another member.
    Published { from: PeerId, payload: serde_json::Value },
    /// A room `Direct` payload addressed to this member.
    Direct { from: PeerId, payload: serde_json::Value },
    Error { message: String },
}
