# Building corcel

## Linux

Runtime/build dependencies: GStreamer (core + good/bad plugins, the
PipeWire element, and the VAAPI `va` plugin — see PROJECT.md decision 3,
hardware codecs are mandatory) and a desktop with the XDG ScreenCast
portal for screen sharing.

```sh
cargo build --release
./target/release/corcel
```

## macOS

The media layer maps onto Apple's stack automatically at compile time:
AVFoundation for camera and screen capture, CoreAudio for mic/speakers,
VideoToolbox for hardware H264 — all through the same GStreamer pipelines.

One-time setup (needs [Homebrew](https://brew.sh) and
[rustup](https://rustup.rs), plus the Xcode command-line tools):

```sh
xcode-select --install   # if you don't have the CLT yet
brew install gstreamer pkg-config
```

Homebrew's monolithic `gstreamer` formula bundles every plugin set,
including `applemedia` (avfvideosrc/vtenc/vtdec) and `osxaudio`.

Build and run:

```sh
cargo build --release
./target/release/corcel
```

Things to know on macOS:

- **Permissions**: the first mic/camera use pops the system permission
  prompt for the app that launched corcel (Terminal, usually). Screen
  sharing needs *Screen Recording* permission granted in System Settings →
  Privacy & Security, then a relaunch.
- **Screen share picks the main display** — macOS has no portal-style
  source picker wired up yet, so it shares the primary screen, not a
  chosen window.
- Config and the message database live in `~/.config/corcel`, same layout
  as Linux.

If `cargo build` can't find GStreamer, Homebrew's pkg-config dir isn't on
the search path — export it and retry:

```sh
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:$PKG_CONFIG_PATH"
```

## Windows

Port status: compiles under CI (see `.github/workflows/ci.yml`); the
media element arms are being brought up per PLAN-windows.md. Windows 10
1903+ / Windows 11, x86_64, **MSVC toolchain only** — GStreamer's
official Windows binaries are MSVC, so the GNU toolchain can't link
against them.

One-time setup:

1. [rustup](https://rustup.rs) with the default `x86_64-pc-windows-msvc`
   toolchain, plus Visual Studio 2022 Build Tools (the "Desktop
   development with C++" workload).
2. GStreamer's official **MSVC 1.26.x** packages from
   [gstreamer.freedesktop.org/download](https://gstreamer.freedesktop.org/download/)
   — both the *runtime* and *development* installers, and pick the
   **Complete** install so the `wasapi2`, `mediafoundation`, `d3d11`,
   `nvcodec`, `qsv`, and `amfcodec` plugin sets are present.
   (Equivalent via chocolatey, which is what CI uses:
   `choco install gstreamer gstreamer-devel pkgconfiglite`.)
3. A `pkg-config.exe` on PATH — `choco install pkgconfiglite` is the
   easiest; the GStreamer dev package ships the `.pc` files but not the
   tool itself.

Then, in the shell you build from (PowerShell):

```powershell
$env:PKG_CONFIG_PATH = "C:\gstreamer\1.0\msvc_x86_64\lib\pkgconfig"
$env:PATH = "C:\gstreamer\1.0\msvc_x86_64\bin;$env:PATH"
cargo build --release
.\target\release\corcel.exe
```

Things to know on Windows:

- Config and the message database live in `%APPDATA%\corcel`.
- **Permissions**: Settings → Privacy & Security → Microphone/Camera has
  a global "let desktop apps access" switch — if capture pipelines fail
  instantly, check there first.
- **Firewall**: first launch pops the Windows Defender prompt for
  corcel's UDP socket. Hosts (especially private-network/`LocalNetwork`
  servers) must allow it, or nobody can dial them.
- **Screen share** uses Windows Graphics Capture: primary monitor only
  for now (same simplification as macOS), and Windows 11 draws its
  yellow "being captured" border — that's the OS, not corcel.

## Windows installer (for people who just want to use corcel)

Nobody installing corcel needs any of the setup above — that's all
build-time. `packaging/windows/` produces a self-contained installer
(corcel.exe with the GStreamer runtime DLLs bundled beside it, Discord-
style):

```powershell
cargo build --release
powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1
iscc packaging\windows\corcel.iss   # -> dist\corcel-setup.exe
```

CI builds the same installer on demand (run the "CI" workflow manually
and grab the `corcel-setup` artifact) and attaches it to a GitHub
release on every `v*` tag. The only thing the installer can't do for a
user is accept the Windows firewall prompt on first launch.
