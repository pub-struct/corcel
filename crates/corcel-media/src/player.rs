//! Progressive playback of a remote video URL — chat's inline video embeds.
//! `playbin` does all the heavy lifting (souphttpsrc fetch, buffering,
//! demux, decode, audio output), and its video branch is redirected into the
//! same BGRA `appsink` arrangement as [`crate::playback::VideoPlayback`], so
//! the app renders these frames exactly like a call's.

use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use tokio::sync::mpsc;

use crate::pipeline;
use crate::playback::{FRAME_CHANNEL_CAPACITY, VideoFrame};

/// What a [`UrlPlayer`] emits on its channel.
pub enum PlayerEvent {
    Frame(VideoFrame),
    /// The stream reached its end. The pipeline pauses itself; the next
    /// [`UrlPlayer::set_paused`]`(false)` rewinds and replays (see there).
    Ended,
}

pub struct UrlPlayer {
    gst_pipeline: gstreamer::Pipeline,
}

impl UrlPlayer {
    /// Starts playing `url` immediately (the caller decides when to create
    /// one — chat embeds are click-to-play, so creation *is* the play
    /// action). Frames and the end-of-stream notice arrive on the returned
    /// channel; audio goes straight to the default output.
    pub fn new(url: &str) -> anyhow::Result<(Self, mpsc::Receiver<PlayerEvent>)> {
        // The URL is spliced into gst-launch syntax below; it comes from a
        // whitespace-delimited chat token so these can't appear in practice,
        // but a quote would otherwise break out of the property value.
        anyhow::ensure!(
            (url.starts_with("http://") || url.starts_with("https://"))
                && !url.contains(['"', '\\'])
                && !url.contains(char::is_whitespace),
            "not a playable http(s) url: {url}"
        );
        let description = format!(
            "playbin uri=\"{url}\" video-sink=\"videoconvert ! video/x-raw,format=BGRA \
             ! appsink name=sink emit-signals=false sync=true max-buffers=2 drop=true\""
        );
        let gst_pipeline = pipeline::build(&description)?;
        let sink = pipeline::element::<AppSink>(&gst_pipeline, "sink")?;

        let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        Self::watch(&gst_pipeline, tx.clone());
        gst_pipeline.set_state(gstreamer::State::Playing)?;

        // Same pump as VideoPlayback's, except `pull_sample`'s error isn't
        // always fatal here: it also fails while the pipeline sits at EOS or
        // paused-for-good, and a later replay (rewind + play) makes samples
        // flow again — so only a torn-down (Null) pipeline ends the thread.
        let pump_pipeline = gst_pipeline.clone();
        std::thread::spawn(move || {
            loop {
                match sink.pull_sample() {
                    Ok(sample) => {
                        let Some(buffer) = sample.buffer() else { continue };
                        let Some(caps) = sample.caps() else { continue };
                        let Ok(info) = gstreamer_video::VideoInfo::from_caps(caps) else { continue };
                        let Ok(map) = buffer.map_readable() else { continue };
                        let frame = VideoFrame {
                            width: info.width(),
                            height: info.height(),
                            data: map.to_vec(),
                        };
                        match tx.try_send(PlayerEvent::Frame(frame)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => return,
                        }
                    }
                    Err(_) => {
                        let (_, current, _) = pump_pipeline.state(gstreamer::ClockTime::ZERO);
                        if current == gstreamer::State::Null || tx.is_closed() {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }
        });

        Ok((Self { gst_pipeline }, rx))
    }

    /// Pauses or resumes. Resuming a stream that already ended rewinds to
    /// the start first, so "play again" is the same button as "play".
    pub fn set_paused(&self, paused: bool) {
        if !paused {
            let position = self.gst_pipeline.query_position::<gstreamer::ClockTime>();
            let duration = self.gst_pipeline.query_duration::<gstreamer::ClockTime>();
            if let (Some(position), Some(duration)) = (position, duration) {
                if position + gstreamer::ClockTime::from_mseconds(500) >= duration {
                    let _ = self.gst_pipeline.seek_simple(
                        gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::KEY_UNIT,
                        gstreamer::ClockTime::ZERO,
                    );
                }
            }
        }
        let state = if paused { gstreamer::State::Paused } else { gstreamer::State::Playing };
        let _ = self.gst_pipeline.set_state(state);
    }

    /// Like [`pipeline::watch`], but EOS pauses the pipeline and tells the
    /// caller instead of ending the watch — the player stays alive for a
    /// replay. The thread still exits when [`pipeline::stop`] flushes the
    /// bus (`timed_pop_filtered` then returns `None`).
    fn watch(gst_pipeline: &gstreamer::Pipeline, tx: mpsc::Sender<PlayerEvent>) {
        let Some(bus) = gst_pipeline.bus() else { return };
        let weak = gst_pipeline.downgrade();
        std::thread::spawn(move || {
            while let Some(msg) = bus.timed_pop_filtered(
                gstreamer::ClockTime::NONE,
                &[gstreamer::MessageType::Error, gstreamer::MessageType::Eos],
            ) {
                match msg.view() {
                    gstreamer::MessageView::Error(err) => {
                        eprintln!(
                            "corcel-media: url player pipeline error: {} ({:?})",
                            err.error(),
                            err.debug()
                        );
                        break;
                    }
                    gstreamer::MessageView::Eos(_) => {
                        let Some(gst_pipeline) = weak.upgrade() else { break };
                        let _ = gst_pipeline.set_state(gstreamer::State::Paused);
                        if tx.blocking_send(PlayerEvent::Ended).is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
    }
}

impl Drop for UrlPlayer {
    fn drop(&mut self) {
        pipeline::stop(&self.gst_pipeline);
    }
}
