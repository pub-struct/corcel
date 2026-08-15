//! The chat payloads peers exchange through a server's relay room, and the
//! replication scheme that keeps every member's local database converging
//! on the same history.
//!
//! Philosophy (the user-facing promise): there is no message server. Every
//! member stores every message in their own SQLite database, new messages
//! are broadcast to whoever's online, and anyone who was offline catches up
//! by asking *any* online member for what they missed ([`ChatPayload::HistoryRequest`]).
//! The server's owner is just another member — their machine only matters
//! as the room's meeting point (the relay), not as the source of truth.
//!
//! Convergence relies on two properties: message ids are unique (UUIDv4, so
//! `INSERT OR IGNORE` makes replication idempotent), and history requests
//! over-fetch (`since` backs up [`HISTORY_OVERLAP_MILLIS`] before the
//! requester's newest message) so modest clock skew between authors can't
//! open a permanent gap.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use freecord_signal::ChannelId;

/// How far behind its own newest message a peer asks for history from, to
/// absorb clock skew between authors. One hour is far beyond any sane NTP
/// drift; the redundant messages it re-fetches are deduped by id anyway.
pub const HISTORY_OVERLAP_MILLIS: i64 = 60 * 60 * 1000;

/// One chat message, exactly as it's stored and as it travels. `sent_at` is
/// the author's clock (unix millis) — good enough for display ordering in a
/// friends-scale app, and the basis for `since` backfill windows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: Uuid,
    pub channel: ChannelId,
    pub author: String,
    pub sent_at: i64,
    pub body: String,
}

/// Everything peers say to each other inside a server's room. Serialized to
/// JSON and carried opaquely by the relay's `Publish`/`Direct` routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatPayload {
    /// A new message, broadcast to the whole room. Receivers store it and
    /// (if they're looking at its channel) render it.
    Message(ChatMessage),
    /// "Send me everything for this server newer than `since`" — sent
    /// directly to one online peer right after entering the room.
    HistoryRequest { since: i64 },
    /// The answer to a [`ChatPayload::HistoryRequest`]: every message the
    /// responder has that's newer than the requested `since`, in one batch
    /// (fine at friends scale; chunking can come when someone's history is
    /// big enough to need it).
    HistoryBatch { messages: Vec<ChatMessage> },
}

/// The author's current unix time in milliseconds — the timestamp stamped
/// on outgoing [`ChatMessage`]s.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
