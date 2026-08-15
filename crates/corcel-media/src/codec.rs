//! Picks a concrete GStreamer element for H264 encode/decode, preferring the
//! actively maintained `va` plugin over the older `vaapi` plugin where both
//! are installed — same VAAPI hardware, two plugin generations
//! (PROJECT.md decision 3, which mandates VAAPI with no software fallback).

use anyhow::Context;

fn find(candidates: &[&'static str]) -> anyhow::Result<&'static str> {
    candidates
        .iter()
        .find(|name| gstreamer::ElementFactory::find(name).is_some())
        .copied()
        .with_context(|| {
            format!(
                "none of {candidates:?} are installed — see PROJECT.md decision 3 (VAAPI required)"
            )
        })
}

pub fn h264_encoder() -> anyhow::Result<&'static str> {
    find(&["vah264enc", "vaapih264enc"])
}

pub fn h264_decoder() -> anyhow::Result<&'static str> {
    find(&["vah264dec", "vaapih264dec"])
}

/// The VA-aware convert/scale element matching whichever encoder
/// [`h264_encoder`] picks. Plain `videoconvert`/`videoscale` can't negotiate
/// pipewiresrc's DMA-BUF screen-capture output (fails with "no more input
/// formats"); these import DMA-BUF frames into VA surfaces directly.
pub fn video_postproc() -> anyhow::Result<&'static str> {
    find(&["vapostproc", "vaapipostproc"])
}
