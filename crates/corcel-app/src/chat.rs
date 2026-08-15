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

use corcel_signal::ChannelId;

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
    /// The message this one replies to, if any. `#[serde(default)]` keeps
    /// old builds' payloads (which lack the field) deserializing.
    #[serde(default)]
    pub reply_to: Option<Uuid>,
    /// When the author last edited this message (unix millis). `None` means
    /// never edited; larger always wins when merging (last-writer-wins).
    #[serde(default)]
    pub edited_at: Option<i64>,
    /// Tombstone: the author deleted this message. Monotone — once true it
    /// can never merge back to false, so deletes survive any backfill order.
    #[serde(default)]
    pub deleted: bool,
}

/// One member's reaction to one message, exactly as stored and replicated.
/// Keyed by `(message_id, emoji, author)`; `removed` is the un-react
/// tombstone and `at` is the author's clock — the newer `at` wins when two
/// states of the same key meet (react → un-react → re-react all converge).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionRow {
    pub message_id: Uuid,
    pub channel: ChannelId,
    pub emoji: String,
    pub author: String,
    pub at: i64,
    #[serde(default)]
    pub removed: bool,
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
    /// big enough to need it). `reactions` carries every reaction change
    /// (including removals) newer than `since` — `#[serde(default)]` so
    /// batches from old builds still parse.
    HistoryBatch {
        messages: Vec<ChatMessage>,
        #[serde(default)]
        reactions: Vec<ReactionRow>,
    },
    /// The author edited a message. Applied only if `edited_at` is newer
    /// than what the receiver has (last-writer-wins); an edit for a message
    /// the receiver doesn't know is dropped — backfill delivers the merged
    /// message later.
    Edit { message_id: Uuid, channel: ChannelId, edited_at: i64, body: String },
    /// The author deleted a message. Sets the monotone tombstone; the row
    /// stays (and replicates) so the delete wins over any resurrecting
    /// backfill.
    Delete { message_id: Uuid, channel: ChannelId, deleted_at: i64 },
    /// One reaction toggled on (`removed: false`) or off (`removed: true`).
    Reaction(ReactionRow),
    /// "I'm composing a message in this channel" — broadcast at most every
    /// few seconds while typing. Purely ephemeral: never stored, expires on
    /// the receiver's clock, and old builds drop the unknown tag silently
    /// (their deserialization already ignores payloads it can't parse).
    Typing { channel: ChannelId, author: String },
    /// "My mic went hot / quiet in this voice channel" — broadcast on
    /// transitions of the sender's local speech detector, suppressed while
    /// they're muted. Ephemeral exactly like [`ChatPayload::Typing`]: never
    /// stored, expires on the receiver's clock (the safety net for a peer
    /// that vanishes mid-word and never sends its `false`).
    Speaking { channel: ChannelId, author: String, speaking: bool },
    /// "This is what I look like": the sender's display name, avatar
    /// (base64 PNG, downscaled by the sender — see
    /// [`crate::profile::encode_avatar`]), and bio. Broadcast on entering
    /// a room (and again whenever the profile is edited), and sent
    /// directly to every peer that joins afterwards, so everyone online
    /// ends up with everyone's card; receivers cache the avatar on disk
    /// (keyed by author) so it also survives restarts. `avatar: None`
    /// means the sender has no photo — receivers drop any cached one, so
    /// removing your photo propagates like setting one does.
    Profile {
        author: String,
        avatar: Option<String>,
        #[serde(default)]
        bio: Option<String>,
    },
    /// "I'm connected to this voice channel" (`present: true`) or "I left
    /// it" (`false`) — what fills the roster under each voice channel.
    /// Broadcast on join/leave, re-broadcast on room reconnect, and sent
    /// directly to peers who enter the room mid-call so late joiners see
    /// who's already talking. Ephemeral like [`ChatPayload::Typing`], but
    /// with a different safety net: receivers key entries by the sender's
    /// *room peer id*, so when a member's app dies without saying `false`,
    /// the relay's `PeerLeft` sweeps their roster row out.
    VoicePresence { channel: ChannelId, author: String, present: bool },
}

/// The author's current unix time in milliseconds — the timestamp stamped
/// on outgoing [`ChatMessage`]s.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
