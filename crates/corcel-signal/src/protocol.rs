use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type PeerId = Uuid;
pub type ChannelId = Uuid;

/// Sent by a client over its relay connection. The first message must be
/// [`ClientMessage::Room`]; everything after is room traffic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Enter a room: a hostless membership set keyed like a channel (in
    /// practice the *server's* id, so one room spans all its text channels).
    /// A room exists as soon as anyone is in it. Used for the chat mesh:
    /// every member of a server sits in its room for as long as their app
    /// is open.
    Room { channel: ChannelId },
    /// Send an opaque payload to every other member of the room this
    /// connection entered. The relay never inspects it — chat semantics
    /// (messages, history sync, ...) live entirely in the peers, keeping the
    /// relay a dumb router.
    Publish { payload: serde_json::Value },
    /// Send an opaque payload to one specific member of this connection's
    /// room — used for pairwise exchanges like history backfill, where
    /// broadcasting a full message log to everyone would be noise.
    Direct { to: PeerId, payload: serde_json::Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Acknowledges `Room`: this connection's assigned id plus everyone
    /// already in the room — enough for the joiner to immediately pick a
    /// peer to backfill history from.
    RoomWelcome { your_peer: PeerId, peers: Vec<PeerId> },
    /// A new member entered this connection's room.
    PeerJoined { peer: PeerId },
    /// A member's room connection dropped.
    PeerLeft { peer: PeerId },
    /// A room broadcast (`Publish`) from another member.
    Published { from: PeerId, payload: serde_json::Value },
    /// A room `Direct` payload addressed to this member.
    Direct { from: PeerId, payload: serde_json::Value },
    Error { message: String },
}
