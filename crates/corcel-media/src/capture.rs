//! Encode pipelines: capture raw media, hardware-encode it to H264 (video)
//! or Opus (audio), and RTP-packetize it — producing exactly the
//! `rtc::rtp::Packet`s that a `TrackLocal::write_rtp` call expects
//! (PROJECT.md decisions 2-4). What the caller does with those packets
//! (which local track to write them to, at what SSRC) is `corcel-net`'s
//! concern, not this crate's.

use std::any::Any;
use std::future::Future;
use std::os::fd::AsRawFd;
use std::pin::Pin;

use anyhow::Context;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use tokio::sync::{mpsc, watch};

use crate::{codec, pipeline, rtp};

/// A running capture pipeline. Producing packets on [`Capture::packets`]
/// stops, and the underlying pipeline tears down, when this is dropped —
/// except for [`Self::on_close`], which needs an explicit [`Self::close`]
/// call (see there for why).
pub struct Capture {
    gst_pipeline: gstreamer::Pipeline,
    pub packets: mpsc::Receiver<rtc::rtp::Packet>,
    /// Resources that must outlive the pipeline but that nothing else here
    /// touches — e.g. a screen capture's PipeWire remote fd.
    _keep_alive: Vec<Box<dyn Any + Send>>,
    /// Async cleanup that a plain `Drop` can't perform — e.g. a screen
    /// capture's `ashpd` portal `Session` has no `Drop` impl at all, since
    /// closing it is a D-Bus round trip and `Drop` can only run sync code.
    /// Left undone, the portal-side (Mutter, for GNOME) session — and the
    /// OS-level "you are sharing your screen" indicator tied to it — stays
    /// open indefinitely even after this pipeline itself has stopped.
    on_close: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl Capture {
    /// Tears down the pipeline and runs [`Self::on_close`] (if any) —
    /// callers that can `.await` (i.e. everything but a `Drop` impl) should
    /// prefer this over plain `drop`, so a screen capture's portal session
    /// actually closes instead of leaking until the process exits.
    pub async fn close(mut self) {
        if let Some(on_close) = self.on_close.take() {
            on_close.await;
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        pipeline::stop(&self.gst_pipeline);
        // `close()` normally consumes `self` and awaits `on_close` directly
        // (see there). Reaching here with it still set means the caller
        // dropped without calling `close()` — e.g. an early `?` return
        // between `screen()` building this and the caller getting a chance
        // to use it. Fire-and-forget it rather than leaking the portal
        // session; every construction path runs inside a tokio task (via
        // `corcel-app::runtime::spawn_and_send`), so a runtime handle is
        // always available here in practice.
        if let Some(on_close) = self.on_close.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(on_close);
            }
        }
    }
}

/// Bound applied to every capture's outgoing packet channel: real-time RTP
/// tolerates loss far better than unbounded memory growth, so a stalled
/// consumer (e.g. the network write side backpressuring) drops packets
/// instead of buffering them indefinitely. Small multiple of typical
/// jitter, generous for packets this size (~1200B MTU).
const PACKET_CHANNEL_CAPACITY: usize = 32;

fn start(
    label: &'static str,
    description: &str,
    keep_alive: Vec<Box<dyn Any + Send>>,
    on_close: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
) -> anyhow::Result<Capture> {
    let gst_pipeline = pipeline::build(description)?;
    let sink = pipeline::element::<AppSink>(&gst_pipeline, "sink")?;
    pipeline::watch(&gst_pipeline, label);
    gst_pipeline.set_state(gstreamer::State::Playing)?;

    let packets = pump_packets(label, sink);
    Ok(Capture {
        gst_pipeline,
        packets,
        _keep_alive: keep_alive,
        on_close,
    })
}

/// Spawns the thread that pulls encoded samples out of the pipeline's
/// appsink and parses them into RTP packets — shared by every capture,
/// whether its bus is watched by [`pipeline::watch`] or (the microphone)
/// by its own combined watcher.
fn pump_packets(label: &'static str, sink: AppSink) -> mpsc::Receiver<rtc::rtp::Packet> {
    let (tx, rx) = mpsc::channel(PACKET_CHANNEL_CAPACITY);
    std::thread::spawn(move || {
        while let Ok(sample) = sink.pull_sample() {
            let Some(buffer) = sample.buffer() else {
                continue;
            };
            let Ok(map) = buffer.map_readable() else {
                continue;
            };
            match rtp::unmarshal(&map) {
                Ok(packet) => match tx.try_send(packet) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                },
                Err(err) => {
                    eprintln!("corcel-media: {label}: failed to parse RTP packet: {err:#}")
                }
            }
        }
    });
    rx
}

/// Captures from a V4L2 camera device (e.g. `/dev/video0`).
pub fn camera(device: &str) -> anyhow::Result<Capture> {
    let description = format!(
        "v4l2src device=\"{device}\" ! {postproc} \
         ! video/x-raw,width=1280,height=720,framerate=30/1 ! {encoder} \
         ! h264parse config-interval=-1 ! rtph264pay pt={pt} mtu=1200 config-interval=-1 \
         ! appsink name=sink emit-signals=false sync=false",
        postproc = codec::video_postproc()?,
        encoder = codec::h264_encoder()?,
        pt = corcel_net::H264_PAYLOAD_TYPE,
    );
    start("camera", &description, Vec::new(), None)
}

/// RMS loudness (dBFS) above which the mic counts as someone speaking.
/// ~-40 dB sits comfortably between room tone / keyboard noise and an
/// actual voice at normal mic gain.
const SPEAKING_THRESHOLD_DB: f64 = -40.0;

/// How long the speaking flag stays up after the level last crossed the
/// threshold, so the natural dips between words don't flicker it.
const SPEAKING_HANGOVER: std::time::Duration = std::time::Duration::from_millis(300);

/// Captures from the default PipeWire audio source (GNOME/Wayland routes
/// the system mic through PipeWire, same as screen capture). Also returns
/// a live "someone is speaking into this mic" flag, measured *before* the
/// encoder by GStreamer's `level` element — so it keeps reporting while
/// the app-level mute merely discards the encoded packets downstream.
pub fn microphone() -> anyhow::Result<(Capture, watch::Receiver<bool>)> {
    let description = format!(
        "pipewiresrc ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 \
         ! level interval=100000000 ! opusenc ! rtpopuspay pt={pt} \
         ! appsink name=sink emit-signals=false sync=false",
        pt = corcel_net::OPUS_PAYLOAD_TYPE,
    );
    let gst_pipeline = pipeline::build(&description)?;
    let sink = pipeline::element::<AppSink>(&gst_pipeline, "sink")?;
    // Not `pipeline::watch` — a bus supports only one popping consumer, and
    // this pipeline needs its Element messages (the `level` reports) read
    // alongside Error/EOS. One combined thread does both.
    let speaking = watch_microphone_bus(&gst_pipeline);
    gst_pipeline.set_state(gstreamer::State::Playing)?;

    let packets = pump_packets("microphone", sink);
    let capture = Capture {
        gst_pipeline,
        packets,
        _keep_alive: Vec::new(),
        on_close: None,
    };
    Ok((capture, speaking))
}

/// The microphone pipeline's bus thread: plays [`pipeline::watch`]'s role
/// (log the first error / EOS) *and* turns the `level` element's periodic
/// loudness reports into an edge-triggered speaking flag. The thread exits
/// when [`pipeline::stop`] flushes the bus (the `Capture`'s `Drop`),
/// resetting the flag to `false` on the way out.
fn watch_microphone_bus(gst_pipeline: &gstreamer::Pipeline) -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    let Some(bus) = gst_pipeline.bus() else { return rx };
    std::thread::spawn(move || {
        // When the level last crossed the threshold — the hangover only
        // needs re-evaluating when a report arrives, and reports tick every
        // 100ms for as long as the pipeline runs.
        let mut last_loud: Option<std::time::Instant> = None;
        while let Some(msg) = bus.timed_pop_filtered(
            gstreamer::ClockTime::NONE,
            &[
                gstreamer::MessageType::Error,
                gstreamer::MessageType::Eos,
                gstreamer::MessageType::Element,
            ],
        ) {
            match msg.view() {
                gstreamer::MessageView::Error(err) => {
                    eprintln!(
                        "corcel-media: microphone pipeline error: {} ({:?})",
                        err.error(),
                        err.debug()
                    );
                    break;
                }
                gstreamer::MessageView::Eos(_) => break,
                gstreamer::MessageView::Element(element) => {
                    let Some(report) = element.structure().filter(|s| s.name() == "level") else {
                        continue;
                    };
                    // "rms" is one dBFS value per channel; the loudest
                    // channel decides (a mono voice on a stereo capture
                    // shouldn't be halved into silence).
                    let Ok(rms) = report.get::<gstreamer::glib::ValueArray>("rms") else {
                        continue;
                    };
                    let loudest = rms
                        .iter()
                        .filter_map(|value| value.get::<f64>().ok())
                        .fold(f64::NEG_INFINITY, f64::max);
                    let now = std::time::Instant::now();
                    if loudest > SPEAKING_THRESHOLD_DB {
                        last_loud = Some(now);
                    }
                    let speaking =
                        last_loud.is_some_and(|at| now.duration_since(at) < SPEAKING_HANGOVER);
                    tx.send_if_modified(|current| {
                        let changed = *current != speaking;
                        *current = speaking;
                        changed
                    });
                }
                _ => {}
            }
        }
        let _ = tx.send(false);
    });
    rx
}

/// Captures the screen via the `org.freedesktop.portal.ScreenCast` portal
/// (PROJECT.md decision 2): prompts the user to pick a monitor or window,
/// then feeds the resulting PipeWire stream through the same H264 encode
/// pipeline as [`camera`].
pub async fn screen() -> anyhow::Result<Capture> {
    use ashpd::desktop::PersistMode;
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};

    let proxy = Screencast::new()
        .await
        .context("connect to the ScreenCast portal")?;
    let session = proxy
        .create_session(Default::default())
        .await
        .context("create a ScreenCast session")?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(SourceType::Monitor | SourceType::Window)
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await
        .context("select ScreenCast sources")?
        .response()
        .context("ScreenCast source selection was not approved")?;
    let streams = proxy
        .start(&session, None, Default::default())
        .await
        .context("start the ScreenCast session")?
        .response()
        .context("ScreenCast session was not approved")?;
    let stream = streams
        .streams()
        .first()
        .context("portal returned no ScreenCast streams")?;
    let node_id = stream.pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("open the ScreenCast PipeWire remote")?;

    // `always-copy=true` makes pipewiresrc itself request plain (non-DMABuf)
    // memory from PipeWire at the SPA level, not just restrict the
    // GStreamer-caps side of negotiation.
    //
    // No `framerate=30/1` constraint here (unlike `camera`'s pipeline):
    // capsfilter fields fixed downstream propagate backward through
    // negotiation, and pipewiresrc's logs showed that pinning to exactly
    // 30/1 was making it all the way back to pipewiresrc's own negotiated
    // caps — but Mutter's ScreenCast PipeWire node apparently doesn't
    // actually support being pinned to that exact rate (it only offers
    // whatever the monitor's real refresh rate is), so asking for it
    // outright failed SPA-level negotiation ("no more input formats").
    // Leaving framerate unconstrained lets it negotiate whatever the
    // compositor actually offers.
    let description = format!(
        "pipewiresrc fd={fd} path={node_id} do-timestamp=true always-copy=true ! video/x-raw \
         ! {postproc} ! video/x-raw ! {encoder} \
         ! h264parse config-interval=-1 ! rtph264pay pt={pt} mtu=1200 config-interval=-1 \
         ! appsink name=sink emit-signals=false sync=false",
        fd = fd.as_raw_fd(),
        postproc = codec::video_postproc()?,
        encoder = codec::h264_encoder()?,
        pt = corcel_net::H264_PAYLOAD_TYPE,
    );

    // `session.close()` is a D-Bus round trip, so it can't happen from
    // `Drop` — stashed here as a future instead, for [`Capture::close`] to
    // run explicitly once the caller is done sharing.
    let on_close: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
        if let Err(err) = session.close().await {
            eprintln!("corcel-media: screen: failed to close ScreenCast portal session: {err:#}");
        }
    });

    start("screen", &description, vec![Box::new(fd)], Some(on_close))
}
