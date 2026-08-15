# corcel

## Vision

A Discord-shaped voice/video app with no central server. Every "server" is
hosted by one of its members' own machines instead of rented infrastructure —
the host *is* the server for that session. Long-term this extends into text
channels, persistent profiles, and torrent-style state sync between peers
(no single host required to keep history alive). This phase builds the
foundation that everything else sits on: the call engine, and the
server/channel shell around it.

## Phase 1 Scope

- Rust project, UI built with [GPUI](https://www.gpui.rs/) (Zed's UI framework).
- **Linux only**, targeting GNOME on Wayland (the current dev machine's
  actual environment).
- A server/channel shell: create or join a server via a link, see its list
  of voice channels, join a channel to enter a live call. No text channels
  yet.
- Group voice + video + screen share, relayed through the channel's host —
  not full mesh.
- No accounts, no persistence across devices, no DHT, no chat. No visual
  design pass yet (Apple "liquid glass" is a deliberately deferred phase).

Definition of done: two or more people can join a server, enter a voice
channel via a shared link, and hold a live call with audio, video, and
screen share, relayed through the host, running reliably on Linux/GNOME/Wayland.

## Architecture Decisions

1. **Signaling — ephemeral `wss://` relay.** A minimal, stateless WebSocket
   relay carries only the SDP/ICE handshake for a few hundred ms at call
   setup. It never touches live media, and TLS costs nothing here since it's
   not in the real-time path — it just closes off a MITM risk on the one
   part of the system that WebRTC's own DTLS doesn't already protect.

2. **Screen capture — Wayland ScreenCast via PipeWire + `xdg-desktop-portal`.**
   Matches the current machine (GNOME/Wayland). GNOME's portal
   implementation is one of the more reliable ones, unlike some
   wlroots-based compositors. X11 is out of scope.

3. **Media stack — `webrtc-rs` + GStreamer/VAAPI.** `webrtc-rs` owns the
   P2P protocol layer (ICE, DTLS, SRTP, data channels) in idiomatic Rust.
   GStreamer (`gstreamer-rs`) owns capture and hardware-accelerated
   encode/decode via VAAPI, feeding encoded frames into `webrtc-rs`'s RTP
   track. Chosen over a pure-Rust or all-GStreamer stack to get hardware
   encoding for screen share (the CPU-heaviest part) while keeping the
   protocol logic in Rust we own directly.

4. **Video codec — H264 only.** The one codec with broad, mature VAAPI
   hardware encode/decode support across Intel/AMD/NVIDIA on Linux. VP8/VP9
   hardware *encode* support is patchy enough to silently fall back to
   software, undermining the point of picking GStreamer/VAAPI at all.

5. **NAT traversal — STUN only for now.** TURN relay fallback is deferred:
   unlike everything else here, it's an ongoing bandwidth-cost commitment
   rather than a one-time build task, so it's a deliberate later decision.
   Known limitation: calls between peers on hard/symmetric NATs may fail to
   connect in this phase.

6. **Call topology — host-relay (SFU-lite), not mesh.** Each participant
   uploads their stream once, to the channel's host; the host forwards
   already-encoded packets to everyone else (no transcoding). Full mesh was
   rejected because upload bandwidth scales linearly with participant count
   for *every* participant, capping out around 4-5 people even for
   audio-only. Trade-off: the host needs more upload bandwidth than other
   participants, and the channel drops if the host disconnects — there is
   no failover in this phase (elected/rotating relay is a later hardening
   step).

7. **GPUI video rendering — zero-copy texture import, built in two stages.**
   Target: hardware-decoded frames (GPU textures via DMA-BUF) imported
   directly into GPUI's renderer, avoiding a CPU readback+upload per frame.
   GPUI's Linux backend renders via `wgpu` (migrated off `blade` in Feb
   2026), and `wgpu-graft` exists specifically for importing externally
   created GPU textures (Vulkan images, incl. on Linux) into a wgpu texture
   — so the mechanics are real. But GPUI itself exposes no public API for
   injecting an external texture into its render tree; the only existing
   video-in-GPUI project (`gpui-video-player`, GStreamer-backed) does CPU
   readback through GPUI's sprite atlas instead. So true zero-copy will
   require patching/extending GPUI's renderer internals, not just app-level
   code. Given that, build order is: (1) get the call pipeline working
   end-to-end with CPU-readback rendering (same approach as
   `gpui-video-player`) first, (2) attempt the zero-copy texture-import
   patch as an isolated follow-up once the rest of the pipeline is proven.

8. **Server/channel shell built now, not deferred.** Even though each
   voice channel currently maps to a single live call session underneath,
   the server → channel-list → join-channel structure is built as part of
   this phase, not bolted on after.

## Explicitly Out of Scope (Phase 1)

- Text chat / chat rooms
- User profiles, accounts, persistent identity
- Cross-device sync, DHT-based discovery, torrent-style state sync
- TURN relay fallback
- macOS / Windows support
- X11 support
- Full mesh or elected-relay call topology
- Liquid glass / Apple visual design system
