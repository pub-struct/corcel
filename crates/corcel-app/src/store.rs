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

    /// Stores a message if it isn't already known. Returns whether it was
    /// new — replication dedupes on this (`INSERT OR IGNORE` keyed on the
    /// message's UUID), so re-receiving history is harmless.
    pub fn insert_message(&self, server_id: Uuid, message: &ChatMessage) -> anyhow::Result<bool> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO messages (id, server_id, channel_id, author, sent_at, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                message.id.to_string(),
                server_id.to_string(),
                message.channel.to_string(),
                message.author,
                message.sent_at,
                message.body,
            ],
        )?;
        Ok(inserted > 0)
    }

    /// A channel's messages in display order.
    pub fn messages(&self, channel: ChannelId) -> anyhow::Result<Vec<ChatMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_id, author, sent_at, body FROM messages
             WHERE channel_id = ?1 ORDER BY sent_at, id",
        )?;
        let rows = stmt.query_map(params![channel.to_string()], row_to_message)?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    /// Every message of a server newer than `since` — what a peer sends back
    /// for a [`crate::chat::ChatPayload::HistoryRequest`].
    pub fn messages_since(&self, server_id: Uuid, since: i64) -> anyhow::Result<Vec<ChatMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_id, author, sent_at, body FROM messages
             WHERE server_id = ?1 AND sent_at > ?2 ORDER BY sent_at, id",
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
    Ok(ChatMessage {
        id: id.parse().unwrap_or_else(|_| Uuid::nil()),
        channel: channel.parse().unwrap_or_else(|_| Uuid::nil()),
        author: row.get(2)?,
        sent_at: row.get(3)?,
        body: row.get(4)?,
    })
}

/// `$XDG_CONFIG_HOME/corcel/corcel.db` — deliberately the same directory
/// as `profile.json` (see [`crate::profile::config_dir`]) so "everything
/// corcel knows" lives in one place the user can find, back up, or delete.
fn db_path() -> PathBuf {
    crate::profile::config_dir().join("corcel.db")
}
