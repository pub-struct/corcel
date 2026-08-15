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

/// Hardware H264 encode: VAAPI on Linux, VideoToolbox on macOS, and the
/// vendor blocks (NVENC/QuickSync/AMF) on Windows with Media Foundation
/// as the always-present catch-all — the same "hardware or bust" policy
/// everywhere (MF itself fronts whatever encoder MFT the machine has).
pub fn h264_encoder() -> anyhow::Result<&'static str> {
    #[cfg(target_os = "macos")]
    return find(&["vtenc_h264_hw", "vtenc_h264"]);
    #[cfg(target_os = "windows")]
    return find(&["nvh264enc", "qsvh264enc", "amfh264enc", "mfh264enc"]);
    #[cfg(target_os = "linux")]
    find(&["vah264enc", "vaapih264enc"])
}

pub fn h264_decoder() -> anyhow::Result<&'static str> {
    #[cfg(target_os = "macos")]
    return find(&["vtdec_hw", "vtdec"]);
    // d3d11h264dec is DXVA — one element covering every GPU vendor.
    #[cfg(target_os = "windows")]
    return find(&["d3d12h264dec", "d3d11h264dec", "mfh264dec"]);
    #[cfg(target_os = "linux")]
    find(&["vah264dec", "vaapih264dec"])
}

/// The VA-aware convert/scale element matching whichever encoder
/// [`h264_encoder`] picks. Plain `videoconvert`/`videoscale` can't negotiate
/// pipewiresrc's DMA-BUF screen-capture output (fails with "no more input
/// formats"); these import DMA-BUF frames into VA surfaces directly.
/// macOS and Windows have no DMA-BUF, and their encoders take plain system
/// memory, so the generic software convert/scale pair is the right
/// front-end there (Windows screen capture downloads out of D3D11 memory
/// before it reaches this — see `capture::screen`).
pub fn video_postproc() -> anyhow::Result<&'static str> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    return Ok("videoconvert ! videoscale");
    #[cfg(target_os = "linux")]
    find(&["vapostproc", "vaapipostproc"])
}
