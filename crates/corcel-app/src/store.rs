//! The local database: every server this user belongs to and every chat
//! message they've ever seen, in one SQLite file next to `profile.json`.
//! SQLite is compiled in (rusqlite's `bundled` feature), so there is nothing
//! for the user to install or configure — the "server" each user carries is
//! just this file.
//!
//! All access goes through the GPUI main thread ([`crate::Shell`] owns the
//! one [`Store`]); background tasks that need the database route through
//! entity updates rather than sharing the connection across threads.

use std::path::PathBuf;

use anyhow::Context as _;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use corcel_signal::{ChannelId, RelayIdentity};

use crate::chat::ChatMessage;
use crate::invite::ServerLink;

/// A server this user belongs to, as persisted. `identity` is present only
/// for servers this user hosts — it's the TLS cert/key the relay must be
/// respawned with for old invite links to keep verifying (see
/// [`RelayIdentity`]).
#[derive(Clone)]
pub struct SavedServer {
    pub link: ServerLink,
    pub is_host: bool,
    pub identity: Option<RelayIdentity>,
}

pub struct Store {
    conn: Connection,
}

/// The schema, as an append-only list of migrations. `PRAGMA user_version`
/// records how many have run; opening walks the tail. Rules: **never edit a
/// shipped migration** (databases that already ran it will never see the
/// edit) — append a new one. Migration 1 uses `IF NOT EXISTS` because it
/// adopts databases created before versioning existed.
const MIGRATIONS: &[&str] = &[
    // 1: the original schema — servers this user belongs to + all messages.
    "CREATE TABLE IF NOT EXISTS servers (
        id        TEXT PRIMARY KEY,
        link      TEXT NOT NULL,
        is_host   INTEGER NOT NULL,
        cert_der  BLOB,
        key_der   BLOB,
        position  INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS messages (
        id         TEXT PRIMARY KEY,
        server_id  TEXT NOT NULL,
        channel_id TEXT NOT NULL,
        author     TEXT NOT NULL,
        sent_at    INTEGER NOT NULL,
        body       TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS messages_by_channel
        ON messages (channel_id, sent_at);
    CREATE INDEX IF NOT EXISTS messages_by_server_time
        ON messages (server_id, sent_at);",
    // 2: unread tracking — the newest sent_at this user has *seen* (not
    // merely stored) per channel. Purely local, never replicated.
    "CREATE TABLE last_read (
        channel_id TEXT PRIMARY KEY,
        last_read  INTEGER NOT NULL
    );",
    // 3: replicated message features — replies, edits, delete tombstones,
    // and reactions. Ids are TEXT like every other id column here.
    "ALTER TABLE messages ADD COLUMN reply_to TEXT;
    ALTER TABLE messages ADD COLUMN edited_at INTEGER;
    ALTER TABLE messages ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0;
    CREATE TABLE reactions (
        message_id TEXT NOT NULL,
        emoji      TEXT NOT NULL,
        author     TEXT NOT NULL,
        at         INTEGER NOT NULL,
        removed    INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (message_id, emoji, author)
    );
    CREATE INDEX reactions_by_time ON reactions (at);",
];

impl Store {
    /// Opens (creating if needed) the app database and brings the schema up
    /// to date by running whatever tail of [`MIGRATIONS`] this database
    /// hasn't seen, each in its own transaction.
    pub fn open() -> anyhow::Result<Self> {
        let path = db_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(&path)
            .with_context(|| format!("couldn't open database at {}", path.display()))?;

        let version: usize =
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as usize;
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = conn.transaction()?;
            tx.execute_batch(migration)
                .with_context(|| format!("schema migration {} failed", index + 1))?;
            tx.pragma_update(None, "user_version", (index + 1) as i64)?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    /// Every saved server in rail order.
    pub fn servers(&self) -> anyhow::Result<Vec<SavedServer>> {
        let mut stmt = self
            .conn
            .prepare("SELECT link, is_host, cert_der, key_der FROM servers ORDER BY position, rowid")?;
        let rows = stmt.query_map([], |row| {
            let link: String = row.get(0)?;
            let is_host: bool = row.get(1)?;
            let cert_der: Option<Vec<u8>> = row.get(2)?;
            let key_der: Option<Vec<u8>> = row.get(3)?;
            Ok((link, is_host, cert_der, key_der))
        })?;

        let mut servers = Vec::new();
        for row in rows {
            let (link, is_host, cert_der, key_der) = row?;
            // A row whose link no longer decodes (e.g. written by a newer
            // build) is skipped rather than wedging the whole app at launch.
            let Ok(link) = ServerLink::decode(&link) else { continue };
            let identity = match (cert_der, key_der) {
                (Some(cert_der), Some(key_der)) => Some(RelayIdentity { cert_der, key_der }),
                _ => None,
            };
            servers.push(SavedServer { link, is_host, identity });
        }
        Ok(servers)
    }

    /// Inserts or updates a server row (keyed by `link.id`), appending it to
    /// the end of the rail if it's new.
    pub fn save_server(&self, server: &SavedServer) -> anyhow::Result<()> {
        let (cert_der, key_der) = match &server.identity {
            Some(identity) => (Some(identity.cert_der.clone()), Some(identity.key_der.clone())),
            None => (None, None),
        };
        self.conn.execute(
            "INSERT INTO servers (id, link, is_host, cert_der, key_der, position)
             VALUES (?1, ?2, ?3, ?4, ?5,
                     COALESCE((SELECT MAX(position) + 1 FROM servers), 0))
             ON CONFLICT(id) DO UPDATE SET
                 link = excluded.link,
                 is_host = excluded.is_host,
                 cert_der = excluded.cert_der,
                 key_der = excluded.key_der",
            params![
                server.link.id.to_string(),
                server.link.encode(),
                server.is_host,
                cert_der,
                key_der,
            ],
        )?;
        Ok(())
    }

    /// Rewrites just a server's link — used after a rehost lands on a new
    /// address/port so "Copy invite link" always hands out a reachable one.
    pub fn update_link(&self, link: &ServerLink) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE servers SET link = ?2 WHERE id = ?1",
            params![link.id.to_string(), link.encode()],
        )?;
        Ok(())
    }

    /// Removes a server and all of its messages — leaving a server forgets
    /// its history too, matching what "leave" means everywhere else.
    pub fn remove_server(&self, id: Uuid) -> anyhow::Result<()> {
        self.conn.execute("DELETE FROM messages WHERE server_id = ?1", params![id.to_string()])?;
        self.conn.execute("DELETE FROM servers WHERE id = ?1", params![id.to_string()])?;
        Ok(())
    }

    /// Stores or merges a message, returning whether anything changed.
    /// New ids insert; a known id merges by last-writer-wins on `edited_at`
    /// (a newer edit replaces the body) and monotonically on `deleted` (a
    /// tombstone never un-deletes). Re-receiving identical history is a
    /// no-op, so replication stays idempotent.
    pub fn insert_message(&self, server_id: Uuid, message: &ChatMessage) -> anyhow::Result<bool> {
        let changed = self.conn.execute(
            "INSERT INTO messages (id, server_id, channel_id, author, sent_at, body,
                                   reply_to, edited_at, deleted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                 body      = excluded.body,
                 edited_at = excluded.edited_at,
                 deleted   = MAX(messages.deleted, excluded.deleted)
             WHERE COALESCE(excluded.edited_at, 0) > COALESCE(messages.edited_at, 0)
                OR excluded.deleted > messages.deleted",
            params![
                message.id.to_string(),
                server_id.to_string(),
                message.channel.to_string(),
                message.author,
                message.sent_at,
                message.body,
                message.reply_to.map(|id| id.to_string()),
                message.edited_at,
                message.deleted,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Applies an edit if it's newer than what we have (last-writer-wins on
    /// `edited_at`). Returns whether the row changed; an edit for an unknown
    /// id changes nothing — backfill delivers the already-merged message.
    pub fn apply_edit(&self, message_id: Uuid, body: &str, edited_at: i64) -> anyhow::Result<bool> {
        let changed = self.conn.execute(
            "UPDATE messages SET body = ?2, edited_at = ?3
             WHERE id = ?1 AND ?3 > COALESCE(edited_at, 0)",
            params![message_id.to_string(), body, edited_at],
        )?;
        Ok(changed > 0)
    }

    /// Sets a message's delete tombstone. Monotone: already-deleted rows
    /// don't change (returns false), and nothing ever clears the flag.
    pub fn apply_delete(&self, message_id: Uuid) -> anyhow::Result<bool> {
        let changed = self.conn.execute(
            "UPDATE messages SET deleted = 1 WHERE id = ?1 AND deleted = 0",
            params![message_id.to_string()],
        )?;
        Ok(changed > 0)
    }

    /// Merges one reaction state, newest `at` wins per `(message, emoji,
    /// author)` key — so react/un-react/re-react converge no matter the
    /// arrival order. Returns whether the stored state changed.
    pub fn upsert_reaction(&self, reaction: &crate::chat::ReactionRow) -> anyhow::Result<bool> {
        let changed = self.conn.execute(
            "INSERT INTO reactions (message_id, emoji, author, at, removed)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(message_id, emoji, author) DO UPDATE SET
                 at = excluded.at,
                 removed = excluded.removed
             WHERE excluded.at > reactions.at",
            params![
                reaction.message_id.to_string(),
                reaction.emoji,
                reaction.author,
                reaction.at,
                reaction.removed,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Every live (non-removed) reaction on a channel's messages, oldest
    /// first — the order chips appear in under a message.
    pub fn reactions_for(&self, channel: ChannelId) -> anyhow::Result<Vec<(Uuid, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.message_id, r.emoji, r.author
             FROM reactions r JOIN messages m ON m.id = r.message_id
             WHERE m.channel_id = ?1 AND r.removed = 0
             ORDER BY r.at, r.emoji",
        )?;
        let rows = stmt.query_map(params![channel.to_string()], |row| {
            let message_id: String = row.get(0)?;
            Ok((message_id, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        Ok(rows
            .filter_map(|row| row.ok())
            .filter_map(|(id, emoji, author)| Some((id.parse().ok()?, emoji, author)))
            .collect())
    }

    /// Every reaction change (removals included — they're tombstones that
    /// must replicate) on a server's messages newer than `since`, for
    /// answering a history request.
    pub fn reactions_since(&self, server_id: Uuid, since: i64) -> anyhow::Result<Vec<crate::chat::ReactionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.message_id, m.channel_id, r.emoji, r.author, r.at, r.removed
             FROM reactions r JOIN messages m ON m.id = r.message_id
             WHERE m.server_id = ?1 AND r.at > ?2
             ORDER BY r.at",
        )?;
        let rows = stmt.query_map(params![server_id.to_string(), since], |row| {
            let message_id: String = row.get(0)?;
            let channel: String = row.get(1)?;
            Ok((message_id, channel, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, i64>(4)?, row.get::<_, bool>(5)?))
        })?;
        Ok(rows
            .filter_map(|row| row.ok())
            .filter_map(|(message_id, channel, emoji, author, at, removed)| {
                Some(crate::chat::ReactionRow {
                    message_id: message_id.parse().ok()?,
                    channel: channel.parse().ok()?,
                    emoji,
                    author,
                    at,
                    removed,
                })
            })
            .collect())
    }

    /// A channel's messages in display order.
    pub fn messages(&self, channel: ChannelId) -> anyhow::Result<Vec<ChatMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_id, author, sent_at, body, reply_to, edited_at, deleted
             FROM messages WHERE channel_id = ?1 ORDER BY sent_at, id",
        )?;
        let rows = stmt.query_map(params![channel.to_string()], row_to_message)?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    /// Every message of a server newer than `since` — what a peer sends back
    /// for a [`crate::chat::ChatPayload::HistoryRequest`].
    pub fn messages_since(&self, server_id: Uuid, since: i64) -> anyhow::Result<Vec<ChatMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_id, author, sent_at, body, reply_to, edited_at, deleted
             FROM messages WHERE server_id = ?1 AND sent_at > ?2 ORDER BY sent_at, id",
        )?;
        let rows = stmt.query_map(params![server_id.to_string(), since], row_to_message)?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    /// Marks a channel read up to `ts` — everything at or before it stops
    /// counting as unread.
    pub fn set_last_read(&self, channel: ChannelId, ts: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO last_read (channel_id, last_read) VALUES (?1, ?2)
             ON CONFLICT(channel_id) DO UPDATE SET last_read = excluded.last_read",
            params![channel.to_string(), ts],
        )?;
        Ok(())
    }

    /// Per-channel `(unread, mentions)` counts for a server — messages newer
    /// than the channel's `last_read`, excluding this user's own. The
    /// mention test is a coarse `LIKE '%@name%'` (can over-count `@namely`);
    /// the precise per-message check lives in [`crate::richtext`], which is
    /// what actually highlights rows. Channels with nothing unread aren't
    /// returned.
    pub fn unread_counts(&self, server_id: Uuid, my_name: &str) -> anyhow::Result<Vec<(ChannelId, u32, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.channel_id,
                    COUNT(*),
                    SUM(CASE WHEN m.body LIKE '%@' || ?2 || '%' THEN 1 ELSE 0 END)
             FROM messages m
             LEFT JOIN last_read r ON r.channel_id = m.channel_id
             WHERE m.server_id = ?1
               AND m.author <> ?2
               AND m.sent_at > COALESCE(r.last_read, 0)
             GROUP BY m.channel_id",
        )?;
        let rows = stmt.query_map(params![server_id.to_string(), my_name], |row| {
            let channel: String = row.get(0)?;
            let unread: u32 = row.get(1)?;
            let mentions: u32 = row.get(2)?;
            Ok((channel, unread, mentions))
        })?;
        Ok(rows
            .filter_map(|row| row.ok())
            .filter_map(|(channel, unread, mentions)| Some((channel.parse().ok()?, unread, mentions)))
            .collect())
    }

    /// The newest `sent_at` this user has for a server, or `None` when they
    /// have no history at all — the basis of the backfill `since` window.
    pub fn latest_timestamp(&self, server_id: Uuid) -> anyhow::Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT MAX(sent_at) FROM messages WHERE server_id = ?1",
                params![server_id.to_string()],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten())
    }
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatMessage> {
    let id: String = row.get(0)?;
    let channel: String = row.get(1)?;
    let reply_to: Option<String> = row.get(5)?;
    Ok(ChatMessage {
        id: id.parse().unwrap_or_else(|_| Uuid::nil()),
        channel: channel.parse().unwrap_or_else(|_| Uuid::nil()),
        author: row.get(2)?,
        sent_at: row.get(3)?,
        body: row.get(4)?,
        reply_to: reply_to.and_then(|id| id.parse().ok()),
        edited_at: row.get(6)?,
        deleted: row.get(7)?,
    })
}

/// `$XDG_CONFIG_HOME/corcel/corcel.db` — deliberately the same directory
/// as `profile.json` (see [`crate::profile::config_dir`]) so "everything
/// corcel knows" lives in one place the user can find, back up, or delete.
fn db_path() -> PathBuf {
    crate::profile::config_dir().join("corcel.db")
}
