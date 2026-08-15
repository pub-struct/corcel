//! The host side of a server's relay, served over iroh.
//!
//! When a peer starts hosting a server, it spawns one of these locally. It
//! serves two ALPNs on one endpoint (one identity, one invite link):
//!
//! - [`ALPN`]: *rooms* (see [`ClientMessage::Room`]) — hostless membership
//!   sets used as the chat mesh, where it broadcasts/routes opaque JSON
//!   payloads with a "never inspect, only deliver" contract.
//! - [`MEDIA_ALPN`]: call media. The relay doesn't handle these connections
//!   itself — they're handed whole to the caller via [`Relay::media`], and
//!   `corcel-net`'s host relay forwards RTP between them.
//!
//! Transport is an [`iroh`] endpoint rather than a plain socket, which is
//! what makes invite links work across the open internet with zero user
//! configuration: joining clients dial the host's [`EndpointId`] (a public
//! key carried in the invite link), iroh's public relay/DNS infrastructure
//! handles the rendezvous, and the connection hole-punches to a direct
//! QUIC path where the NATs allow it (falling back to relayed traffic where
//! they don't). The endpoint id doubles as the trust anchor — the QUIC
//! handshake proves the host holds the matching secret key, replacing the
//! old self-signed-certificate fingerprint pinning outright. The secret key
//! is the [`RelayIdentity`] the caller persists and hands back on every
//! spawn — regenerating it per process would silently invalidate every
//! invite link ever shared for the server.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::protocol::{ChannelId, ClientMessage, PeerId, ServerMessage};

/// The ALPN for room (chat/presence) connections. Bump the trailing number
/// on any wire-format break — iroh refuses mismatched ALPNs at handshake,
/// which beats a JSON parse error deep into a session.
pub const ALPN: &[u8] = b"corcel/signal/1";

/// The ALPN for call-media connections (RTP over QUIC datagrams). The relay
/// only routes these to [`Relay::media`]; the wire protocol on them belongs
/// to `corcel-net`.
pub const MEDIA_ALPN: &[u8] = b"corcel/media/1";

pub struct Relay {
    /// What invite links carry — the stable, publicly-dialable identity.
    pub endpoint_id: EndpointId,
    /// The full local address (direct socket addrs included), registered
    /// with [`crate::client`] so the host's own connections to this relay
    /// go straight over loopback instead of waiting for discovery to find
    /// their own machine.
    pub addr: EndpointAddr,
    /// Every accepted [`MEDIA_ALPN`] connection, in arrival order. The
    /// caller is expected to feed these to `corcel-net`'s host relay;
    /// if the receiver is dropped they're just closed on arrival.
    pub media: mpsc::UnboundedReceiver<iroh::endpoint::Connection>,
}

/// The relay's identity: an iroh secret key, kept as raw bytes so it
/// round-trips through storage as a plain blob. This is what makes a hosted
/// server *the same server* across app restarts — invite links carry the
/// public [`EndpointId`], so as long as the caller persists this and spawns
/// with it again, old links keep dialing (and keep authenticating: the QUIC
/// handshake proves possession of this key).
#[derive(Clone)]
pub struct RelayIdentity {
    secret: [u8; 32],
}

impl RelayIdentity {
    pub fn generate() -> anyhow::Result<Self> {
        Ok(Self { secret: SecretKey::generate().to_bytes() })
    }

    /// Rebuilds a persisted identity. `None` for blobs that aren't an iroh
    /// secret key — e.g. rows written by the pre-iroh TLS transport.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let secret: [u8; 32] = bytes.try_into().ok()?;
        Some(Self { secret })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.secret.to_vec()
    }

    /// The value invite links carry (and joining clients authenticate).
    pub fn endpoint_id(&self) -> EndpointId {
        SecretKey::from_bytes(&self.secret).public()
    }
}

/// Room membership: who's here, and how to reach them. Keyed by the room's
/// id (the app uses the *server's* id as its room key).
type Rooms = Arc<Mutex<HashMap<ChannelId, HashMap<PeerId, mpsc::UnboundedSender<ServerMessage>>>>>;

/// Binds an iroh endpoint on the given persisted identity and starts
/// accepting connections in the background. The task runs for the lifetime
/// of the process — there's no shutdown handle yet, which is fine for a
/// relay whose whole reason to exist is "as long as the host's app is
/// open."
pub async fn spawn(identity: &RelayIdentity) -> anyhow::Result<Relay> {
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(SecretKey::from_bytes(&identity.secret))
        .alpns(vec![ALPN.to_vec(), MEDIA_ALPN.to_vec()])
        .bind()
        .await?;
    let endpoint_id = endpoint.id();
    let addr = endpoint.addr();
    crate::client::register_local_relay(addr.clone());

    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
    let (media_tx, media_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let rooms = rooms.clone();
            let media_tx = media_tx.clone();
            tokio::spawn(async move {
                let result = async {
                    let conn = incoming.accept()?.await?;
                    anyhow::Ok(conn)
                }
                .await;
                match result {
                    Ok(conn) if conn.alpn() == MEDIA_ALPN => {
                        // Media connections are corcel-net's to drive; an
                        // unconsumed one just drops (and thereby closes).
                        let _ = media_tx.send(conn);
                    }
                    Ok(conn) => {
                        if let Err(err) = handle_connection(conn, rooms).await {
                            log_connection_error(&err);
                        }
                    }
                    // Handshake failures are background noise on an open
                    // QUIC socket (stray datagrams, wrong ALPN) — log and
                    // move on, same as the doc on `Incoming::accept` says.
                    Err(err) => log_connection_error(&err),
                }
            });
        }
    });

    Ok(Relay { endpoint_id, addr, media: media_rx })
}

// Small placeholder so we're not silently swallowing connection errors
// before a real logging story exists.
fn log_connection_error(err: &anyhow::Error) {
    eprintln!("corcel-signal: connection error: {err:#}");
}

async fn handle_connection(conn: iroh::endpoint::Connection, rooms: Rooms) -> anyhow::Result<()> {
    // One bi-directional stream per connection, newline-delimited JSON both
    // ways.
    let (mut writer, reader) = conn.accept_bi().await?;
    let mut lines = BufReader::new(reader).lines();

    let first = match lines.next_line().await {
        Ok(Some(line)) => line,
        _ => return Ok(()), // client vanished before saying hello
    };
    let channel_id = match serde_json::from_str::<ClientMessage>(&first)? {
        ClientMessage::Room { channel } => channel,
        ClientMessage::Publish { .. } | ClientMessage::Direct { .. } => {
            let _ = send(&mut writer, &ServerMessage::Error {
                message: "expected Room as the first message".into(),
            })
            .await;
            return Ok(());
        }
    };
    let peer_id: PeerId = Uuid::new_v4();

    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    {
        let mut rooms = rooms.lock().unwrap();
        let room = rooms.entry(channel_id).or_default();
        let peers: Vec<PeerId> = room.keys().copied().collect();
        for other in room.values() {
            let _ = other.send(ServerMessage::PeerJoined { peer: peer_id });
        }
        room.insert(peer_id, tx.clone());
        let _ = tx.send(ServerMessage::RoomWelcome { your_peer: peer_id, peers });
    }

    // Forward the per-connection outbound queue to the actual stream.
    let forward = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if send(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<ClientMessage>(&line) else { continue };
        match msg {
            ClientMessage::Publish { payload } => {
                let targets: Vec<_> = rooms
                    .lock()
                    .unwrap()
                    .get(&channel_id)
                    .map(|room| {
                        room.iter().filter(|(id, _)| **id != peer_id).map(|(_, tx)| tx.clone()).collect()
                    })
                    .unwrap_or_default();
                for target in targets {
                    let _ = target.send(ServerMessage::Published {
                        from: peer_id,
                        payload: payload.clone(),
                    });
                }
            }
            ClientMessage::Direct { to, payload } => {
                let target = rooms.lock().unwrap().get(&channel_id).and_then(|room| room.get(&to).cloned());
                if let Some(target) = target {
                    let _ = target.send(ServerMessage::Direct { from: peer_id, payload });
                }
            }
            ClientMessage::Room { .. } => {} // already in a room; ignored
        }
    }

    // Connection closed: tear down this peer's membership and let the rest
    // of the room know.
    {
        let mut rooms = rooms.lock().unwrap();
        if let Some(room) = rooms.get_mut(&channel_id) {
            if room.remove(&peer_id).is_some() {
                for other in room.values() {
                    let _ = other.send(ServerMessage::PeerLeft { peer: peer_id });
                }
            }
            if room.is_empty() {
                rooms.remove(&channel_id);
            }
        }
    }
    forward.abort();

    Ok(())
}

async fn send(writer: &mut iroh::endpoint::SendStream, msg: &ServerMessage) -> anyhow::Result<()> {
    let mut text = serde_json::to_string(msg)?;
    // serde_json escapes control characters inside strings, so the message
    // itself can never contain a raw newline — one line, one message.
    text.push('\n');
    writer.write_all(text.as_bytes()).await?;
    Ok(())
}
