//! A server's invite link: everything a joining client needs to reach the
//! host's signaling relay and see its channel list, with no separate
//! discovery step (PROJECT.md decision 1 — the relay itself is unknown to
//! anyone but the people who already have the link).

use std::net::SocketAddr;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use corcel_signal::ChannelId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What kind of channel a [`ChannelInfo`] names. `#[serde(default)]`'d to
/// `Voice` on `ChannelInfo` so links minted before text channels existed
/// still decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    #[default]
    Voice,
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: ChannelId,
    pub name: String,
    #[serde(default)]
    pub kind: ChannelKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerLink {
    /// The server's stable identity — survives address changes and app
    /// restarts (unlike `addr`, which is re-derived per launch, and unlike
    /// the fingerprint, which is an implementation detail of TLS pinning).
    /// Everything persisted locally (the saved-server row, every chat
    /// message) hangs off this id, and it doubles as the relay's chat-room
    /// key so all of a server's members meet in one room.
    #[serde(default = "Uuid::nil")]
    pub id: Uuid,
    pub name: String,
    pub addr: SocketAddr,
    pub fingerprint: [u8; 32],
    pub channels: Vec<ChannelInfo>,
}

impl ServerLink {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("ServerLink always serializes");
        format!("corcel1{}", URL_SAFE_NO_PAD.encode(json))
    }

    pub fn decode(link: &str) -> anyhow::Result<Self> {
        // "freecord1" is the app's pre-rename prefix — links with it are
        // already saved in users' databases and pasted in chats, so decode
        // keeps accepting it forever. Encode only ever emits "corcel1".
        let payload = link
            .strip_prefix("corcel1")
            .or_else(|| link.strip_prefix("freecord1"))
            .context("not a corcel invite link")?;
        let json = URL_SAFE_NO_PAD.decode(payload).context("invite link is not valid base64")?;
        serde_json::from_slice(&json).context("invite link is malformed")
    }
}
