//! The host side of a server's signaling relay, served over iroh.
//!
//! When a peer starts hosting a server, it spawns one of these locally. It
//! knows nothing about SDP/ICE semantics — it just routes opaque
//! [`SignalPayload`](crate::protocol::SignalPayload)s between the host and
//! whichever participants are currently signaling for a given channel, and
//! gets out of the way once each pair has what it needs to connect directly.
//! It also hosts *rooms* (see [`ClientMessage::Room`]): hostless membership
//! sets used as the chat mesh, where it broadcasts/routes opaque JSON
//! payloads with the same "never inspect, only deliver" contract.
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

/// The ALPN this relay accepts. Bump the trailing number on any wire-format
/// break — iroh refuses mismatched ALPNs at handshake, which beats a JSON
/// parse error deep into a session.
pub const ALPN: &[u8] = b"corcel/signal/1";

pub struct Relay {
    /// What invite links carry — the stable, publicly-dialable identity.
    pub endpoint_id: EndpointId,
    /// The full local address (direct socket addrs included), registered
    /// with [`crate::client`] so the host's own connections to this relay
    /// go straight over loopback instead of waiting for discovery to find
    /// their own machine.
    pub addr: EndpointAddr,
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

#[derive(Default)]
struct ChannelState {
    host: Option<(PeerId, mpsc::UnboundedSender<ServerMessage>)>,
    participants: HashMap<PeerId, mpsc::UnboundedSender<ServerMessage>>,
    watchers: HashMap<PeerId, mpsc::UnboundedSender<ServerMessage>>,
}

impl ChannelState {
    fn sender_for(&self, peer: PeerId) -> Option<mpsc::UnboundedSender<ServerMessage>> {
        if let Some((host_id, tx)) = &self.host {
            if *host_id == peer {
                return Some(tx.clone());
            }
        }
        self.participants.get(&peer).cloned()
    }

    fn broadcast_except(&self, except: PeerId, msg: &ServerMessage) {
        if let Some((host_id, tx)) = &self.host {
            if *host_id != except {
                let _ = tx.send(msg.clone());
            }
        }
        for (id, tx) in &self.participants {
            if *id != except {
                let _ = tx.send(msg.clone());
            }
        }
    }

    fn occupant_count(&self) -> usize {
        self.host.is_some() as usize + self.participants.len()
    }

    fn notify_watchers(&self) {
        let count = self.occupant_count();
        for tx in self.watchers.values() {
            let _ = tx.send(ServerMessage::Presence { count });
        }
    }
}

enum Role {
    Host,
    Participant,
    Watcher,
    /// A room member (see [`ClientMessage::Room`]) — lives in `Rooms`, not
    /// `Channels`, and speaks `Publish`/`Direct` instead of `Relay`.
    Member,
}

type Channels = Arc<Mutex<HashMap<ChannelId, ChannelState>>>;

/// Room membership: no host slot, no watchers — just who's here. Keyed by
/// the same `ChannelId` type as channels but in a separate namespace (the
/// app uses the *server's* id as its room key).
type Rooms = Arc<Mutex<HashMap<ChannelId, HashMap<PeerId, mpsc::UnboundedSender<ServerMessage>>>>>;

/// Binds an iroh endpoint on the given persisted identity and starts
/// accepting connections in the background. The task runs for the lifetime
/// of the process — there's no shutdown handle yet, which is fine for a
/// relay whose whole reason to exist is "as long as the host's app is
/// open."
pub async fn spawn(identity: &RelayIdentity) -> anyhow::Result<Relay> {
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(SecretKey::from_bytes(&identity.secret))
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    let endpoint_id = endpoint.id();
    let addr = endpoint.addr();
    crate::client::register_local_relay(addr.clone());

    let channels: Channels = Arc::new(Mutex::new(HashMap::new()));
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));

    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let channels = channels.clone();
            let rooms = rooms.clone();
            tokio::spawn(async move {
                let result = async {
                    let conn = incoming.accept()?.await?;
                    anyhow::Ok(conn)
                }
                .await;
                match result {
                    Ok(conn) => {
                        if let Err(err) = handle_connection(conn, channels, rooms).await {
                            tracing_stub_log(&err);
                        }
                    }
                    // Handshake failures are background noise on an open
                    // QUIC socket (stray datagrams, wrong ALPN) — log and
                    // move on, same as the doc on `Incoming::accept` says.
                    Err(err) => tracing_stub_log(&err),
                }
            });
        }
    });

    Ok(Relay { endpoint_id, addr })
}

// Small placeholder so we're not silently swallowing connection errors
// before a real logging story exists.
fn tracing_stub_log(err: &anyhow::Error) {
    eprintln!("corcel-signal: connection error: {err:#}");
}

async fn handle_connection(
    conn: iroh::endpoint::Connection,
    channels: Channels,
    rooms: Rooms,
) -> anyhow::Result<()> {
    // One bi-directional stream per connection, newline-delimited JSON both
    // ways — the exact message types the WebSocket transport carried, minus
    // the WebSocket.
    let (mut writer, reader) = conn.accept_bi().await?;
    let mut lines = BufReader::new(reader).lines();

    let first = match lines.next_line().await {
        Ok(Some(line)) => line,
        _ => return Ok(()), // client vanished before saying hello
    };
    let (channel_id, peer_id, role) = match serde_json::from_str::<ClientMessage>(&first)? {
        ClientMessage::Host { channel } => (channel, Uuid::new_v4(), Role::Host),
        ClientMessage::Join { channel } => (channel, Uuid::new_v4(), Role::Participant),
        ClientMessage::Watch { channel } => (channel, Uuid::new_v4(), Role::Watcher),
        ClientMessage::Room { channel } => (channel, Uuid::new_v4(), Role::Member),
        ClientMessage::Relay { .. } | ClientMessage::Publish { .. } | ClientMessage::Direct { .. } => {
            let _ = send(&mut writer, &ServerMessage::Error {
                message: "expected Host, Join, Watch, or Room as the first message".into(),
            })
            .await;
            return Ok(());
        }
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    if matches!(role, Role::Member) {
        let mut rooms = rooms.lock().unwrap();
        let room = rooms.entry(channel_id).or_default();
        let peers: Vec<PeerId> = room.keys().copied().collect();
        for other in room.values() {
            let _ = other.send(ServerMessage::PeerJoined { peer: peer_id });
        }
        room.insert(peer_id, tx.clone());
        let _ = tx.send(ServerMessage::RoomWelcome { your_peer: peer_id, peers });
    } else {
        let mut channels = channels.lock().unwrap();
        let state = channels.entry(channel_id).or_default();
        match role {
            Role::Host => {
                if state.host.is_some() {
                    let _ = tx.send(ServerMessage::Error {
                        message: "channel already has a host".into(),
                    });
                } else {
                    state.host = Some((peer_id, tx.clone()));
                    state.notify_watchers();
                }
            }
            Role::Participant => {
                if state.host.is_none() {
                    let _ = tx.send(ServerMessage::Error {
                        message: "channel has no host yet".into(),
                    });
                } else {
                    state.participants.insert(peer_id, tx.clone());
                    state.broadcast_except(peer_id, &ServerMessage::PeerJoined { peer: peer_id });
                    state.notify_watchers();
                }
            }
            Role::Watcher => {
                state.watchers.insert(peer_id, tx.clone());
            }
            Role::Member => unreachable!("members are registered in `rooms` above"),
        }
        let host_id = state.host.as_ref().map(|(id, _)| *id);
        let _ = tx.send(ServerMessage::Welcome {
            your_peer: peer_id,
            host: host_id,
        });
        if matches!(role, Role::Watcher) {
            let _ = tx.send(ServerMessage::Presence {
                count: state.occupant_count(),
            });
        }
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
        match (&role, msg) {
            (Role::Host | Role::Participant, ClientMessage::Relay { to, payload }) => {
                let target = channels.lock().unwrap().get(&channel_id).and_then(|s| s.sender_for(to));
                if let Some(target) = target {
                    let _ = target.send(ServerMessage::Relay { from: peer_id, payload });
                }
            }
            (Role::Member, ClientMessage::Publish { payload }) => {
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
            (Role::Member, ClientMessage::Direct { to, payload }) => {
                let target = rooms.lock().unwrap().get(&channel_id).and_then(|room| room.get(&to).cloned());
                if let Some(target) = target {
                    let _ = target.send(ServerMessage::Direct { from: peer_id, payload });
                }
            }
            _ => {} // a message this connection's role doesn't get to send
        }
    }

    // Connection closed: tear down this peer's membership and let the rest
    // of the channel/room know. For channels this is derived from actual
    // membership (not the `is_host` this connection *asked* for above) — a
    // connection whose Host claim was rejected because the channel already
    // had one never became host or a participant, and clearing `state.host`
    // based on its stale intent would evict the real host from under it,
    // breaking the channel for everyone.
    if matches!(role, Role::Member) {
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
    } else {
        let mut channels = channels.lock().unwrap();
        if let Some(state) = channels.get_mut(&channel_id) {
            let was_host = state.host.as_ref().is_some_and(|(id, _)| *id == peer_id);
            let was_participant = if was_host {
                false
            } else {
                state.participants.remove(&peer_id).is_some()
            };
            if was_host {
                state.host = None;
            }
            state.watchers.remove(&peer_id);
            if was_host || was_participant {
                state.broadcast_except(peer_id, &ServerMessage::PeerLeft { peer: peer_id });
                state.notify_watchers();
            }
            if state.host.is_none() && state.participants.is_empty() && state.watchers.is_empty() {
                channels.remove(&channel_id);
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
