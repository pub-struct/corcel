//! Decode pipelines: take incoming RTP packets (as delivered by a
//! `TrackRemote`'s `TrackRemoteEvent::OnRtpPacket`) and hardware-decode them
//! back into raw video frames, or straight out to the system's audio
//! output. Video decodes to raw frames rather than rendering directly,
//! matching PROJECT.md decision 7's stage 1 plan (CPU-readback into GPUI,
//! zero-copy texture import deferred).

use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSrc};
use tokio::sync::mpsc;

use crate::{codec, pipeline, rtp};

/// One decoded video frame, tightly packed BGRA8, row-major — the byte
/// order `gpui::RenderImage` expects, so `freecord-app` can hand this
/// straight to it without a conversion pass.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Bound applied to the decoded-frame channel: only the newest frame is
/// ever useful for rendering, so a stalled consumer (e.g. the GPUI thread
/// busy elsewhere) drops older frames instead of buffering ~3.5MB (720p
/// BGRA8) apiece without limit. 2 gives a little slack without allowing
/// real backlog to build.
pub const FRAME_CHANNEL_CAPACITY: usize = 2;

pub struct VideoPlayback {
    gst_pipeline: gstreamer::Pipeline,
    src: AppSrc,
    pub frames: mpsc::Receiver<VideoFrame>,
}

impl VideoPlayback {
    pub fn new() -> anyhow::Result<Self> {
        let description = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true \
             caps=\"application/x-rtp,media=video,encoding-name=H264,clock-rate=90000,payload={pt}\" \
             ! rtpjitterbuffer ! rtph264depay ! h264parse ! {decoder} ! videoconvert \
             ! video/x-raw,format=BGRA ! appsink name=sink emit-signals=false sync=false \
             max-buffers=1 drop=true",
            decoder = codec::h264_decoder()?,
            pt = freecord_net::H264_PAYLOAD_TYPE,
        );
        let gst_pipeline = pipeline::build(&description)?;
        let src = pipeline::element::<AppSrc>(&gst_pipeline, "src")?;
        let sink = pipeline::element::<AppSink>(&gst_pipeline, "sink")?;
        pipeline::watch(&gst_pipeline, "video playback");
        gst_pipeline.set_state(gstreamer::State::Playing)?;

        let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        std::thread::spawn(move || {
            while let Ok(sample) = sink.pull_sample() {
                let Some(buffer) = sample.buffer() else {
                    continue;
                };
                let Some(caps) = sample.caps() else {
                    continue;
                };
                let Ok(info) = gstreamer_video::VideoInfo::from_caps(caps) else {
                    continue;
                };
                let Ok(map) = buffer.map_readable() else {
                    continue;
                };
                let frame = VideoFrame {
                    width: info.width(),
                    height: info.height(),
                    data: map.to_vec(),
                };
                match tx.try_send(frame) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        });

        Ok(Self {
            gst_pipeline,
            src,
            frames: rx,
        })
    }

    /// Feeds one incoming H264 RTP packet into the decoder.
    pub fn push(&self, packet: &rtc::rtp::Packet) -> anyhow::Result<()> {
        let bytes = rtp::marshal(packet)?;
        self.src.push_buffer(gstreamer::Buffer::from_slice(bytes))?;
        Ok(())
    }
}

impl Drop for VideoPlayback {
    fn drop(&mut self) {
        pipeline::stop(&self.gst_pipeline);
    }
}

pub struct AudioPlayback {
    gst_pipeline: gstreamer::Pipeline,
    src: AppSrc,
}

impl AudioPlayback {
    /// Decodes straight to the default PipeWire audio sink — there's no
    /// caller-facing output here, unlike [`VideoPlayback`], since audio has
    /// nowhere else useful to go.
    pub fn new() -> anyhow::Result<Self> {
        let description = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true \
             caps=\"application/x-rtp,media=audio,encoding-name=OPUS,clock-rate=48000,payload={pt}\" \
             ! rtpjitterbuffer ! rtpopusdepay ! opusdec ! audioconvert ! audioresample ! pipewiresink",
            pt = freecord_net::OPUS_PAYLOAD_TYPE,
        );
        let gst_pipeline = pipeline::build(&description)?;
        let src = pipeline::element::<AppSrc>(&gst_pipeline, "src")?;
        pipeline::watch(&gst_pipeline, "audio playback");
        gst_pipeline.set_state(gstreamer::State::Playing)?;

        Ok(Self { gst_pipeline, src })
    }

    /// Feeds one incoming Opus RTP packet into the decoder.
    pub fn push(&self, packet: &rtc::rtp::Packet) -> anyhow::Result<()> {
        let bytes = rtp::marshal(packet)?;
        self.src.push_buffer(gstreamer::Buffer::from_slice(bytes))?;
        Ok(())
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        pipeline::stop(&self.gst_pipeline);
    }
}
