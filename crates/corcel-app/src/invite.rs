//! A server's invite link: everything a joining client needs to reach the
//! host's signaling relay and see its channel list, with no separate
//! discovery step (PROJECT.md decision 1 — the relay itself is unknown to
//! anyone but the people who already have the link).

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use corcel_signal::{ChannelId, EndpointId};
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
    /// The server's stable identity — everything persisted locally (the
    /// saved-server row, every chat message) hangs off this id, and it
    /// doubles as the relay's chat-room key so all of a server's members
    /// meet in one room.
    #[serde(default = "Uuid::nil")]
    pub id: Uuid,
    pub name: String,
    /// The relay's iroh endpoint id (z-base-32 public key) — the whole
    /// "address": stable across restarts, IP changes, and networks, since
    /// iroh's discovery infrastructure resolves it to wherever the host
    /// currently is. It's also the trust anchor (the QUIC handshake proves
    /// the host holds the matching key). `Option` only so links minted by
    /// the pre-iroh transport still *decode* — they can't be dialed
    /// anymore, see [`ServerLink::endpoint_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub channels: Vec<ChannelInfo>,
}

impl ServerLink {
    /// The dialable relay identity, parsed out of [`Self::node`]. Errors on
    /// links from the old TCP transport (which carried a LAN `addr` +
    /// certificate fingerprint instead) — those addresses were never
    /// reachable beyond the host's network anyway, so the honest answer is
    /// a fresh link, not a doomed dial.
    pub fn endpoint_id(&self) -> anyhow::Result<EndpointId> {
        let node = self.node.as_deref().context(
            "this invite link predates corcel's P2P transport — ask the host for a fresh link",
        )?;
        node.parse().context("invite link carries an invalid endpoint id")
    }

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
