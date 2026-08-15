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
///
/// Returned with its streaming properties baked in, keyed to whichever
/// element was found. The non-negotiable one is a bounded keyframe
/// interval: corcel has no PLI/FIR path asking the encoder for a refresh,
/// so a receiver that joins (or loses packets) can only recover at the
/// next periodic keyframe. Encoders left at their defaults may emit
/// exactly one IDR ever on near-static content (VideoToolbox does on
/// screen capture), which turns "joined after frame 0" into a permanently
/// black stream.
///
/// `bitrate_kbps` is an explicit target, not a hint: every encoder here
/// defaults to either "auto" or ~2Mbps, both of which pick call-quality
/// rates (VideoToolbox auto lands at ~1Mbps for a 1920x1200 screen) that
/// smear text into mush the moment anything moves. Every element listed
/// takes a `bitrate` property in kbps.
pub fn h264_encoder(bitrate_kbps: u32) -> anyhow::Result<String> {
    // ~2s at 30fps; on macOS the frame-count and duration caps back each
    // other up. `realtime` favors encode latency over compression, and
    // `allow-frame-reordering=false` disables B-frames — vtenc emits them
    // by default, and RTP receivers shouldn't pay the reorder latency for
    // a live stream.
    #[cfg(target_os = "macos")]
    {
        let name = find(&["vtenc_h264_hw", "vtenc_h264"])?;
        Ok(format!(
            "{name} realtime=true allow-frame-reordering=false bitrate={bitrate_kbps} \
             max-keyframe-interval=60 max-keyframe-interval-duration=2000000000"
        ))
    }
    #[cfg(target_os = "windows")]
    {
        let name = find(&["nvh264enc", "qsvh264enc", "amfh264enc", "mfh264enc"])?;
        Ok(format!("{name} gop-size=60 bitrate={bitrate_kbps}"))
    }
    #[cfg(target_os = "linux")]
    {
        let name = find(&["vah264enc", "vaapih264enc"])?;
        Ok(match name {
            "vah264enc" => format!("{name} key-int-max=60 bitrate={bitrate_kbps}"),
            _ => format!("{name} keyframe-period=60 bitrate={bitrate_kbps}"),
        })
    }
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
