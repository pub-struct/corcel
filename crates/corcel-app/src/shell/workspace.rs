//! The signed-in workspace chrome: the server rail, the channel sidebar
//! with the self footer, the main panel routing, invite-link copying,
//! leaving servers, and the app-shell composition of it all.

use super::*;

/// Fixed widths of the shell's chrome columns. Shared constants so nothing
/// (like the old call dock's `72 + 240 - 24` magic width) can silently desync
/// from the columns it's aligned against.
const RAIL_WIDTH: f32 = 60.;
const SIDEBAR_WIDTH: f32 = 288.;

impl Shell {
    pub(super) fn copy_link_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.active_server() else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(server.link.encode()));
        self.link_copied = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1600)).await;
            let _ = this.update(cx, |shell, cx| {
                shell.link_copied = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Leaves (removes) the active server: hangs up any call in it, stops
    /// its chat pump, and deletes the saved row *and its message history*
    /// (see [`store::Store::remove_server`]). A hosted server's relay keeps
    /// running until the app exits — other members stay connected; only
    /// this user's membership is forgotten.
    pub(super) fn leave_server_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Screen::Server { id, .. } = &self.screen else { return };
        let id = *id;
        if let Some(call) = &self.call {
            if call.server_id == id {
                if let ChannelStatus::Connected(connected) = &call.status {
                    hang_up(connected);
                }
                self.call = None;
            }
        }
        // Bumping the generation without restarting a loop is how a room
        // dies for good: the pump sees the mismatch and returns.
        *self.room_generations.entry(id).or_insert(0) += 1;
        self.rooms.remove(&id);
        self.chat_messages.clear();
        self.reset_composer_state();
        self.stop_video_embeds(cx);
        if let Some(store) = &self.store {
            let _ = store.remove_server(id);
        }
        self.servers.retain(|server| server.link.id != id);
        self.screen = Screen::Home;
        // If that was the last server, Home is the fullscreen arrival flow
        // again — start it back at the fork.
        self.add_server_stage = AddServerStage::Choice;
        cx.notify();
    }

    /// The server-icon rail: one bubble per saved server (initial + tooltip,
    /// with Discord's pill indicator — full height on the active one, a
    /// short hover hint on the rest) and the "+" action below a divider.
    pub(super) fn render_server_rail(&mut self, cx: &mut Context<Self>) -> Div {
        let active_id = match &self.screen {
            Screen::Server { id, .. } => Some(*id),
            _ => None,
        };

        let server_items = self
            .servers
            .iter()
            .map(|server| {
                let id = server.link.id;
                let is_active = active_id == Some(id);
                let initial = server
                    .link
                    .name
                    .trim()
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "?".to_string());
                let name: SharedString = server.link.name.clone().into();
                let group_name: SharedString = format!("rail-server-{id}").into();
                // Aggregate the server's channels into one rail indicator:
                // a red count for mentions, a plain dot for mere unread.
                let (unread_total, mention_total) =
                    server.link.channels.iter().fold((0u32, 0u32), |totals, channel| {
                        let (unread, mentions) = self.unread.get(&channel.id).copied().unwrap_or((0, 0));
                        (totals.0 + unread, totals.1 + mentions)
                    });

                let pill = if is_active {
                    div()
                        .absolute()
                        .left_0()
                        .top(px(4.))
                        .w(px(4.))
                        .h(px(40.))
                        .rounded_r_full()
                        .bg(theme::foreground())
                } else {
                    div()
                        .absolute()
                        .left_0()
                        .top(px(14.))
                        .w(px(4.))
                        .h(px(20.))
                        .rounded_r_full()
                        .bg(theme::foreground())
                        .invisible()
                        .group_hover(group_name.clone(), |style| style.visible())
                };

                div()
                    .w_full()
                    .relative()
                    .flex()
                    .justify_center()
                    .group(group_name)
                    .child(pill)
                    .child(
                        div()
                            .id(("rail-server", id.as_u128() as u64))
                            .size(px(44.))
                            .rounded(if is_active { px(14.) } else { px(22.) })
                            .bg(if is_active { theme::primary() } else { theme::card() })
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(18.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(if is_active { theme::primary_foreground() } else { theme::foreground() })
                            .cursor_pointer()
                            .hover(|style| {
                                style.rounded(px(16.)).bg(theme::primary()).text_color(theme::primary_foreground())
                            })
                            .active(|style| style.opacity(0.85))
                            .tooltip(theme::tooltip(name))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(move |shell, _, _window, cx| shell.open_server(id, cx)),
                            )
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |shell, event, _window, cx| {
                                    shell.open_context_menu(
                                        event,
                                        vec![
                                            ContextMenuItem::new(
                                                "Copy invite link",
                                                icons::LINK,
                                                ContextAction::CopyInvite { server: id },
                                            ),
                                            ContextMenuItem::new(
                                                "Mark all as read",
                                                icons::MESSAGE_SQUARE,
                                                ContextAction::MarkAllRead { server: id },
                                            ),
                                            ContextMenuItem::new(
                                                "Leave server",
                                                icons::LOG_OUT,
                                                ContextAction::LeaveServer { server: id },
                                            )
                                            .soon()
                                            .destructive(),
                                        ],
                                        cx,
                                    );
                                }),
                            )
                            .child(initial),
                    )
                    .children((mention_total > 0).then(|| {
                        div()
                            .absolute()
                            .bottom(px(-2.))
                            .right(px(6.))
                            .min_w(px(18.))
                            .h(px(18.))
                            .px(px(4.))
                            .rounded_full()
                            .bg(theme::destructive())
                            .border_2()
                            .border_color(theme::rail())
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(10.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::primary_foreground())
                            .child(mention_total.to_string())
                    }))
                    .children((mention_total == 0 && unread_total > 0).then(|| {
                        div()
                            .absolute()
                            .bottom(px(0.))
                            .right(px(8.))
                            .size(px(10.))
                            .rounded_full()
                            .bg(theme::foreground())
                            .border_2()
                            .border_color(theme::rail())
                    }))
            })
            .collect::<Vec<_>>();

        // The "+": a neutral circle that morphs into a green squircle on
        // hover (Discord's exact affordance), with the hover pill hint.
        // Opens the add-server modal *without* leaving anything.
        let add_item = div()
            .w_full()
            .relative()
            .flex()
            .justify_center()
            .group("rail-add")
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(14.))
                    .w(px(4.))
                    .h(px(20.))
                    .rounded_r_full()
                    .bg(theme::foreground())
                    .invisible()
                    .group_hover("rail-add", |style| style.visible()),
            )
            .child(
                div()
                    .id("rail-add-server")
                    .size(px(44.))
                    .rounded(px(24.))
                    .bg(theme::card())
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme::success())
                    .cursor_pointer()
                    .hover(|style| {
                        style.rounded(px(16.)).bg(theme::success()).text_color(theme::primary_foreground())
                    })
                    .active(|style| style.opacity(0.85))
                    .tooltip(theme::tooltip("Add a Server"))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::open_add_server_clicked))
                    .child(
                        theme::icon(icons::PLUS, px(20.))
                            .text_color(theme::success())
                            .group_hover("rail-add", |style| style.text_color(theme::primary_foreground())),
                    ),
            );

        div()
            .w(px(RAIL_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.))
            .pt(px(12.))
            .bg(theme::rail())
            .children(server_items)
            .child(div().w(px(32.)).h(px(2.)).rounded_full().bg(theme::border()))
            .child(add_item)
    }

    /// The channel sidebar: server header (name, host badge, copy-invite),
    /// the text- and voice-channel lists, and — pinned to the bottom — the
    /// voice status panel (while in a call, wherever it is) above the
    /// persistent user footer.
    pub(super) fn render_channel_sidebar(&mut self, profile: &Profile, server: &SavedServer, cx: &mut Context<Self>) -> Div {
        let server_id = server.link.id;
        let is_host = server.is_host;

        // What the bottom panels need to know about the live call, pulled
        // out before any element building borrows start. The call may be in
        // a *different* server — it still shows, Discord-style.
        let call_state = self.call.as_ref().map(|call| {
            let (label, color) = match &call.status {
                ChannelStatus::Connecting => ("Voice connecting…", theme::muted_foreground()),
                ChannelStatus::Connected(_) => ("Voice connected", theme::success()),
                ChannelStatus::Failed(_) => ("Connection failed", theme::destructive_foreground()),
            };
            (call.channel.name.clone(), label, color, matches!(call.status, ChannelStatus::Connected(_)))
        });
        let in_call = matches!(&self.call, Some(ActiveCall { status: ChannelStatus::Connected(_), .. }));
        let connecting = matches!(&self.call, Some(ActiveCall { status: ChannelStatus::Connecting, .. }));
        let (self_muted, self_deafened, self_speaking, self_sharing) = match &self.call {
            Some(ActiveCall { status: ChannelStatus::Connected(call), .. }) => (
                call.muted,
                call.deafened,
                call.self_speaking,
                matches!(call.sharing, SharingState::Active(_)),
            ),
            _ => (false, false, false, false),
        };
        let connected_channel_id = self
            .call
            .as_ref()
            .filter(|call| {
                call.server_id == server_id
                    && matches!(call.status, ChannelStatus::Connected(_) | ChannelStatus::Connecting)
            })
            .map(|call| call.channel.id);
        let viewing_channel_id = match &self.screen {
            Screen::Server { view: ServerView::Text { channel } | ServerView::Voice { channel }, .. } => {
                Some(channel.id)
            }
            _ => None,
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
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(15.))
                            .child(server.link.name.clone()),
                    )
                    .children(is_host.then(|| {
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .px(px(6.))
                            .py(px(1.))
                            .rounded_full()
                            .bg(theme::wash())
                            .text_color(theme::muted_foreground())
                            .child("Host")
                    })),
            )
            .child(
                theme::icon_button(
                    "copy-invite",
                    icons::LINK,
                    if self.link_copied { "Copied!" } else { "Copy invite link" },
                )
                .on_mouse_up(MouseButton::Left, cx.listener(Self::copy_link_clicked)),
            );

        let section_heading = |label: &'static str| {
            div()
                .px(px(16.))
                .pt(px(14.))
                .pb(px(4.))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::muted_foreground())
                .child(label)
        };

        let text_rows = server
            .link
            .channels
            .iter()
            .filter(|channel| channel.kind == ChannelKind::Text)
            .map(|channel| {
                let is_viewing = viewing_channel_id == Some(channel.id);
                let channel_for_click = channel.clone();
                let channel_id_for_menu = channel.id;
                let (unread_count, mention_count) = self.unread.get(&channel.id).copied().unwrap_or((0, 0));
                let has_unread = unread_count > 0 && !is_viewing;
                div()
                    .id(("text-channel", channel.id.as_u128() as u64))
                    .relative()
                    .h(px(34.))
                    .mx(px(8.))
                    .px(px(8.))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(14.5))
                    .cursor_pointer()
                    .when(is_viewing, |style| {
                        style.bg(theme::wash()).text_color(theme::foreground()).font_weight(FontWeight::MEDIUM)
                    })
                    // Unread channels read like Discord's: full-strength
                    // name instead of muted, plus the dot on the left edge.
                    .when(has_unread, |style| {
                        style.text_color(theme::foreground()).font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!is_viewing && !has_unread, |style| style.text_color(theme::muted_foreground()))
                    .hover(|style| style.bg(theme::wash()).text_color(theme::foreground()))
                    .active(|style| style.bg(theme::wash_strong()))
                    .children(has_unread.then(|| {
                        div()
                            .absolute()
                            .left(px(-8.))
                            .top(px(13.))
                            .w(px(4.))
                            .h(px(8.))
                            .rounded_r_full()
                            .bg(theme::foreground())
                    }))
                    .child(theme::icon(icons::HASH, px(18.)).text_color(theme::muted_foreground()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(channel.name.clone()),
                    )
                    .children((mention_count > 0 && !is_viewing).then(|| {
                        div()
                            .flex_none()
                            .min_w(px(16.))
                            .h(px(16.))
                            .px(px(4.))
                            .rounded_full()
                            .bg(theme::destructive())
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(10.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::primary_foreground())
                            .child(mention_count.to_string())
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |shell, _, _window, cx| {
                            shell.open_text_channel(channel_for_click.clone(), cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |shell, event, _window, cx| {
                            shell.open_context_menu(
                                event,
                                vec![
                                    ContextMenuItem::new(
                                        "Mark as read",
                                        icons::MESSAGE_SQUARE,
                                        ContextAction::MarkChannelRead { channel: channel_id_for_menu },
                                    ),
                                    ContextMenuItem::new(
                                        "Edit channel name",
                                        icons::PENCIL,
                                        ContextAction::EditChannelName { channel: channel_id_for_menu },
                                    )
                                    .soon(),
                                ],
                                cx,
                            );
                        }),
                    )
            })
            .collect::<Vec<_>>();

        // The remote roster per voice channel: everyone whose VoicePresence
        // says they're connected, with their speaking ring lit while the
        // speaking map holds them. Members still on builds without
        // VoicePresence appear the old way — only while audibly talking.
        let mut remote_roster: HashMap<Uuid, Vec<(String, bool)>> = HashMap::new();
        for ((channel_id, _), author) in &self.voice_occupants {
            let speaking = self.speaking.contains_key(&(*channel_id, author.clone()));
            remote_roster.entry(*channel_id).or_default().push((author.clone(), speaking));
        }
        for (channel_id, author) in self.speaking.keys() {
            let rows = remote_roster.entry(*channel_id).or_default();
            if !rows.iter().any(|(name, _)| name == author) {
                rows.push((author.clone(), true));
            }
        }
        for rows in remote_roster.values_mut() {
            // Same display name twice (two peers, one name): keep one row,
            // lit if either is speaking — speaking sorts first, dedup keeps
            // the first.
            rows.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
            rows.dedup_by(|a, b| a.0 == b.0);
        }

        let voice_rows = server
            .link
            .channels
            .iter()
            .filter(|channel| channel.kind == ChannelKind::Voice)
            .map(|channel| {
                let is_viewing = viewing_channel_id == Some(channel.id);
                let is_connected = connected_channel_id == Some(channel.id);
                let channel_for_click = channel.clone();

                let row = div()
                    .id(("voice-channel", channel.id.as_u128() as u64))
                    .h(px(34.))
                    .mx(px(8.))
                    .px(px(8.))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(14.5))
                    .cursor_pointer()
                    .when(is_viewing, |style| {
                        style.bg(theme::wash()).text_color(theme::foreground()).font_weight(FontWeight::MEDIUM)
                    })
                    .when(!is_viewing, |style| style.text_color(theme::muted_foreground()))
                    .hover(|style| style.bg(theme::wash()).text_color(theme::foreground()))
                    .active(|style| style.bg(theme::wash_strong()))
                    .child(theme::icon(icons::VOLUME, px(18.)).text_color(if is_connected {
                        theme::success()
                    } else {
                        theme::muted_foreground()
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(channel.name.clone()),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |shell, _, _window, cx| {
                            shell.enter_voice_channel(channel_for_click.clone(), cx);
                        }),
                    );

                // While in this channel, nest what the app actually knows
                // about who's here (just this side for now) under the row,
                // the way Discord nests connected users under a voice
                // channel.
                let participant = is_connected.then(|| {
                    let mute_label = if self_muted { "Unmute" } else { "Mute" };
                    let deafen_label = if self_deafened { "Undeafen" } else { "Deafen" };
                    div()
                        .id("vc-self-row")
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |shell, event, _window, cx| {
                                let muted = shell.connected_call_mut().is_some_and(|call| call.muted);
                                shell.open_context_menu(
                                    event,
                                    vec![
                                        ContextMenuItem::new(
                                            mute_label,
                                            if muted { icons::MIC } else { icons::MIC_OFF },
                                            ContextAction::ToggleSelfMute,
                                        ),
                                        ContextMenuItem::new(
                                            deafen_label,
                                            icons::HEADPHONE_OFF,
                                            ContextAction::ToggleSelfDeafen,
                                        ),
                                    ],
                                    cx,
                                );
                            }),
                        )
                        .mx(px(8.))
                        .pl(px(26.))
                        .pr(px(8.))
                        .py(px(3.))
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::wash()))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(speaking_avatar(
                            profile.avatar_path.clone(),
                            profile.initial().into(),
                            20.,
                            self_speaking,
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(13.))
                                .text_color(if connecting { theme::faint_foreground() } else { theme::foreground() })
                                .child(if connecting { "Connecting…".to_string() } else { profile.name.clone() }),
                        )
                        .children(self_deafened.then(|| {
                            theme::icon(icons::HEADPHONE_OFF, px(14.)).text_color(theme::destructive_foreground())
                        }))
                        .children(
                            (self_muted && !self_deafened)
                                .then(|| theme::icon(icons::MIC_OFF, px(14.)).text_color(theme::destructive_foreground())),
                        )
                        .children(
                            self_sharing.then(|| theme::icon(icons::MONITOR_UP, px(14.)).text_color(theme::info())),
                        )
                });

                // Everyone else in this channel (see remote_roster), ring
                // lit while they're audibly talking.
                let roster = remote_roster.get(&channel.id).cloned().unwrap_or_default();
                let speaker_rows = roster.into_iter().enumerate().map(|(row_ix, (name, speaking))| {
                    let initial: SharedString =
                        name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into();
                    let avatar = self.peer_avatars.get(&name).cloned();
                    let name_for_menu = name.clone();
                    div()
                        .id(SharedString::from(format!("vc-occupant-{row_ix}")))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |shell, event, _window, cx| {
                                shell.open_context_menu(
                                    event,
                                    vec![
                                        ContextMenuItem::new(
                                            "Adjust volume",
                                            icons::VOLUME,
                                            ContextAction::AdjustUserVolume { author: name_for_menu.clone() },
                                        )
                                        .soon(),
                                        ContextMenuItem::new(
                                            "Kick from voice",
                                            icons::LOG_OUT,
                                            ContextAction::KickFromVoice { author: name_for_menu.clone() },
                                        )
                                        .soon()
                                        .destructive(),
                                    ],
                                    cx,
                                );
                            }),
                        )
                        .mx(px(8.))
                        .pl(px(26.))
                        .pr(px(8.))
                        .py(px(3.))
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::wash()))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(speaking_avatar(avatar, initial, 20., speaking))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(13.))
                                .text_color(theme::foreground())
                                .child(name),
                        )
                });

                div().flex().flex_col().child(row).children(participant).children(speaker_rows)
            })
            .collect::<Vec<_>>();

        let channel_list = div()
            .id("channel-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children((!text_rows.is_empty()).then(|| section_heading("TEXT CHANNELS")))
            .children(text_rows)
            .children((!voice_rows.is_empty()).then(|| section_heading("VOICE CHANNELS")))
            .children(voice_rows);

        // The voice status panel — connection state lives directly above the
        // user it belongs to, not floating over the middle of the app.
        let voice_panel = call_state.map(|(channel_name, label, color, is_connected)| {
            div()
                .flex_none()
                .border_t_1()
                .border_color(theme::border())
                .px(px(8.))
                .py(px(8.))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(label))
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(11.))
                                .text_color(theme::muted_foreground())
                                .child(channel_name),
                        ),
                )
                .child(
                    div()
                        .id("voice-disconnect")
                        .group("voice-disconnect")
                        .size(px(32.))
                        .rounded_md()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::wash()))
                        .active(|style| style.bg(theme::wash_strong()))
                        .tooltip(theme::tooltip(if is_connected { "Disconnect" } else { "Cancel" }))
                        .on_mouse_up(MouseButton::Left, cx.listener(Self::leave_channel_clicked))
                        .child(
                            theme::icon(icons::PHONE, px(18.))
                                .text_color(theme::muted_foreground())
                                .group_hover("voice-disconnect", |style| {
                                    style.text_color(theme::destructive_foreground())
                                })
                                .with_transformation(Transformation::rotate(radians(3.926))),
                        ),
                )
        });

        let mute_button = in_call.then(|| {
            div()
                .id("footer-mute")
                .size(px(32.))
                .rounded_md()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .when(self_muted, |style| {
                    style.bg(theme::destructive()).text_color(theme::primary_foreground())
                })
                .when(!self_muted, |style| style.text_color(theme::muted_foreground()))
                .hover(move |style| {
                    if self_muted {
                        style.opacity(0.9)
                    } else {
                        style.bg(theme::wash()).text_color(theme::foreground())
                    }
                })
                .active(|style| style.opacity(0.8))
                .tooltip(theme::tooltip(if self_muted { "Unmute" } else { "Mute" }))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::toggle_mute_clicked))
                .child(
                    theme::icon(if self_muted { icons::MIC_OFF } else { icons::MIC }, px(18.)).text_color(
                        if self_muted { theme::primary_foreground() } else { theme::muted_foreground() },
                    ),
                )
        });

        let deafen_button = in_call.then(|| {
            div()
                .id("footer-deafen")
                .size(px(32.))
                .rounded_md()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .when(self_deafened, |style| {
                    style.bg(theme::destructive()).text_color(theme::primary_foreground())
                })
                .when(!self_deafened, |style| style.text_color(theme::muted_foreground()))
                .hover(move |style| {
                    if self_deafened {
                        style.opacity(0.9)
                    } else {
                        style.bg(theme::wash()).text_color(theme::foreground())
                    }
                })
                .active(|style| style.opacity(0.8))
                .tooltip(theme::tooltip(if self_deafened { "Undeafen" } else { "Deafen" }))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::toggle_deafen_clicked))
                .child(
                    theme::icon(if self_deafened { icons::HEADPHONE_OFF } else { icons::HEADPHONES }, px(18.))
                        .text_color(if self_deafened {
                            theme::primary_foreground()
                        } else {
                            theme::muted_foreground()
                        }),
                )
        });

        // The persistent user footer, on the darkest surface so it reads as
        // chrome: who you are, whether you're audible, and the way out.
        let footer = div()
            .h(px(52.))
            .flex_none()
            .bg(theme::rail())
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .id("footer-edit-profile")
                    .relative()
                    .flex_none()
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.85))
                    .tooltip(theme::tooltip("Edit profile"))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::open_edit_profile_clicked))
                    .child(theme::avatar(profile.avatar_path.clone(), profile.initial(), px(32.)))
                    .child(
                        div()
                            .absolute()
                            .bottom(px(-2.))
                            .right(px(-2.))
                            .size(px(12.))
                            .rounded_full()
                            .border_2()
                            .border_color(theme::rail())
                            .bg(theme::success()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(profile.name.clone()),
                    )
                    .child(
                        div().text_size(px(11.)).text_color(theme::muted_foreground()).child(
                            if self_deafened {
                                "Deafened"
                            } else if self_muted {
                                "Muted"
                            } else if in_call {
                                "In voice"
                            } else {
                                "Online"
                            },
                        ),
                    ),
            )
            .children(mute_button)
            .children(deafen_button)
            .child(
                theme::icon_button("footer-leave-server", icons::LOG_OUT, "Leave Server")
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::leave_server_clicked)),
            );

        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme::card())
            .child(header)
            .child(channel_list)
            .children(voice_panel)
            .child(footer)
    }

    /// The main panel for the active server: lobby welcome, a text channel's
    /// chat, or a voice channel's stage (header toolbar, video/tile stage,
    /// floating control bar). The error toast overlays whichever is shown.
    pub(super) fn render_main_panel(&mut self, profile: &Profile, server: &SavedServer, cx: &mut Context<Self>) -> Div {
        let server_name = server.link.name.clone();

        let error_toast = self.error.clone().map(|message| {
            div()
                .absolute()
                .top(px(12.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .max_w(px(600.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .pl(px(12.))
                        .pr(px(6.))
                        .py(px(6.))
                        .rounded_lg()
                        .bg(theme::popover())
                        .border_1()
                        .border_color(theme::destructive())
                        .shadow_lg()
                        .child(div().size(px(8.)).rounded_full().flex_none().bg(theme::destructive()))
                        .child(div().text_size(px(12.5)).child(message))
                        .child(
                            theme::icon_button("dismiss-error", icons::X, "Dismiss")
                                .on_mouse_up(MouseButton::Left, cx.listener(Self::dismiss_error_clicked)),
                        )
                        .with_animation(
                            "error-toast-in",
                            Animation::new(Duration::from_millis(200)).with_easing(ease_out_quint()),
                            |toast, delta| toast.opacity(delta).mt(px(-12. * (1. - delta))),
                        ),
                )
        });

        let view = match &self.screen {
            Screen::Server { view, .. } => view,
            _ => &ServerView::Lobby,
        };

        let content = match view {
            ServerView::Lobby => {
                let copy_label: SharedString =
                    if self.link_copied { "Link Copied ✓".into() } else { "Copy Invite Link".into() };
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .size(px(64.))
                            .rounded_full()
                            .bg(theme::card())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(theme::icon(icons::HEADPHONES, px(28.)).text_color(theme::muted_foreground())),
                    )
                    .child(
                        div()
                            .max_w(px(600.))
                            .text_center()
                            .text_size(px(18.))
                            .font_weight(FontWeight::BOLD)
                            .child(format!("Welcome to {server_name}")),
                    )
                    .child(
                        div()
                            .text_size(px(13.5))
                            .text_color(theme::muted_foreground())
                            .child("Pick a channel from the sidebar — text to chat, voice to talk."),
                    )
                    .child(
                        div().flex().gap_2().mt(px(8.)).child(
                            theme::button(ButtonVariant::Primary, copy_label)
                                .on_mouse_up(MouseButton::Left, cx.listener(Self::copy_link_clicked)),
                        ),
                    )
                    .into_any_element()
            }

            ServerView::Text { channel } => {
                let channel = channel.clone();
                self.render_chat_panel(profile, &channel, cx).into_any_element()
            }

            ServerView::Voice { channel } => {
                let channel = channel.clone();
                self.render_voice_panel(profile, &channel, cx).into_any_element()
            }
        };

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::aurora())
            .child(content)
            .children(error_toast)
    }

    /// The composed layout once any server exists: server rail, then — for
    /// the active server — channel sidebar and main panel, or a "pick a
    /// server" hint when none is open.
    pub(super) fn render_app_shell(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let profile = self.profile.clone().expect("render_app_shell is only reached once a profile exists");
        let rail = self.render_server_rail(cx);

        let root = div()
            .relative()
            .size_full()
            .flex()
            .overflow_hidden()

            .text_color(theme::foreground())
            .child(rail);

        match self.active_server().cloned() {
            Some(server) => {
                let sidebar = self.render_channel_sidebar(&profile, &server, cx);
                let main_panel = self.render_main_panel(&profile, &server, cx);
                root.child(sidebar).child(main_panel).into_any_element()
            }
            None => root
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(8.))
                        .child(div().text_size(px(16.)).font_weight(FontWeight::BOLD).child("Welcome back"))
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme::muted_foreground())
                                .child("Pick a server on the left, or add a new one."),
                        ),
                )
                .into_any_element(),
        }
    }
}
