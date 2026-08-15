//! The client side of the signaling protocol: dial a relay by its iroh
//! [`EndpointId`] and exchange protocol messages over channels.
//!
//! The endpoint id comes straight out of the invite link, and iroh's QUIC
//! handshake proves the other side holds the matching secret key — so
//! dialing *is* authenticating, with no certificate pinning layered on
//! top. Discovery (finding a route to that id across the internet) is
//! handled by the endpoint's n0 preset: public DNS lookup for published
//! addresses, public relays for rendezvous, then hole-punching to a direct
//! path where possible.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, OnceCell};

use crate::protocol::{ClientMessage, ServerMessage};
use crate::relay::ALPN;

/// A live signaling connection. Send [`ClientMessage`]s into `outbound`,
/// receive [`ServerMessage`]s from `inbound`; drop it (or either half) to
/// disconnect.
pub struct Connection {
    pub outbound: mpsc::UnboundedSender<ClientMessage>,
    pub inbound: mpsc::UnboundedReceiver<ServerMessage>,
}

/// One outbound endpoint for the whole process, shared by every signaling
/// connection. Its identity is ephemeral (relays don't care who dials, they
/// assign peer ids themselves) and deliberately distinct from any relay
/// endpoint this process hosts — a locally-hosted relay is *dialed*, never
/// short-circuited, so host and guest exercise the same code path.
static CLIENT_ENDPOINT: OnceCell<Endpoint> = OnceCell::const_new();

/// Relays hosted by this process, keyed by endpoint id. The host's own
/// connections to its relay would otherwise round-trip through discovery
/// to find their own machine (slow at best, racy right after binding, when
/// nothing has been published yet) — with the full local address on file
/// we dial loopback directly.
static LOCAL_RELAYS: LazyLock<Mutex<HashMap<EndpointId, EndpointAddr>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Called by [`crate::relay::spawn`] so same-process connections to the
/// relay skip discovery (see [`LOCAL_RELAYS`]).
pub(crate) fn register_local_relay(addr: EndpointAddr) {
    LOCAL_RELAYS.lock().unwrap().insert(addr.id, addr);
}

async fn client_endpoint() -> anyhow::Result<&'static Endpoint> {
    CLIENT_ENDPOINT
        .get_or_try_init(|| async {
            anyhow::Ok(Endpoint::builder(presets::N0).bind().await?)
        })
        .await
}

/// Connects to the relay at `relay`, immediately sends `initial` (the
/// role-declaring hello — Host/Join/Watch/Room), and returns the channel
/// pair for everything after.
pub async fn connect(relay: EndpointId, initial: ClientMessage) -> anyhow::Result<Connection> {
    let endpoint = client_endpoint().await?;
    let addr = LOCAL_RELAYS
        .lock()
        .unwrap()
        .get(&relay)
        .cloned()
        .unwrap_or_else(|| EndpointAddr::from(relay));
    let conn = endpoint.connect(addr, ALPN).await?;
    let (mut writer, reader) = conn.open_bi().await?;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Queue the hello before the writer task starts draining, so it's
    // guaranteed to be the first message on the wire.
    let _ = out_tx.send(initial);

    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let Ok(mut text) = serde_json::to_string(&msg) else { continue };
            text.push('\n');
            if writer.write_all(text.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        // The reader task owns the connection: QUIC connections close when
        // every handle drops, and the stream halves don't keep it alive on
        // their own.
        let _conn = conn;
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = serde_json::from_str::<ServerMessage>(&line) else { continue };
            if in_tx.send(msg).is_err() {
                break;
            }
        }
    });

    Ok(Connection {
        outbound: out_tx,
        inbound: in_rx,
    })
}
