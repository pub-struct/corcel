# Windows build plan

Goal: `corcel.exe` on Windows 10 (1903+) / Windows 11, x86_64 MSVC, with
chat, voice, camera, and screen share working — the same feature set the
macOS port (6c1b4cc) reached, by the same route: everything
platform-specific already funnels through a handful of `#[cfg]` points in
`corcel-media` plus one config-dir function, so the port is mostly
choosing the right GStreamer elements.

## What already works untouched

- **Networking** — iroh, tokio, rustls: fully cross-platform. Chat,
  presence, invite links, and the whole call *transport* (RTP over QUIC
  datagrams) should work the moment the workspace compiles.
- **Storage** — rusqlite is `bundled` (SQLite compiled in), serde, uuid:
  nothing to do.
- **UI** — GPUI 0.2 supports Windows (DirectX-based renderer, the same
  one Zed ships on). corcel uses stock GPUI APIs only. *This is the
  first thing to verify on a real machine — if the published crate has
  Windows gaps, everything else waits on it.*

## The porting surface (mirrors the macOS `#[cfg]` blocks)

| Concern | Linux | macOS | Windows (planned) |
|---|---|---|---|
| Mic source | `pipewiresrc` | `osxaudiosrc` | `wasapi2src` (fallback `wasapisrc`) |
| Audio sink | `pipewiresink` | `osxaudiosink` | `wasapi2sink` (fallback `wasapisink`) |
| Camera | `v4l2src device=…` | `avfvideosrc` (device arg ignored) | `mfvideosrc` (Media Foundation; device arg ignored, like macOS) |
| Screen capture | ScreenCast portal + `pipewiresrc` | `avfvideosrc capture-screen=true` | `d3d11screencapturesrc` (Windows Graphics Capture; `show-cursor=true`, primary monitor first pass — no portal-style picker, same simplification as macOS) |
| H264 encode | `vah264enc` | `vtenc_h264_hw` | candidates in order: `nvh264enc`, `qsvh264enc`, `amfh264enc`, `mfh264enc` — vendor blocks first, Media Foundation as the always-present catch-all (MF itself fronts vendor hardware, so "hardware or bust" still holds) |
| H264 decode | `vah264dec` | `vtdec_hw` | `d3d11h264dec`, then `nvh264dec`/`qsvh264dec`, then `mfh264dec` |
| Video postproc | `vapostproc` (DMA-BUF import) | `videoconvert ! videoscale` | `videoconvert ! videoscale` first pass (no DMA-BUF concept; revisit `d3d11convert` if CPU convert shows up in profiles) |
| Config dir | `$XDG_CONFIG_HOME/corcel` | same | `%APPDATA%\corcel` (keep the `XDG_CONFIG_HOME` override for tests) |

Code changes, concretely:

1. **`corcel-media/src/codec.rs`** — add `target_os = "windows"` arms to
   `h264_encoder` / `h264_decoder` / `video_postproc`. While in there,
   flip the existing `#[cfg(not(target_os = "macos"))] → Linux` branches
   to explicit per-OS arms: today an unknown OS silently gets
   `pipewiresrc`/`vah264enc`, which turns a missing port into a runtime
   pipeline error instead of a compile error.
2. **`corcel-media/src/capture.rs`** — `AUDIO_SOURCE` arm, camera source
   arm, and a `#[cfg(target_os = "windows")] pub async fn screen()`
   (plain pipeline, no portal, no `on_close` — the WGC session dies with
   the pipeline).
3. **`corcel-media/src/playback.rs`** — `AUDIO_SINK` arm.
4. **`corcel-app/src/profile.rs::config_dir`** — `APPDATA` branch
   (Windows has no `$HOME`; today it would land in `./corcel` next to
   the exe).
5. **`corcel-app/src/shell/call.rs`** — nothing: `CAMERA_DEVICE` is
   passed but ignored on non-Linux, same as macOS.

Nothing in `corcel-signal`/`corcel-net` needs touching.

## Toolchain & dependencies

- Rust: `x86_64-pc-windows-msvc` via rustup, VS 2022 Build Tools
  (Desktop C++ workload). GNU toolchain is out — GStreamer's official
  Windows binaries are MSVC.
- GStreamer: the official **MSVC 1.26.x** installers from
  gstreamer.freedesktop.org — both the *runtime* and *development*
  packages, with the "complete" option so `wasapi2`, `mediafoundation`,
  `d3d11`, `nvcodec`, `qsv`, and `amfcodec` plugin sets land.
  `gstreamer-rs` finds it through the dev package's bundled `pkg-config`:
  set `PKG_CONFIG_PATH=%GSTREAMER_1_0_ROOT_MSVC_X86_64%\lib\pkgconfig`
  (and put `…\bin` on `PATH` for the DLLs at run time).
- Document all of it in a `## Windows` section of BUILDING.md, like the
  macOS one.

## Order of work

1. **Compile**: fix `config_dir`, sweep for unix-isms (the only known
   one, `AsRawFd`, is already Linux-gated), get `cargo check` green on a
   Windows machine with GStreamer dev installed. Verify GPUI actually
   builds and opens a window — the one real unknown.
2. **Chat milestone**: app boots, profile setup, create/join server,
   text chat. Zero media code involved — this should work as soon as it
   compiles, and proves iroh + GPUI + SQLite on Windows.
3. **Voice**: `wasapi2src`/`wasapi2sink` arms; call between Windows and
   Linux/macOS. Opus is pure software — no hardware variables here.
4. **Video**: decode first (receive a screen share from another OS —
   exercises `d3d11h264dec` + the existing CPU-readback render path),
   then `mfvideosrc` camera, then WGC screen capture, then encoder
   candidate ordering on real NVIDIA/Intel/AMD boxes.
5. **Packaging (separate, later)**: first pass ships "install the
   GStreamer runtime" as a prerequisite, exactly like macOS's brew
   instructions. A bundled installer (runtime DLLs next to the exe, or
   cargo-wix/Inno) is its own work item once the port is proven.
6. **CI guard**: a `windows-latest` GitHub Actions job that installs the
   GStreamer dev package (choco or the official installer in silent
   mode) and runs `cargo check`, so the port can't rot between releases.

## Windows-specific things to know (for BUILDING.md and the UI copy)

- Settings → Privacy & Security → Microphone/Camera can block desktop
  apps globally; first-run capture failures should point there.
- Windows Graphics Capture draws a yellow border around the captured
  monitor on Win 11 (system behavior, not removable) and needs
  Win 10 1903+ — that sets the app's minimum supported Windows.
- No screen/window picker in the first pass: primary monitor only, same
  as the macOS port's deliberate simplification. A picker (WGC supports
  per-window capture) is a follow-up.
- Firewall: first launch will pop the Defender inbound-rules prompt for
  the QUIC UDP socket; hosts on `LocalNetwork` reach need to accept it
  or nobody can dial them.
