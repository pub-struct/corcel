//! A server's invite link: everything a joining client needs to reach the
//! host's signaling relay and see its channel list, with no separate
//! discovery step (PROJECT.md decision 1 — the relay itself is unknown to
//! anyone but the people who already have the link).

use std::net::SocketAddr;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use corcel_signal::{ChannelId, EndpointAddr, EndpointId, Reach, TransportAddr};
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
    /// How the relay is reachable ([`Reach::Global`] via iroh's public
    /// infrastructure, or [`Reach::LocalNetwork`] over LAN/VPN routes
    /// only). Defaulted so every link minted before the choice existed —
    /// all of which were global — still decodes as what it was.
    #[serde(default, skip_serializing_if = "reach_is_global")]
    pub reach: Reach,
    /// For [`Reach::LocalNetwork`] servers: the host's direct socket
    /// addresses (LAN and/or VPN IPs) at the moment the link was minted.
    /// With no public discovery to resolve the endpoint id, these are the
    /// only routes — which is why local links go stale if the host's
    /// address changes, and why the host regenerates them on every launch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<SocketAddr>,
    pub channels: Vec<ChannelInfo>,
}

fn reach_is_global(reach: &Reach) -> bool {
    *reach == Reach::Global
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

    /// Everything needed to dial the relay: the endpoint id, plus — for
    /// [`Reach::LocalNetwork`] servers — the direct addresses the link
    /// carries (there is no discovery to resolve the id alone).
    pub fn endpoint_addr(&self) -> anyhow::Result<EndpointAddr> {
        let addr = EndpointAddr::from(self.endpoint_id()?)
            .with_addrs(self.addrs.iter().map(|addr| TransportAddr::Ip(*addr)));
        Ok(addr)
    }

    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("ServerLink always serializes");
        format!("corcel1{}", URL_SAFE_NO_PAD.encode(json))
    }

    #[cfg(test)]
    fn roundtrip(&self) -> Self {
        Self::decode(&self.encode()).expect("a just-encoded link always decodes")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn base_link() -> ServerLink {
        ServerLink {
            id: Uuid::new_v4(),
            name: "test".to_string(),
            node: None,
            reach: Reach::Global,
            addrs: Vec::new(),
            channels: Vec::new(),
        }
    }

    #[test]
    fn local_network_links_carry_reach_and_addrs() {
        let link = ServerLink {
            reach: Reach::LocalNetwork,
            addrs: vec!["100.64.1.2:4242".parse().unwrap(), "192.168.0.7:4242".parse().unwrap()],
            ..base_link()
        };
        let decoded = link.roundtrip();
        assert_eq!(decoded.reach, Reach::LocalNetwork);
        assert_eq!(decoded.addrs, link.addrs);
    }

    #[test]
    fn global_links_stay_free_of_local_fields() {
        let encoded = base_link().encode();
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded.strip_prefix("corcel1").unwrap())
            .unwrap();
        let json = String::from_utf8(json).unwrap();
        // Wire-compat both ways: today's global links look exactly like the
        // ones minted before reach existed.
        assert!(!json.contains("reach"));
        assert!(!json.contains("addrs"));
    }

    #[test]
    fn links_minted_before_reach_existed_decode_as_global() {
        let stripped = serde_json::json!({
            "id": Uuid::new_v4(),
            "name": "old",
            "channels": [],
        });
        let encoded = format!(
            "corcel1{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&stripped).unwrap())
        );
        let decoded = ServerLink::decode(&encoded).unwrap();
        assert_eq!(decoded.reach, Reach::Global);
        assert!(decoded.addrs.is_empty());
    }
}
