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

/// Hardware H264 encode: VAAPI on Linux, VideoToolbox on macOS — the same
/// "hardware or bust" policy either way, just Apple's encoder blocks.
pub fn h264_encoder() -> anyhow::Result<&'static str> {
    #[cfg(target_os = "macos")]
    return find(&["vtenc_h264_hw", "vtenc_h264"]);
    #[cfg(not(target_os = "macos"))]
    find(&["vah264enc", "vaapih264enc"])
}

pub fn h264_decoder() -> anyhow::Result<&'static str> {
    #[cfg(target_os = "macos")]
    return find(&["vtdec_hw", "vtdec"]);
    #[cfg(not(target_os = "macos"))]
    find(&["vah264dec", "vaapih264dec"])
}

/// The VA-aware convert/scale element matching whichever encoder
/// [`h264_encoder`] picks. Plain `videoconvert`/`videoscale` can't negotiate
/// pipewiresrc's DMA-BUF screen-capture output (fails with "no more input
/// formats"); these import DMA-BUF frames into VA surfaces directly.
/// macOS has no DMA-BUF and VideoToolbox takes plain system memory, so the
/// generic software convert/scale pair is the right front-end there.
pub fn video_postproc() -> anyhow::Result<&'static str> {
    #[cfg(target_os = "macos")]
    return Ok("videoconvert ! videoscale");
    #[cfg(not(target_os = "macos"))]
    find(&["vapostproc", "vaapipostproc"])
}
