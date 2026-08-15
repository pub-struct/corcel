//! Glue between the UI and `corcel-signal`/`corcel-net`: creating a
//! server (spawning a local signaling relay plus one host-relay per channel)
//! and joining a channel (signaling connect + WebRTC handshake). No media is
//! wired in yet — see PROJECT.md decision 6 for the host-relay topology this
//! mirrors, and [`crate::session`]'s callers for how results cross back onto
//! the GPUI thread.

use corcel_net::{CallHandle, Participant};
use corcel_signal::{ChannelId, ClientMessage, EndpointId, RelayIdentity};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use crate::invite::{ChannelInfo, ChannelKind, ServerLink};

/// Creates a brand-new server: mints its identity (server id + iroh secret
/// key), spawns its relay, and starts hosting the default channels. The
/// caller persists the returned identity so [`rehost`] can bring the same
/// server (same endpoint id, so the *same invite links*) back after a
/// restart.
pub async fn host(name: String) -> anyhow::Result<(ServerLink, RelayIdentity)> {
    let identity = RelayIdentity::generate()?;
    let channels = vec![
        ChannelInfo { id: Uuid::new_v4(), name: "general".to_string(), kind: ChannelKind::Text },
        ChannelInfo { id: Uuid::new_v4(), name: "General".to_string(), kind: ChannelKind::Voice },
    ];
    let link = spawn_and_host(Uuid::new_v4(), name, channels, &identity).await?;
    Ok((link, identity))
}

/// Brings a persisted hosted server back up after an app restart. Because
/// the relay's dialable identity is its key (not an address), the returned
/// link is equivalent to the old one — links shared months ago keep
/// working, from any network the host wanders to.
pub async fn rehost(link: ServerLink, identity: RelayIdentity) -> anyhow::Result<ServerLink> {
    spawn_and_host(link.id, link.name, link.channels, &identity).await
}

/// Spawns the relay with `identity` and starts a host-side media relay for
/// every *voice* channel (text channels need no host claim — chat flows
/// through the relay's room, not a WebRTC session). Returns the link to
/// share; everything in it is stable across launches.
async fn spawn_and_host(
    server_id: Uuid,
    name: String,
    channels: Vec<ChannelInfo>,
    identity: &RelayIdentity,
) -> anyhow::Result<ServerLink> {
    let relay = corcel_signal::relay::spawn(identity).await?;
    let relay_id = relay.endpoint_id;

    for channel in &channels {
        if channel.kind != ChannelKind::Voice {
            continue;
        }
        let id = channel.id;
        tokio::spawn(async move {
            let conn = match corcel_signal::client::connect(relay_id, ClientMessage::Host { channel: id }).await {
                Ok(conn) => conn,
                Err(err) => {
                    eprintln!("corcel-app: failed to host channel {id}: {err:#}");
                    return;
                }
            };
            if let Err(err) = corcel_net::HostRelay::run(conn).await {
                eprintln!("corcel-app: host relay for channel {id} exited: {err:#}");
            }
        });
    }

    Ok(ServerLink {
        id: server_id,
        name,
        node: Some(relay_id.to_string()),
        channels,
    })
}

/// Opens this member's chat-room connection to a server's relay: the
/// long-lived stream every chat broadcast, direct history exchange, and
/// room-presence event flows over. The host dials its own relay the same
/// way — [`corcel_signal::client`] short-circuits same-process relays to
/// loopback internally.
pub async fn open_room(
    relay: EndpointId,
    server_id: Uuid,
) -> anyhow::Result<corcel_signal::client::Connection> {
    corcel_signal::client::connect(relay, ClientMessage::Room { channel: server_id }).await
}

/// Joins a channel already listed in `link` — host and guest use the exact
/// same path (see [`corcel_signal::client`]'s doc comment on why the host
/// also connects to its own relay as a plain participant, same as everyone
/// else). Once connected, starts streaming mic audio in and remote audio
/// out — see [`attach_media`].
pub async fn join(link: ServerLink, channel: ChannelId) -> anyhow::Result<CallSession> {
    let conn = corcel_signal::client::connect(link.endpoint_id()?, ClientMessage::Join { channel }).await?;
    let participant = corcel_net::join(conn).await?;
    attach_media(participant).await
}

/// A live call, handed back to the UI once [`join`]
/// connects: `pc` lets it publish more tracks later (e.g. [`start_screen_share`]),
/// and `remote_video` streams decoded frames from whatever video track(s)
/// the host forwards, for rendering.
pub struct CallSession {
    pub pc: CallHandle,
    pub remote_video: mpsc::Receiver<corcel_media::VideoFrame>,
    /// Sending `true` (or just dropping this) is the only signal every task
    /// [`attach_media`] spawned — statically there, and dynamically as new
    /// tracks arrive — shares to know when to stop. Nothing else ties their
    /// lifetime to this struct's: without this, mic upload and (more
    /// noticeably) incoming audio/video playback keep running in the
    /// background — you keep hearing everyone else — even after the caller
    /// has otherwise moved on. Combine with closing `pc` (see
    /// [`CallHandle::close`]) to also tear down the WebRTC connection so the
    /// far side notices too.
    pub hang_up: watch::Sender<bool>,
    /// Live mute switch for the mic-upload task: while `true`, captured mic
    /// packets are dropped instead of written to the outgoing track. The
    /// capture pipeline keeps running (so unmute is instant, no device
    /// re-open), but nothing leaves the machine — which is what "muted" has
    /// to mean to be trustworthy.
    pub mute: watch::Sender<bool>,
    /// Live deafen switch for every incoming-audio playback task: while
    /// `true`, received packets are dropped instead of pushed to the
    /// speaker — the mirror image of `mute`, applied on the way in. Tracks
    /// keep flowing (undeafen is instant); they just make no sound.
    pub deafen: watch::Sender<bool>,
    /// `true` while the local mic hears someone talking (see
    /// [`corcel_media::capture::microphone`]) — drives this side's speaking
    /// ring and, via the shell's watcher, the broadcast to everyone else.
    /// Reports raw mic activity even while muted; the consumer decides what
    /// muted means for display.
    pub speaking: watch::Receiver<bool>,
    /// A sender into the same frame channel `remote_video` reads from, so
    /// local capture ([`start_screen_share`]/[`start_camera`]) can feed its
    /// own self-preview frames to the very stage that renders remote video —
    /// without sharing, you'd be the only one in the call who can't see the
    /// screen you're sharing.
    pub local_video: mpsc::Sender<corcel_media::VideoFrame>,
}

/// Wires this participant's connection up to real media (PROJECT.md decision
/// 9): captures the mic and publishes it as an outgoing track, and plays
/// back every track the host forwards — audio straight to the speaker,
/// video decoded and handed to the caller via [`CallSession::remote_video`].
/// Every task spawned here selects on `hang_up_rx` (a clone of
/// [`CallSession::hang_up`]'s receiver) so [`CallSession::hang_up`] can stop
/// all of them, including ones spawned later for tracks that arrive after
/// this function returns.
async fn attach_media(participant: Participant) -> anyhow::Result<CallSession> {
    let Participant { pc, mut tracks, .. } = participant;
    let (hang_up_tx, hang_up_rx) = watch::channel(false);
    let (mute_tx, mute_rx) = watch::channel(false);
    let (deafen_tx, deafen_rx) = watch::channel(false);

    let (mut mic, speaking_rx) = corcel_media::capture::microphone()?;
    let outgoing = corcel_net::publish_audio_track(&pc).await?;
    let mut stop_rx = hang_up_rx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop_rx.changed() => break,
                packet = mic.packets.recv() => {
                    let Some(packet) = packet else { break };
                    if *mute_rx.borrow() {
                        continue;
                    }
                    if !outgoing.write_rtp(packet).await {
                        break;
                    }
                }
            }
        }
    });

    // Same bound/drop-when-full reasoning as corcel_media::playback's own
    // internal frame channel (this is the next hop downstream of it): only
    // the newest frame matters for rendering, so a stalled GPUI thread
    // shouldn't make this buffer decoded frames without limit.
    let (video_tx, video_rx) = mpsc::channel(corcel_media::playback::FRAME_CHANNEL_CAPACITY);
    let local_video = video_tx.clone();
    let mut stop_rx = hang_up_rx.clone();
    tokio::spawn(async move {
        loop {
            let track = tokio::select! {
                _ = stop_rx.changed() => break,
                track = tracks.recv() => track,
            };
            let Some(track) = track else { break };

            if let Some(mut packets) = corcel_net::subscribe_audio(track.clone()).await {
                let playback = match corcel_media::AudioPlayback::new() {
                    Ok(playback) => playback,
                    Err(err) => {
                        eprintln!("corcel-app: failed to start audio playback: {err:#}");
                        continue;
                    }
                };
                let mut stop_rx = hang_up_rx.clone();
                let deafen_rx = deafen_rx.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = stop_rx.changed() => break,
                            packet = packets.recv() => {
                                let Some(packet) = packet else { break };
                                // Deafened: drop instead of play — same
                                // trustworthy shape as the mic loop's mute.
                                if *deafen_rx.borrow() {
                                    continue;
                                }
                                let _ = playback.push(&packet);
                            }
                        }
                    }
                });
                continue;
            }

            if let Some(mut packets) = corcel_net::subscribe_video(track).await {
                let mut playback = match corcel_media::VideoPlayback::new() {
                    Ok(playback) => playback,
                    Err(err) => {
                        eprintln!("corcel-app: failed to start video playback: {err:#}");
                        continue;
                    }
                };
                let video_tx = video_tx.clone();
                let mut stop_rx = hang_up_rx.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = stop_rx.changed() => break,
                            packet = packets.recv() => {
                                let Some(packet) = packet else { break };
                                let _ = playback.push(&packet);
                            }
                            frame = playback.frames.recv() => {
                                let Some(frame) = frame else { break };
                                match video_tx.try_send(frame) {
                                    Ok(()) => {}
                                    Err(mpsc::error::TrySendError::Full(_)) => {}
                                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                                }
                            }
                        }
                    }
                });
            }
        }
    });

    Ok(CallSession {
        pc,
        remote_video: video_rx,
        hang_up: hang_up_tx,
        mute: mute_tx,
        deafen: deafen_tx,
        speaking: speaking_rx,
        local_video,
    })
}

/// A screen share started with [`start_screen_share`]. Dropping/[`stop`]ping
/// this tears down the capture pipeline and releases the portal's screen
/// recording session — there's no way to "pause" short of stopping outright.
///
/// [`stop`]: ScreenShareHandle::stop
pub struct ScreenShareHandle {
    stop: oneshot::Sender<()>,
}

impl ScreenShareHandle {
    pub fn stop(self) {
        let _ = self.stop.send(());
    }
}

/// A camera capture started with [`start_camera`]. Dropping/[`stop`]ping this
/// tears down the capture pipeline — unlike [`ScreenShareHandle`] there's no
/// portal session to close, since V4L2 capture doesn't go through
/// `xdg-desktop-portal`.
///
/// [`stop`]: CameraHandle::stop
pub struct CameraHandle {
    stop: oneshot::Sender<()>,
}

impl CameraHandle {
    pub fn stop(self) {
        let _ = self.stop.send(());
    }
}

/// Captures `device` (e.g. `/dev/video0`) and publishes it as a video track
/// on `pc`, same as [`start_screen_share`] but reading from V4L2 instead of
/// the ScreenCast portal. `preview` receives locally decoded frames of the
/// same stream — see [`start_screen_share`] for why self-preview is done by
/// decoding our own RTP rather than teeing the raw capture pipeline.
pub async fn start_camera(
    pc: CallHandle,
    device: &str,
    preview: mpsc::Sender<corcel_media::VideoFrame>,
) -> anyhow::Result<CameraHandle> {
    let mut capture = corcel_media::capture::camera(device)?;
    let outgoing = corcel_net::publish_video_track(&pc).await?;
    let mut playback = corcel_media::VideoPlayback::new()?;

    let (stop_tx, mut stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                packet = capture.packets.recv() => {
                    let Some(packet) = packet else { break };
                    let _ = playback.push(&packet);
                    if !outgoing.write_rtp(packet).await {
                        break;
                    }
                }
                frame = playback.frames.recv() => {
                    let Some(frame) = frame else { break };
                    let _ = preview.try_send(frame);
                }
            }
        }
        capture.close().await;
    });

    Ok(CameraHandle { stop: stop_tx })
}

/// Prompts the user (via the `xdg-desktop-portal` ScreenCast picker) to
/// choose a monitor or window, then publishes it as a video track on `pc` —
/// the same connection [`attach_media`] already has the mic and remote
/// tracks flowing over, so no renegotiation dance beyond what
/// `publish_video_track` already triggers.
///
/// `preview` (normally [`CallSession::local_video`]) receives locally
/// decoded frames of the outgoing stream, so the sharer sees exactly what
/// everyone else sees. Self-preview works by decoding our own RTP through
/// the same [`corcel_media::VideoPlayback`] path remote video uses rather
/// than teeing raw frames out of the capture pipeline: it costs one extra
/// decode, but reuses proven components unchanged — and what it shows is
/// the *encoded* stream, artifacts and all, which is the honest preview.
pub async fn start_screen_share(
    pc: CallHandle,
    preview: mpsc::Sender<corcel_media::VideoFrame>,
) -> anyhow::Result<ScreenShareHandle> {
    let mut capture = corcel_media::capture::screen().await?;
    let outgoing = corcel_net::publish_video_track(&pc).await?;
    let mut playback = corcel_media::VideoPlayback::new()?;

    let (stop_tx, mut stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                packet = capture.packets.recv() => {
                    let Some(packet) = packet else { break };
                    let _ = playback.push(&packet);
                    if !outgoing.write_rtp(packet).await {
                        break;
                    }
                }
                frame = playback.frames.recv() => {
                    let Some(frame) = frame else { break };
                    let _ = preview.try_send(frame);
                }
            }
        }
        // Closes the portal ScreenCast session (a D-Bus round trip `Drop`
        // can't do on its own — see `Capture::close`) so GNOME's "you are
        // sharing your screen" indicator actually clears, instead of
        // sticking around until the whole process exits.
        capture.close().await;
    });

    Ok(ScreenShareHandle { stop: stop_tx })
}
