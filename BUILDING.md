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
