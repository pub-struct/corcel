//! Voice and video: entering/leaving voice channels, mute/deafen/PTT,
//! camera and screen-share pipelines, the speaking detector broadcast, and
//! the voice panel (stage + call bar) that renders a call.

use super::*;

/// The V4L2 device [`Shell::toggle_camera_clicked`] captures from. No device
/// picker yet — first-pass wiring just needs one working camera.
const CAMERA_DEVICE: &str = "/dev/video0";

impl Shell {
    pub(super) fn enter_voice_channel(&mut self, channel: ChannelInfo, cx: &mut Context<Self>) {
        let Screen::Server { id, .. } = &self.screen else { return };
        let server_id = *id;

        if let Some(call) = &self.call {
            // Clicking the channel you're already in just brings its stage
            // back on screen — it must not rejoin.
            if call.server_id == server_id && call.channel.id == channel.id {
                self.screen = Screen::Server { id: server_id, view: ServerView::Voice { channel } };
                cx.notify();
                return;
            }
            // Switching calls: tear the old one down first (and clear our
            // speaking ring there — its watcher dies with it).
            if let ChannelStatus::Connected(connected) = &call.status {
                hang_up(connected);
                let (old_server, old_channel) = (call.server_id, call.channel.id);
                let author = self.my_name();
                self.send_room(
                    old_server,
                    ChatPayload::Speaking { channel: old_channel, author, speaking: false },
                    None,
                );
            }
        }
        let Some(server) = self.servers.iter().find(|s| s.link.id == server_id) else { return };
        let link = server.link.clone();
        let is_host = server.is_host;

        self.call = Some(ActiveCall { server_id, channel: channel.clone(), status: ChannelStatus::Connecting });
        self.screen = Screen::Server { id: server_id, view: ServerView::Voice { channel: channel.clone() } };
        cx.notify();

        let rx = if is_host {
            runtime::spawn_and_send(session::join_as_host(link, channel.id))
        } else {
            runtime::spawn_and_send(session::join(link, channel.id))
        };
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let session = match result {
                Ok(Ok(session)) => session,
                Ok(Err(err)) => {
                    let _ = this.update(cx, |shell, cx| {
                        if let Some(call) = shell.call.as_mut().filter(|call| call.channel.id == channel.id) {
                            call.status = ChannelStatus::Failed(format!("{err:#}"));
                            cx.notify();
                        }
                    });
                    return;
                }
                Err(_) => {
                    let _ = this.update(cx, |shell, cx| {
                        if let Some(call) = shell.call.as_mut().filter(|call| call.channel.id == channel.id) {
                            call.status = ChannelStatus::Failed("join task was dropped".to_string());
                            cx.notify();
                        }
                    });
                    return;
                }
            };

            let CallSession { pc, mut remote_video, hang_up: hang_up_tx, mute, deafen, speaking, local_video } =
                session;
            let channel_id = channel.id;
            let surface = this.update(cx, |shell, cx| {
                let profile = shell.profile.as_ref();
                let surface = cx.new(|_| VideoSurface {
                    frame: None,
                    name: profile.map(|p| p.name.clone()).unwrap_or_default().into(),
                    avatar: profile.and_then(|p| p.avatar_path.clone()),
                    initial: profile.map(|p| p.initial()).unwrap_or_else(|| "?".to_string()).into(),
                    speaking: false,
                });
                let call = ConnectedCall {
                    pc,
                    remote_surface: surface.clone(),
                    sharing: SharingState::Idle,
                    camera: CameraState::Idle,
                    muted: false,
                    mute,
                    deafened: false,
                    deafen,
                    muted_before_deafen: false,
                    ptt_enabled: false,
                    self_speaking: false,
                    speaking_rx: speaking.clone(),
                    hang_up: hang_up_tx,
                    local_video,
                };
                if let Some(active) = shell.call.as_mut().filter(|active| active.channel.id == channel_id) {
                    active.status = ChannelStatus::Connected(call);
                    shell.spawn_speaking_watcher(channel_id, speaking, cx);
                    cx.notify();
                    return Some(surface);
                }
                // The user already left / switched channels while the join
                // was in flight — tear the fresh session down instead of
                // leaking its media tasks.
                hang_up(&call);
                None
            });
            let Ok(Some(surface)) = surface else { return };

            // Pump decoded frames into the video surface only — the rest of
            // the shell doesn't re-render for them. Dropping the *previous*
            // frame's texture explicitly matters: `RenderImage`s are keyed
            // into GPUI's sprite atlas by id and are never evicted on their
            // own, so without this every frame of a call permanently leaks
            // a full-resolution GPU texture.
            while let Some(frame) = remote_video.recv().await {
                let Some(buffer) = image::RgbaImage::from_raw(frame.width, frame.height, frame.data) else {
                    continue;
                };
                let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
                let updated = surface.update(cx, |surface, cx| {
                    if let Some(old) = surface.frame.replace(image) {
                        cx.drop_image(old, None);
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn leave_channel_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(call) = self.call.take() else { return };
        if let ChannelStatus::Connected(connected) = &call.status {
            hang_up(connected);
            // The speaking watcher dies with the call and can't say this
            // for us — clear our ring for everyone explicitly rather than
            // leaving it to their expiry timer.
            let author = self.my_name();
            self.send_room(
                call.server_id,
                ChatPayload::Speaking { channel: call.channel.id, author, speaking: false },
                None,
            );
        }
        // If that channel's stage was on screen, fall back to the lobby.
        if let Screen::Server { id, view: ServerView::Voice { channel } } = &self.screen {
            if channel.id == call.channel.id {
                self.screen = Screen::Server { id: *id, view: ServerView::Lobby };
            }
        }
        cx.notify();
    }

    /// Re-attempts a join that landed in `ChannelStatus::Failed` — re-runs
    /// the whole signaling + WebRTC handshake for the same channel.
    pub(super) fn retry_channel_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(call) = &self.call else { return };
        if !matches!(call.status, ChannelStatus::Failed(_)) {
            return;
        }
        let (server_id, channel) = (call.server_id, call.channel.clone());
        self.call = None;
        if !matches!(&self.screen, Screen::Server { id, .. } if *id == server_id) {
            self.screen = Screen::Server { id: server_id, view: ServerView::Lobby };
        }
        self.enter_voice_channel(channel, cx);
    }

    pub(super) fn connected_call_mut(&mut self) -> Option<&mut ConnectedCall> {
        match self.call.as_mut() {
            Some(ActiveCall { status: ChannelStatus::Connected(call), .. }) => Some(call),
            _ => None,
        }
    }

    /// Bridges the mic's speech detector to the rest of the app. A
    /// tokio-side task turns the watch channel into events — every
    /// transition, plus a `true` refresh every 2 seconds while hot, so
    /// receivers' [`SPEAKING_EXPIRY`] never fires mid-sentence — and a
    /// GPUI-side task applies each event: own ring state, the stage tile,
    /// and the room broadcast (gated to `false` while muted, so mute means
    /// "nothing about my mic leaves this machine", not just no audio).
    pub(super) fn spawn_speaking_watcher(&self, channel_id: Uuid, mut detector: watch::Receiver<bool>, cx: &mut Context<Self>) {
        // The tokio half exists because racing a timer against the watch
        // channel needs a real timer driver, which GPUI's executor lacks.
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        drop(runtime::spawn_and_send(async move {
            loop {
                let speaking = *detector.borrow();
                if event_tx.send(speaking).is_err() {
                    return;
                }
                if speaking {
                    tokio::select! {
                        changed = detector.changed() => { if changed.is_err() { break; } }
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                } else if detector.changed().await.is_err() {
                    break;
                }
            }
            // The detector's gone (mic torn down) — the last word is "off".
            let _ = event_tx.send(false);
        }));

        cx.spawn(async move |this, cx| {
            // Skips re-broadcasting `false` while muted-and-talking; only
            // `true` needs the periodic refresh.
            let mut last_sent = false;
            while let Some(speaking) = event_rx.recv().await {
                let alive = this.update(cx, |shell, cx| {
                    let Some(active) = shell.call.as_mut().filter(|active| active.channel.id == channel_id)
                    else {
                        return false;
                    };
                    let server_id = active.server_id;
                    let ChannelStatus::Connected(call) = &mut active.status else { return false };
                    let audible = speaking && !call.muted;
                    if call.self_speaking != audible {
                        call.self_speaking = audible;
                        call.remote_surface.update(cx, |surface, cx| {
                            surface.speaking = audible;
                            cx.notify();
                        });
                        cx.notify();
                    }
                    if audible || last_sent {
                        let author = shell.my_name();
                        shell.send_room(
                            server_id,
                            ChatPayload::Speaking { channel: channel_id, author, speaking: audible },
                            None,
                        );
                        last_sent = audible;
                    }
                    true
                });
                if !alive.unwrap_or(false) {
                    return;
                }
            }
        })
        .detach();
    }

    /// The one place mute actually changes: flag, media-task switch, own
    /// ring, and the room's view of it move together — every path that
    /// flips mute (the buttons, deafen's implied mute, push-to-talk's
    /// press/release) funnels through here so none of them can desync.
    pub(super) fn set_muted(&mut self, muted: bool, cx: &mut Context<Self>) {
        let Some(call) = self.connected_call_mut() else { return };
        if call.muted == muted {
            return;
        }
        call.muted = muted;
        let _ = call.mute.send(muted);
        // Sync ring read: unmuting mid-sentence lights up immediately
        // instead of waiting for the detector's next off→on edge.
        let audible = !muted && *call.speaking_rx.borrow();
        call.self_speaking = audible;
        let surface = call.remote_surface.clone();
        surface.update(cx, |surface, cx| {
            surface.speaking = audible;
            cx.notify();
        });
        let Some(active) = self.call.as_ref() else { return };
        let (server_id, channel) = (active.server_id, active.channel.id);
        let author = self.my_name();
        self.send_room(server_id, ChatPayload::Speaking { channel, author, speaking: audible }, None);
        cx.notify();
    }

    pub(super) fn toggle_mute_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(call) = self.connected_call_mut() else { return };
        let muted = !call.muted;
        self.set_muted(muted, cx);
    }

    /// Deafen implies mute (you shouldn't be audible while you can't hear
    /// anyone); undeafen restores whatever mute state preceded it.
    pub(super) fn toggle_deafen_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(call) = self.connected_call_mut() else { return };
        if call.deafened {
            call.deafened = false;
            let _ = call.deafen.send(false);
            let restore = call.muted_before_deafen;
            self.set_muted(restore, cx);
        } else {
            call.deafened = true;
            let _ = call.deafen.send(true);
            call.muted_before_deafen = call.muted;
            self.set_muted(true, cx);
        }
        // `set_muted` no-ops (including its notify) when mute didn't
        // actually change, but the deafen flag always did.
        cx.notify();
    }

    /// Push-to-talk on ⇒ the mic idles muted and Space unmutes while held
    /// (see [`Shell::root_key_down`]/[`Shell::root_key_up`]); off ⇒ back to
    /// open mic. In-window only — global hotkeys are OS-specific and out of
    /// scope, which the toggle's tooltip says honestly.
    pub(super) fn toggle_ptt_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(call) = self.connected_call_mut() else { return };
        call.ptt_enabled = !call.ptt_enabled;
        let idle_muted = call.ptt_enabled;
        self.set_muted(idle_muted, cx);
        cx.notify();
    }

    pub(super) fn share_screen_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.call.as_mut() else { return };
        let channel_id = active.channel.id;
        let ChannelStatus::Connected(call) = &mut active.status else { return };
        if !matches!(call.sharing, SharingState::Idle) {
            return;
        }
        // Flips synchronously, before the portal round trip even starts —
        // closes the window where an impatient second click (the picker
        // dialog can take a while) would otherwise race a second capture
        // pipeline into existence alongside the first.
        call.sharing = SharingState::Pending;
        cx.notify();

        let pc = call.pc.clone();
        let preview = call.local_video.clone();

        let rx = runtime::spawn_and_send(session::start_screen_share(pc, preview));
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |shell, cx| {
                let Some(active) = shell.call.as_mut().filter(|call| call.channel.id == channel_id) else {
                    return;
                };
                let ChannelStatus::Connected(call) = &mut active.status else { return };
                match result {
                    Ok(Ok(handle)) => call.sharing = SharingState::Active(handle),
                    Ok(Err(err)) => {
                        call.sharing = SharingState::Idle;
                        shell.error = Some(format!("failed to share screen: {err:#}"));
                    }
                    Err(_) => {
                        call.sharing = SharingState::Idle;
                        shell.error = Some("screen share task was dropped".to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn stop_sharing_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(call) = self.connected_call_mut() {
            if let SharingState::Active(handle) = std::mem::replace(&mut call.sharing, SharingState::Idle) {
                handle.stop();
                let surface = call.remote_surface.clone();
                clear_stage(&surface, cx);
                cx.notify();
            }
        }
    }

    pub(super) fn toggle_camera_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.call.as_mut() else { return };
        let channel_id = active.channel.id;
        let ChannelStatus::Connected(call) = &mut active.status else { return };
        if !matches!(call.camera, CameraState::Idle) {
            return;
        }
        // Same reasoning as share_screen_clicked: flip synchronously so a
        // second click before this resolves can't start a second capture.
        call.camera = CameraState::Pending;
        cx.notify();

        let pc = call.pc.clone();
        let preview = call.local_video.clone();

        let rx = runtime::spawn_and_send(session::start_camera(pc, CAMERA_DEVICE, preview));
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |shell, cx| {
                let Some(active) = shell.call.as_mut().filter(|call| call.channel.id == channel_id) else {
                    return;
                };
                let ChannelStatus::Connected(call) = &mut active.status else { return };
                match result {
                    Ok(Ok(handle)) => call.camera = CameraState::Active(handle),
                    Ok(Err(err)) => {
                        call.camera = CameraState::Idle;
                        shell.error = Some(format!("failed to start camera: {err:#}"));
                    }
                    Err(_) => {
                        call.camera = CameraState::Idle;
                        shell.error = Some("camera task was dropped".to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn stop_camera_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(call) = self.connected_call_mut() {
            if let CameraState::Active(handle) = std::mem::replace(&mut call.camera, CameraState::Idle) {
                handle.stop();
                let surface = call.remote_surface.clone();
                clear_stage(&surface, cx);
                cx.notify();
            }
        }
    }

    /// A voice channel's stage — driven by [`Shell::call`] when it matches
    /// this channel; a plain join prompt otherwise (e.g. after the call was
    /// disconnected from elsewhere while this view stayed up).
    pub(super) fn render_voice_panel(&mut self, profile: &Profile, channel: &ChannelInfo, cx: &mut Context<Self>) -> Div {
        let status = self
            .call
            .as_ref()
            .filter(|call| call.channel.id == channel.id)
            .map(|call| &call.status);

        let pill_el = match status {
            Some(ChannelStatus::Connecting) => theme::pill(theme::PillVariant::Connecting, "Connecting…"),
            Some(ChannelStatus::Connected(_)) => theme::pill(theme::PillVariant::Live, "Voice connected"),
            Some(ChannelStatus::Failed(_)) => theme::pill(theme::PillVariant::Failed, "Connection failed"),
            None => theme::pill(theme::PillVariant::Connecting, "Not connected"),
        };

        let header = div()
            .h(px(48.))
            .flex_none()
            .px(px(12.))
            .border_b_1()
            .border_color(theme::border())
            .flex()
            .items_center()
            .gap(px(8.))
            .child(theme::icon(icons::VOLUME, px(18.)).text_color(theme::muted_foreground()))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(15.))
                    .child(channel.name.clone()),
            )
            .child(div().flex_1())
            .child(pill_el);

        let stage_content = match status {
            Some(ChannelStatus::Connecting) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    stage_tile(
                        profile.avatar_path.clone(),
                        profile.initial(),
                        profile.name.clone(),
                        "Connecting to voice…",
                        false,
                    )
                    .with_animation(
                        "connecting-pulse",
                        Animation::new(Duration::from_millis(1400)).repeat(),
                        |tile, delta| {
                            let t = 1. - (2. * delta - 1.).abs();
                            tile.opacity(0.45 + 0.55 * t)
                        },
                    ),
                )
                .into_any_element(),

            Some(ChannelStatus::Connected(call)) => call.remote_surface.clone().into_any_element(),

            Some(ChannelStatus::Failed(err)) => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .child(
                    div()
                        .size(px(56.))
                        .rounded_full()
                        .bg(theme::card())
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(theme::icon(icons::X, px(26.)).text_color(theme::destructive_foreground())),
                )
                .child(div().text_size(px(15.)).font_weight(FontWeight::BOLD).child("Couldn't connect"))
                .child(
                    div()
                        .max_w(px(420.))
                        .text_size(px(12.5))
                        .text_color(theme::muted_foreground())
                        .text_center()
                        .child(err.clone()),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .mt(px(6.))
                        .child(
                            theme::button(ButtonVariant::Primary, "Try Again")
                                .on_mouse_up(MouseButton::Left, cx.listener(Self::retry_channel_clicked)),
                        )
                        .child(
                            theme::button(ButtonVariant::Ghost, "Back to Channels")
                                .on_mouse_up(MouseButton::Left, cx.listener(Self::leave_channel_clicked)),
                        ),
                )
                .into_any_element(),

            None => {
                let channel_for_click = channel.clone();
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(10.))
                    .child(div().text_size(px(15.)).font_weight(FontWeight::BOLD).child("Not connected"))
                    .child(
                        theme::button(ButtonVariant::Primary, "Join Voice").on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |shell, _, _window, cx| {
                                shell.enter_voice_channel(channel_for_click.clone(), cx);
                            }),
                        ),
                    )
                    .into_any_element()
            }
        };

        let control_bar = match status {
            Some(ChannelStatus::Connected(call)) => {
                let muted = call.muted;
                let deafened = call.deafened;
                let ptt_enabled = call.ptt_enabled;
                let (sharing_active, sharing_pending) = match call.sharing {
                    SharingState::Idle => (false, false),
                    SharingState::Pending => (false, true),
                    SharingState::Active(_) => (true, false),
                };
                let (camera_active, camera_pending) = match call.camera {
                    CameraState::Idle => (false, false),
                    CameraState::Pending => (false, true),
                    CameraState::Active(_) => (true, false),
                };

                let mute_btn = call_button(
                    "call-mute",
                    if muted { icons::MIC_OFF } else { icons::MIC },
                    if muted { "Unmute" } else { "Mute" },
                    if muted { theme::destructive() } else { theme::wash_strong() },
                    false,
                )
                .on_mouse_up(MouseButton::Left, cx.listener(Self::toggle_mute_clicked));

                let deafen_btn = call_button(
                    "call-deafen",
                    if deafened { icons::HEADPHONE_OFF } else { icons::HEADPHONES },
                    if deafened { "Undeafen" } else { "Deafen" },
                    if deafened { theme::destructive() } else { theme::wash_strong() },
                    false,
                )
                .on_mouse_up(MouseButton::Left, cx.listener(Self::toggle_deafen_clicked));

                // Push-to-talk is a mode, not a momentary action, so it gets a
                // text pill instead of an icon: "PTT" lit primary while armed.
                // Honest tooltip — the Space hold only works while the corcel
                // window itself has keyboard focus.
                let ptt_btn = div()
                    .id("call-ptt")
                    .h(px(44.))
                    .px(px(16.))
                    .rounded_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if ptt_enabled { theme::primary() } else { theme::wash_strong() })
                    .text_color(theme::primary_foreground())
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.9))
                    .active(|style| style.opacity(0.8))
                    .tooltip(theme::tooltip(if ptt_enabled {
                        "Push to talk is on: hold Space to speak (while the corcel window is focused). Click to switch back to open mic."
                    } else {
                        "Push to talk: mute the mic and hold Space to speak — works while the corcel window is focused."
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::toggle_ptt_clicked))
                    .child("PTT");

                let camera_btn = call_button(
                    "call-camera",
                    if camera_active { icons::VIDEO } else { icons::VIDEO_OFF },
                    if camera_active {
                        "Turn off camera"
                    } else if camera_pending {
                        "Starting camera…"
                    } else {
                        "Turn on camera"
                    },
                    if camera_active { theme::success() } else { theme::wash_strong() },
                    camera_pending,
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |shell, event, window, cx| {
                        if camera_active {
                            shell.stop_camera_clicked(event, window, cx);
                        } else if !camera_pending {
                            shell.toggle_camera_clicked(event, window, cx);
                        }
                    }),
                );

                let share_btn = call_button(
                    "call-share",
                    icons::MONITOR_UP,
                    if sharing_active {
                        "Stop sharing"
                    } else if sharing_pending {
                        "Waiting for the screen picker…"
                    } else {
                        "Share your screen"
                    },
                    if sharing_active { theme::info() } else { theme::wash_strong() },
                    sharing_pending,
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |shell, event, window, cx| {
                        if sharing_active {
                            shell.stop_sharing_clicked(event, window, cx);
                        } else if !sharing_pending {
                            shell.share_screen_clicked(event, window, cx);
                        }
                    }),
                );

                // Hang-up: the one destructive control — biggest target in
                // the bar, red, and separated by a divider so muscle memory
                // can't confuse it with a toggle.
                let leave_btn = div()
                    .id("call-leave")
                    .h(px(44.))
                    .w(px(64.))
                    .rounded_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::destructive())
                    .text_color(theme::primary_foreground())
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.9))
                    .active(|style| style.opacity(0.8))
                    .tooltip(theme::tooltip("Leave channel"))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::leave_channel_clicked))
                    .child(
                        theme::icon(icons::PHONE, px(20.))
                            .with_transformation(Transformation::rotate(radians(3.926))),
                    );

                Some(
                    div()
                        .absolute()
                        .bottom(px(20.))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .p(px(10.))
                                .rounded_full()
                                .bg(theme::popover())
                                .border_1()
                                .border_color(theme::border())
                                .shadow_lg()
                                .child(mute_btn)
                                .child(deafen_btn)
                                .child(camera_btn)
                                .child(share_btn)
                                .child(ptt_btn)
                                .child(div().w(px(1.)).h(px(28.)).bg(theme::input_border()))
                                .child(leave_btn),
                        ),
                )
            }
            _ => None,
        };

        let stage = div()
            .flex_1()
            .min_h_0()
            .relative()
            .bg(theme::rail())
            .child(stage_content)
            .children(control_bar);

        div().flex_1().min_w_0().h_full().flex().flex_col().child(header).child(stage)
    }

}

/// A voice participant's avatar with the green "I'm audible" ring. The
/// ring is always drawn — transparent when quiet — so speech starting and
/// stopping never shifts the layout around it.
pub(super) fn speaking_avatar(avatar: Option<PathBuf>, initial: SharedString, diameter: f32, speaking: bool) -> Div {
    div()
        .rounded_full()
        .flex_none()
        .border_2()
        .border_color(if speaking { theme::success().into() } else { gpui::transparent_black() })
        .child(theme::avatar(avatar, initial, px(diameter)))
}

/// A participant tile for the stage's audio-only states: avatar (ringed
/// while speaking), name, and a one-line caption saying what's (not)
/// happening.
pub(super) fn stage_tile(
    avatar: Option<PathBuf>,
    initial: impl Into<SharedString>,
    name: impl Into<SharedString>,
    caption: impl Into<SharedString>,
    speaking: bool,
) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(12.))
        .child(speaking_avatar(avatar, initial.into(), 80., speaking))
        .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).text_color(theme::foreground()).child(name.into()))
        .child(div().text_size(px(12.5)).text_color(theme::muted_foreground()).child(caption.into()))
}

/// A round 44px call-control button for the in-call bar. `pending` renders
/// it dimmed and inert (the state between clicking and the async start
/// resolving) so a click visibly *did something* even before the pipeline
/// is up. Callers attach `.on_mouse_up(...)`.
pub(super) fn call_button(
    id: &'static str,
    icon_path: &'static str,
    label: &'static str,
    background: Rgba,
    pending: bool,
) -> Stateful<Div> {
    let button = div()
        .id(id)
        .size(px(44.))
        .rounded_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .bg(background)
        .child(theme::icon(icon_path, px(20.)).text_color(theme::primary_foreground()));

    if pending {
        button.opacity(0.5).tooltip(theme::tooltip(label))
    } else {
        button
            .cursor_pointer()
            .hover(|style| style.opacity(0.9))
            .active(|style| style.opacity(0.75))
            .tooltip(theme::tooltip(label))
    }
}
