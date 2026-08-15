//! Host-relay (SFU-lite) forwarding: each participant uploads once to the
//! host, and the host forwards already-encoded RTP packets to every other
//! participant in the same channel — not a full mesh (PROJECT.md decision
//! 6). This is the host side only; see [`crate::participant`] for the
//! joining side.
//!
//! Connections arrive from the server's relay endpoint (accepted there
//! under [`corcel_signal::relay::MEDIA_ALPN`] and handed over whole via
//! [`corcel_signal::relay::Relay::media`]). One relay task serves every
//! voice channel of the server — each connection's [`Hello`] says which
//! channel it belongs to.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use bytes::BytesMut;
use corcel_signal::ChannelId;
use iroh::endpoint::Connection;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::Hello;

/// A minimal RTP header — datagrams shorter than this can't carry the SSRC
/// field this relay routes by and are dropped.
const RTP_HEADER_LEN: usize = 12;

/// Everyone currently in a call, as `channel → (connection id → connection)`.
/// The per-relay connection id (not anything derived from the peer) is the
/// key so the same person joining twice can't evict themselves.
type Channels = Arc<Mutex<HashMap<ChannelId, HashMap<u64, Connection>>>>;

pub struct HostRelay;

impl HostRelay {
    /// Drives a host's media forwarding for as long as `incoming` (the
    /// relay endpoint's stream of accepted media connections) stays open —
    /// in practice, the lifetime of the process.
    pub async fn run(mut incoming: mpsc::UnboundedReceiver<Connection>) {
        let channels: Channels = Arc::new(Mutex::new(HashMap::new()));
        let mut next_conn: u64 = 0;
        while let Some(conn) = incoming.recv().await {
            let id = next_conn;
            next_conn += 1;
            let channels = channels.clone();
            tokio::spawn(async move {
                if let Err(err) = serve(conn, id, channels).await {
                    eprintln!("corcel-net: media connection setup failed: {err:#}");
                }
            });
        }
    }
}

/// Handles one participant's connection: hello handshake, then forward its
/// datagrams to everyone else in its channel until it disconnects.
async fn serve(conn: Connection, id: u64, channels: Channels) -> anyhow::Result<()> {
    let (mut writer, reader) = conn.accept_bi().await?;
    let hello = BufReader::new(reader)
        .lines()
        .next_line()
        .await?
        .context("media connection closed before its hello")?;
    let Hello { channel } = serde_json::from_str(&hello)?;

    channels.lock().unwrap().entry(channel).or_default().insert(id, conn.clone());
    // Ack only after registering, so by the time the participant hears
    // back (and starts capturing), traffic already flows to it too.
    let registered = writer.write_all(b"{}\n").await;

    // Streams this participant publishes get fresh SSRCs on the way
    // through, so no two forwarded streams in a channel can collide even
    // if two publishers picked the same SSRC locally.
    let mut forwarded: HashMap<u32, u32> = HashMap::new();

    if registered.is_ok() {
        // A read error is how leaving looks (connection closed) — it ends
        // the loop, not the relay.
        while let Ok(datagram) = conn.read_datagram().await {
            if datagram.len() < RTP_HEADER_LEN {
                continue;
            }
            let original = u32::from_be_bytes(datagram[8..12].try_into().unwrap());
            let ssrc = *forwarded.entry(original).or_insert_with(next_ssrc);
            let mut packet = BytesMut::from(&datagram[..]);
            packet[8..12].copy_from_slice(&ssrc.to_be_bytes());
            let packet = packet.freeze();

            let targets: Vec<Connection> = channels
                .lock()
                .unwrap()
                .get(&channel)
                .into_iter()
                .flat_map(|room| {
                    room.iter().filter(|(other, _)| **other != id).map(|(_, c)| c.clone())
                })
                .collect();
            for target in targets {
                // Failures are the target's problem (congested or closing);
                // its own serve() loop cleans it up.
                let _ = target.send_datagram(packet.clone());
            }
        }
    }

    let mut channels = channels.lock().unwrap();
    if let Some(room) = channels.get_mut(&channel) {
        room.remove(&id);
        if room.is_empty() {
            channels.remove(&channel);
        }
    }
    Ok(())
}

/// Scoped to one forwarded stream's SSRC — a process-wide monotonic counter
/// is sufficient uniqueness for that.
fn next_ssrc() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
