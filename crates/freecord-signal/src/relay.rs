//! The host side of a server's wss:// signaling relay.
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
//! TLS uses a self-signed certificate whose fingerprint travels inside the
//! invite link so joining clients can pin against it (see [`crate::client`])
//! instead of trusting a CA. The certificate is a [`RelayIdentity`] the
//! caller persists and hands back on every spawn — regenerating it per
//! process would silently invalidate every invite link ever shared for the
//! server, since the pinned fingerprint would no longer match.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::protocol::{ChannelId, ClientMessage, PeerId, ServerMessage};

pub struct Relay {
    pub port: u16,
    pub fingerprint: [u8; 32],
}

/// The relay's TLS identity: a self-signed certificate plus its private key,
/// both DER-encoded so they round-trip through storage as plain blobs. This
/// is what makes a hosted server *the same server* across app restarts —
/// invite links pin the certificate's fingerprint, so as long as the caller
/// persists this and spawns with it again, old links keep verifying.
#[derive(Clone)]
pub struct RelayIdentity {
    pub cert_der: Vec<u8>,
    /// PKCS#8 private key DER.
    pub key_der: Vec<u8>,
}

impl RelayIdentity {
    pub fn generate() -> anyhow::Result<Self> {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["freecord-host".to_string()])?;
        Ok(Self {
            cert_der: cert.der().to_vec(),
            key_der: signing_key.serialize_der(),
        })
    }

    /// The value invite links pin against (see [`crate::client::connect`]).
    pub fn fingerprint(&self) -> [u8; 32] {
        Sha256::digest(&self.cert_der).into()
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

/// Binds a local listener with the given persisted TLS identity and starts
/// accepting wss connections in the background. The task runs for the
/// lifetime of the process — there's no shutdown handle yet, which is fine
/// for a relay whose whole reason to exist is "as long as the host's app is
/// open."
pub async fn spawn(bind_addr: SocketAddr, identity: &RelayIdentity) -> anyhow::Result<Relay> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert_der: CertificateDer<'static> = CertificateDer::from(identity.cert_der.clone());
    let fingerprint = identity.fingerprint();
    let key_der: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(identity.key_der.clone()).into();

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind(bind_addr).await?;
    let port = listener.local_addr()?.port();

    let channels: Channels = Arc::new(Mutex::new(HashMap::new()));
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));

    tokio::spawn(async move {
        loop {
            let Ok((stream, _peer_addr)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            let channels = channels.clone();
            let rooms = rooms.clone();
            tokio::spawn(async move {
                if let Err(err) = handle_connection(stream, acceptor, channels, rooms).await {
                    tracing_stub_log(&err);
                }
            });
        }
    });

    Ok(Relay { port, fingerprint })
}

// Small placeholder so we're not silently swallowing connection errors
// before a real logging story exists.
fn tracing_stub_log(err: &anyhow::Error) {
    eprintln!("freecord-signal: connection error: {err:#}");
}

async fn handle_connection(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    channels: Channels,
    rooms: Rooms,
) -> anyhow::Result<()> {
    let tls = acceptor.accept(tcp).await?;
    let ws = tokio_tungstenite::accept_async(tls).await?;
    let (mut outgoing, mut incoming) = ws.split();

    let first = match incoming.next().await {
        Some(Ok(Message::Text(text))) => text,
        _ => return Ok(()), // client vanished before saying hello
    };
    let (channel_id, peer_id, role) = match serde_json::from_str::<ClientMessage>(&first)? {
        ClientMessage::Host { channel } => (channel, Uuid::new_v4(), Role::Host),
        ClientMessage::Join { channel } => (channel, Uuid::new_v4(), Role::Participant),
        ClientMessage::Watch { channel } => (channel, Uuid::new_v4(), Role::Watcher),
        ClientMessage::Room { channel } => (channel, Uuid::new_v4(), Role::Member),
        ClientMessage::Relay { .. } | ClientMessage::Publish { .. } | ClientMessage::Direct { .. } => {
            let _ = send(&mut outgoing, &ServerMessage::Error {
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

    // Forward the per-connection outbound queue to the actual socket.
    let forward = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if send(&mut outgoing, &msg).await.is_err() {
                break;
            }
        }
    });

    while let Some(msg) = incoming.next().await {
        let Ok(Message::Text(text)) = msg else { break };
        let Ok(msg) = serde_json::from_str::<ClientMessage>(&text) else { continue };
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

async fn send(
    outgoing: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<TcpStream>>,
        Message,
    >,
    msg: &ServerMessage,
) -> anyhow::Result<()> {
    let text = serde_json::to_string(msg)?;
    outgoing.send(Message::Text(text.into())).await?;
    Ok(())
}
