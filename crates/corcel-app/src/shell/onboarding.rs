//! First-run onboarding: the identity screen (the profile card edited
//! directly, next to the gradient brand panel), the fullscreen "arrival"
//! shown while no servers exist, and the staged add-server flow those and
//! the rail-"+" modal all share (create-vs-join fork, then one focused
//! question per branch).

use super::*;

impl Shell {
    pub(super) fn pick_avatar_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.pick_profile_image(true, cx);
    }

    pub(super) fn pick_banner_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.pick_profile_image(false, cx);
    }

    pub(super) fn pick_profile_image(&mut self, is_avatar: bool, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(if is_avatar { "Choose Photo".into() } else { "Choose Banner".into() }),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = rx.await else { return };
            let Some(path) = paths.pop() else { return };
            let _ = this.update(cx, |shell, cx| {
                if is_avatar {
                    shell.profile_form.avatar_path = Some(path);
                } else {
                    shell.profile_form.banner_path = Some(path);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn profile_continue_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.submit_profile(cx);
    }

    /// Validates and persists the form as the profile. `true` on success —
    /// the edit-profile flow needs to know whether to close and broadcast.
    pub(super) fn submit_profile(&mut self, cx: &mut Context<Self>) -> bool {
        let name = self.profile_form.name_input.read(cx).content.trim().to_string();
        if name.is_empty() {
            self.profile_form.error = Some("A name is required.".to_string());
            cx.notify();
            return false;
        }
        let bio = self.profile_form.bio_input.read(cx).content.trim().to_string();
        let profile = Profile {
            name,
            avatar_path: self.profile_form.avatar_path.clone(),
            banner_path: self.profile_form.banner_path.clone(),
            bio: (!bio.is_empty()).then_some(bio),
        };
        if let Err(err) = profile.save() {
            self.profile_form.error = Some(format!("couldn't save profile: {err:#}"));
            cx.notify();
            return false;
        }
        self.profile_form.error = None;
        self.profile = Some(profile);
        cx.notify();
        true
    }

    /// Reopens the identity card (seeded from the saved profile) as a
    /// modal — the post-onboarding way to change name, photo, banner, or
    /// bio. Reached from the sidebar footer's avatar.
    pub(super) fn open_edit_profile_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.clone() else { return };
        self.profile_form
            .name_input
            .update(cx, |input, cx| input.set_content(profile.name.clone(), cx));
        self.profile_form
            .bio_input
            .update(cx, |input, cx| input.set_content(profile.bio.clone().unwrap_or_default(), cx));
        self.profile_form.avatar_path = profile.avatar_path;
        self.profile_form.banner_path = profile.banner_path;
        self.profile_form.error = None;
        self.edit_profile_open = true;
        cx.notify();
    }

    pub(super) fn close_edit_profile_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.edit_profile_open = false;
        cx.notify();
    }

    /// Saves the edited profile and immediately re-announces the card to
    /// every connected room, so friends see the new photo/bio without
    /// waiting for the next reconnect.
    pub(super) fn save_edited_profile_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.submit_profile(cx) {
            return;
        }
        self.broadcast_profile();
        self.edit_profile_open = false;
        cx.notify();
    }

    /// The edit-profile modal: the same card the onboarding identity
    /// screen edits, wrapped in the app's standard modal chrome.
    pub(super) fn render_edit_profile_modal(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let backdrop = div().size_full().absolute().top_0().left_0().bg(theme::scrim());
        let error = self.profile_form.error.clone().map(|message| {
            div().text_size(px(12.)).text_color(theme::destructive_foreground()).child(message)
        });

        let card = div()
            .id("edit-profile-card")
            .flex()
            .flex_col()
            .gap(px(18.))
            .p(px(24.))
            .bg(theme::popover())
            .border_1()
            .border_color(theme::border())
            .rounded_xl()
            .shadow_lg()
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::close_edit_profile_clicked))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(24.))
                    .child(div().text_size(px(16.)).font_weight(FontWeight::BOLD).child("Your profile"))
                    .child(
                        theme::icon_button("close-edit-profile", icons::X, "Close")
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::close_edit_profile_clicked)),
                    ),
            )
            .child(self.render_profile_card(cx))
            .children(error)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        theme::button(ButtonVariant::Ghost, "Cancel")
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::close_edit_profile_clicked)),
                    )
                    .child(
                        theme::button(ButtonVariant::Primary, "Save")
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::save_edited_profile_clicked)),
                    ),
            )
            .with_animation(
                "edit-profile-in",
                Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
                |card, delta| card.opacity(delta).mt(px(14. * (1. - delta))),
            );

        div()
            .size_full()
            .absolute()
            .top_0()
            .left_0()
            .flex()
            .flex_col()
            .justify_center()
            .items_center()
            .child(backdrop)
            .child(card)
    }

    pub(super) fn open_add_server_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.add_server_open = true;
        self.add_server_stage = AddServerStage::Choice;
        self.server_reach = Reach::default();
        self.error = None;
        cx.notify();
    }

    pub(super) fn close_add_server_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.add_server_open = false;
        cx.notify();
    }

    /// Moves the add-server flow to `stage`, landing focus in that stage's
    /// input so the user can just start typing.
    pub(super) fn arrival_go(&mut self, stage: AddServerStage, window: &mut Window, cx: &mut Context<Self>) {
        self.add_server_stage = stage;
        self.error = None;
        match stage {
            AddServerStage::Create => window.focus(&self.server_name_input.focus_handle(cx)),
            AddServerStage::Join => window.focus(&self.join_link_input.focus_handle(cx)),
            AddServerStage::Choice => {}
        }
        cx.notify();
    }

    pub(super) fn host_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.submit_host(cx);
    }

    pub(super) fn submit_host(&mut self, cx: &mut Context<Self>) {
        if self.hosting_pending {
            return;
        }
        let name = self.server_name_input.read(cx).content.to_string();
        let name = if name.trim().is_empty() { "My Server".to_string() } else { name };
        self.error = None;
        self.hosting_pending = true;
        cx.notify();

        let rx = runtime::spawn_and_send(session::host(name, self.server_reach));
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |shell, cx| {
                shell.hosting_pending = false;
                match result {
                    Ok(Ok((link, identity))) => {
                        let server = SavedServer { link, is_host: true, identity: Some(identity) };
                        if let Some(store) = &shell.store {
                            if let Err(err) = store.save_server(&server) {
                                shell.error = Some(format!("couldn't save the new server: {err:#}"));
                            }
                        }
                        let id = server.link.id;
                        shell.servers.push(server);
                        shell.add_server_open = false;
                        shell.add_server_stage = AddServerStage::Choice;
                        shell.server_name_input.update(cx, |input, cx| input.clear(cx));
                        shell.connect_chat(id, cx);
                        shell.open_server(id, cx);
                    }
                    Ok(Err(err)) => shell.error = Some(format!("failed to host server: {err:#}")),
                    Err(_) => shell.error = Some("hosting task was dropped".to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn join_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.submit_join(cx);
    }

    pub(super) fn submit_join(&mut self, cx: &mut Context<Self>) {
        let text = self.join_link_input.read(cx).content.to_string();
        match ServerLink::decode(text.trim()) {
            Ok(link) => {
                // A link that decodes but carries no endpoint id is from
                // the pre-iroh transport — it can never be dialed, so say
                // so now rather than saving it and retrying in silence.
                if let Err(err) = link.endpoint_id() {
                    self.error = Some(format!("invalid invite link: {err:#}"));
                    cx.notify();
                    return;
                }
                let id = link.id;
                if let Some(existing) = self.servers.iter_mut().find(|s| s.link.id == id) {
                    // Already a member — a re-pasted invite just refreshes
                    // the stored address (hosts re-share links after a
                    // rehost lands on a new port).
                    existing.link = link;
                    let updated = existing.link.clone();
                    if let Some(store) = &self.store {
                        let _ = store.update_link(&updated);
                    }
                } else {
                    let server = SavedServer { link, is_host: false, identity: None };
                    if let Some(store) = &self.store {
                        if let Err(err) = store.save_server(&server) {
                            self.error = Some(format!("couldn't save the joined server: {err:#}"));
                        }
                    }
                    self.servers.push(server);
                }
                self.add_server_open = false;
                self.add_server_stage = AddServerStage::Choice;
                self.error = None;
                self.join_link_input.update(cx, |input, cx| input.clear(cx));
                self.connect_chat(id, cx);
                self.open_server(id, cx);
            }
            Err(err) => self.error = Some(format!("invalid invite link: {err:#}")),
        }
        cx.notify();
    }

    /// The first-run "set up your profile" screen — shown instead of
    /// `screen` while [`Shell::profile`] is `None`. Mirrors the mockup's
    /// "First run — set up your profile" frame: a dashed banner picker with
    /// an avatar picker overlapping its bottom edge, then Name (required)
    /// and Bio (optional).
    /// The first-run identity screen. Split-bleed: a gradient brand panel
    /// stating what corcel is while you type, and — instead of a labeled
    /// form — the actual profile card, edited directly: click the banner to
    /// set a banner, click the avatar to set a photo, type your name into
    /// the card in display type. The form *is* the artifact.
    pub(super) fn render_profile_setup(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // The brand half. Each manifesto line holds invisible for a beat,
        // then fades in — a quiet staggered entrance instead of a wall of
        // text landing all at once.
        let manifesto: [(&'static str, &'static str); 3] = [
            (icons::USERS, "Servers live on your machines — no company in the middle."),
            (icons::LINK, "One link is the whole invite. Share it anywhere."),
            (icons::MIC, "Voice, video, and chat that stay between friends."),
        ];
        let brand = div()
            .w(px(360.))
            .flex_none()
            .h_full()
            .bg(linear_gradient(
                160.,
                linear_color_stop(rgba(0x5865f2ff), 0.),
                linear_color_stop(rgba(0x201a52ff), 1.),
            ))
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(44.))
            .p(px(44.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(42.))
                            .font_weight(FontWeight::EXTRA_BOLD)
                            .text_color(rgba(0xffffffff))
                            .child("corcel"),
                    )
                    .child(div().text_size(px(15.)).text_color(rgba(0xffffffb8)).child("A place for your people.")),
            )
            .child(div().flex().flex_col().gap(px(18.)).children(manifesto.into_iter().enumerate().map(
                |(index, (icon_path, line))| {
                    manifesto_row(icon_path, line).with_animations(
                        ("identity-manifesto", index as u64),
                        vec![
                            Animation::new(Duration::from_millis(160 + 150 * index as u64)),
                            Animation::new(Duration::from_millis(460)).with_easing(ease_out_quint()),
                        ],
                        |row, phase, delta| if phase == 0 { row.opacity(0.) } else { row.opacity(delta) },
                    )
                },
            )));

        let card = self.render_profile_card(cx);

        let error = self.profile_form.error.clone().map(|message| {
            div().text_size(px(12.5)).text_color(theme::destructive_foreground()).child(message)
        });

        let form = div()
            .flex()
            .flex_col()
            .gap(px(22.))
            .w(px(340.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(div().text_size(px(28.)).font_weight(FontWeight::BOLD).child("First — you."))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .text_color(theme::muted_foreground())
                            .child("This card is how friends see you. Only the name is required."),
                    ),
            )
            .child(card)
            .children(error)
            .child(
                theme::button(ButtonVariant::Primary, "That's me →")
                    .w_full()
                    .flex()
                    .justify_center()
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::profile_continue_clicked)),
            );

        div()
            .size_full()
            .flex()
            .bg(theme::background())
            .text_color(theme::foreground())
            .child(brand)
            .child(
                div().flex_1().h_full().flex().items_center().justify_center().p(px(24.)).child(
                    form.with_animation(
                        "identity-form",
                        Animation::new(Duration::from_millis(380)).with_easing(ease_out_quint()),
                        |form, delta| form.opacity(delta),
                    ),
                ),
            )
    }

    /// The editable profile card — banner picker on top, avatar picker
    /// overlapping it, then the name/bio inputs. Shared between first-run
    /// onboarding ([`Self::render_profile_setup`]) and the edit-profile
    /// modal ([`Self::render_edit_profile_modal`]); both read and write
    /// [`Shell::profile_form`].
    pub(super) fn render_profile_card(&mut self, cx: &mut Context<Self>) -> Div {
        let initial = self
            .profile_form
            .name_input
            .read(cx)
            .content
            .trim()
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string());

        // Top corners rounded on the banner itself: the card's
        // overflow_hidden clips to the rectangle, not the radius (same
        // GPUI behavior theme::avatar works around).
        let banner: AnyElement = match &self.profile_form.banner_path {
            Some(path) => img(ImageSource::from(path.clone()))
                .size_full()
                .rounded_t_xl()
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            None => div()
                .size_full()
                .rounded_t_xl()
                .bg(linear_gradient(
                    120.,
                    linear_color_stop(rgba(0x5865f2ff), 0.),
                    linear_color_stop(rgba(0x00a8fcff), 1.),
                ))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .px(px(10.))
                        .py(px(5.))
                        .rounded_full()
                        .bg(rgba(0x00000038))
                        .text_size(px(11.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgba(0xffffffd8))
                        .child("Click to add a banner"),
                )
                .into_any_element(),
        };

        // The pencil badge marks the avatar as editable even before hover.
        let avatar_badge = div()
            .absolute()
            .bottom(px(2.))
            .right(px(2.))
            .size(px(26.))
            .rounded_full()
            .bg(theme::wash_strong())
            .border_2()
            .border_color(theme::card())
            .flex()
            .items_center()
            .justify_center()
            .child(theme::icon(icons::PENCIL, px(12.)).text_color(theme::muted_foreground()));

        let card = div()
            .w(px(340.))
            .rounded_xl()
            .overflow_hidden()
            .bg(theme::card())
            .border_1()
            .border_color(theme::border())
            .shadow_lg()
            .child(
                div()
                    .id("banner-picker")
                    .h(px(110.))
                    .w_full()
                    .cursor_pointer()
                    .active(|style| style.opacity(0.9))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::pick_banner_clicked))
                    .child(banner),
            )
            .child(
                div().px(px(20.)).mt(px(-40.)).child(
                    div()
                        .id("avatar-picker")
                        .relative()
                        .w(px(88.))
                        .cursor_pointer()
                        .active(|style| style.opacity(0.9))
                        .on_mouse_up(MouseButton::Left, cx.listener(Self::pick_avatar_clicked))
                        .child(
                            div()
                                .rounded_full()
                                .border_4()
                                .border_color(theme::card())
                                .child(theme::avatar(self.profile_form.avatar_path.clone(), initial, px(80.))),
                        )
                        .child(avatar_badge),
                ),
            )
            .child(
                div()
                    .px(px(20.))
                    .pt(px(6.))
                    .pb(px(20.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(self.profile_form.name_input.clone())
                    .child(self.profile_form.bio_input.clone())
                    .children(self.profile_form.avatar_path.is_some().then(|| {
                        div()
                            .id("remove-avatar")
                            .text_size(px(12.))
                            .text_color(theme::muted_foreground())
                            .cursor_pointer()
                            .hover(|style| style.text_color(theme::destructive_foreground()))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|shell, _, _window, cx| {
                                    shell.profile_form.avatar_path = None;
                                    cx.notify();
                                }),
                            )
                            .child("Remove photo")
                    })),
            );

        card
    }

    /// The body of the current [`AddServerStage`] — shared verbatim between
    /// the first-run fullscreen arrival and the rail-"+" modal, so the
    /// polished path is the only path.
    pub(super) fn render_arrival_stage(&mut self, cx: &mut Context<Self>) -> Div {
        match self.add_server_stage {
            AddServerStage::Choice => {
                let create_card = arrival_choice_card(
                    "arrival-create",
                    linear_gradient(
                        135.,
                        linear_color_stop(rgba(0x16283aff), 0.),
                        linear_color_stop(rgba(0x0e151dff), 1.),
                    ),
                    icons::PLUS,
                    "Create your own",
                    "Host a server on this machine — a #general to talk in, a voice room to hang in, an invite link to hand out.",
                    "Start fresh →",
                    rgba(0x54a9ffff),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|shell, _: &MouseUpEvent, window, cx| {
                        shell.arrival_go(AddServerStage::Create, window, cx)
                    }),
                );
                let join_card = arrival_choice_card(
                    "arrival-join",
                    linear_gradient(
                        135.,
                        linear_color_stop(rgba(0x15301fff), 0.),
                        linear_color_stop(rgba(0x0d1712ff), 1.),
                    ),
                    icons::LINK,
                    "Join your friends",
                    "Someone sent you an invite link? Paste it and you're standing in their server a second later.",
                    "I have an invite →",
                    rgba(0x2ec06cff),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|shell, _: &MouseUpEvent, window, cx| {
                        shell.arrival_go(AddServerStage::Join, window, cx)
                    }),
                );
                div().flex().gap(px(18.)).child(create_card).child(join_card)
            }
            AddServerStage::Create => {
                let typed = self.server_name_input.read(cx).content.trim().to_string();
                let display: SharedString =
                    if typed.is_empty() { "The Clubhouse".into() } else { typed.clone().into() };
                let initial = display
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "?".to_string());

                // The live preview: the exact rail bubble and name this
                // server will get, updating per keystroke — name it and
                // *watch* it become real.
                let preview = div()
                    .rounded(px(12.))
                    .bg(theme::wash())
                    .border_1()
                    .border_color(theme::glass_edge())
                    .p(px(14.))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(
                        div()
                            .size(px(48.))
                            .rounded(px(16.))
                            .flex_none()
                            .bg(theme::primary())
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(18.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::primary_foreground())
                            .child(initial),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().text_size(px(14.5)).font_weight(FontWeight::SEMIBOLD).child(display))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::muted_foreground())
                                    .child("# general · a voice room · invite link ready to share"),
                            ),
                    );

                // The reach fork, decided at creation and permanent for the
                // server's life — so the copy says plainly what each choice
                // trades away, instead of hiding it behind a settings page
                // the user would only find after a confusing failure.
                let reach_picker = div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        reach_option_row(
                            "reach-global",
                            self.server_reach == Reach::Global,
                            icons::LINK,
                            "Reachable anywhere — recommended",
                            "Friends connect from any network. iroh's public infrastructure \
                             finds this machine and punches through NATs; traffic stays \
                             end-to-end encrypted.",
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|shell, _: &MouseUpEvent, _, cx| {
                                shell.server_reach = Reach::Global;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        reach_option_row(
                            "reach-local",
                            self.server_reach == Reach::LocalNetwork,
                            icons::USERS,
                            "Private network only",
                            "Never announced to any public infrastructure. Only people who can \
                             already reach this machine can join — same LAN, or a shared VPN \
                             like Tailscale. The invite link carries your current IP, so share \
                             a fresh one if it changes.",
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|shell, _: &MouseUpEvent, _, cx| {
                                shell.server_reach = Reach::LocalNetwork;
                                cx.notify();
                            }),
                        ),
                    );

                let cta = if self.hosting_pending {
                    theme::button(ButtonVariant::Primary, "Creating…").opacity(0.6)
                } else {
                    theme::button(ButtonVariant::Primary, "Create server →")
                        .on_mouse_up(MouseButton::Left, cx.listener(Self::host_clicked))
                };

                div()
                    .flex()
                    .flex_col()
                    .gap(px(18.))
                    .w(px(430.))
                    .child(self.server_name_input.clone())
                    .child(preview)
                    .child(reach_picker)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(self.arrival_back_link(cx))
                            .child(cta),
                    )
            }
            AddServerStage::Join => div()
                .flex()
                .flex_col()
                .gap(px(18.))
                .w(px(430.))
                .child(self.join_link_input.clone())
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme::faint_foreground())
                        .child("Ask your friend to hit \"Copy Invite Link\" in their server — the link starts with corcel1."),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(self.arrival_back_link(cx))
                        .child(
                            theme::button(ButtonVariant::Primary, "Join server →")
                                .on_mouse_up(MouseButton::Left, cx.listener(Self::join_clicked)),
                        ),
                ),
        }
    }

    /// The "← Back" step-out shared by the Create and Join stages.
    pub(super) fn arrival_back_link(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id("arrival-back")
            .px(px(10.))
            .py(px(6.))
            .rounded_md()
            .cursor_pointer()
            .text_size(px(13.))
            .text_color(theme::muted_foreground())
            .hover(|style| style.bg(theme::wash()).text_color(theme::foreground()))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|shell, _: &MouseUpEvent, window, cx| {
                    shell.arrival_go(AddServerStage::Choice, window, cx)
                }),
            )
            .child("← Back")
    }

    /// The first-run "arrival": no servers yet, so the whole window becomes
    /// the add-server flow — a fork between creating and joining, then one
    /// focused question per branch. Each stage cross-fades in; Enter
    /// submits, Escape backs out (see [`Self::root_key_down`]).
    pub(super) fn render_arrival(&mut self, cx: &mut Context<Self>) -> Div {
        let (headline, sub) = match self.add_server_stage {
            AddServerStage::Choice => {
                ("Now — a place to meet.", "Host your own in seconds, or walk into a friend's.")
            }
            AddServerStage::Create => {
                ("Name your server.", "It runs from this machine; friends connect straight to you.")
            }
            AddServerStage::Join => ("Got an invite?", "Paste the whole link — nothing else needed."),
        };
        let stage_index = match self.add_server_stage {
            AddServerStage::Choice => 0u64,
            AddServerStage::Create => 1,
            AddServerStage::Join => 2,
        };
        let error = self.error.clone().map(|message| {
            div().text_size(px(12.5)).text_color(theme::destructive_foreground()).child(message)
        });

        div()
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(theme::background())
            .text_color(theme::foreground())
            // A soft brand glow bleeding from the top edge, so even the
            // empty state has some light in it.
            .child(
                div().absolute().top_0().left_0().w_full().h(px(320.)).bg(linear_gradient(
                    180.,
                    linear_color_stop(rgba(0x5865f236), 0.),
                    linear_color_stop(rgba(0x5865f200), 1.),
                )),
            )
            .child(
                div()
                    .absolute()
                    .top(px(26.))
                    .left(px(32.))
                    .text_size(px(17.))
                    .font_weight(FontWeight::EXTRA_BOLD)
                    .child("corcel"),
            )
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(28.))
                    .p(px(24.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(6.))
                            .child(div().text_size(px(28.)).font_weight(FontWeight::BOLD).child(headline))
                            .child(div().text_size(px(13.5)).text_color(theme::muted_foreground()).child(sub)),
                    )
                    .child(self.render_arrival_stage(cx).with_animation(
                        ("arrival-stage", stage_index),
                        Animation::new(Duration::from_millis(280)).with_easing(ease_out_quint()),
                        |stage, delta| stage.opacity(delta),
                    ))
                    .children(error),
            )
    }

    /// The "Add a Server" modal for users who already have servers: the
    /// same staged arrival flow (see [`Self::render_arrival_stage`]) on a
    /// popover card. Dismissed by the "×", Escape, or releasing the mouse
    /// outside the card (same `on_mouse_up_out` trick [`TextInput`] uses to
    /// end a drag started inside it).
    pub(super) fn render_add_server_modal(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let backdrop = div().size_full().absolute().top_0().left_0().bg(theme::scrim());

        let title = match self.add_server_stage {
            AddServerStage::Choice => "Where next?",
            AddServerStage::Create => "Name your server",
            AddServerStage::Join => "Paste your invite",
        };
        let stage_index = match self.add_server_stage {
            AddServerStage::Choice => 0u64,
            AddServerStage::Create => 1,
            AddServerStage::Join => 2,
        };
        let error = self.error.clone().map(|message| {
            div().text_size(px(12.)).text_color(theme::destructive_foreground()).child(message)
        });

        let card = div()
            .id("add-server-card")
            .flex()
            .flex_col()
            .gap(px(18.))
            .p(px(24.))
            .bg(theme::raised_fill())
            .border_1()
            .border_color(theme::glass_edge())
            .rounded(px(16.)) // coss card radius
            .shadow_lg()
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::close_add_server_clicked))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(24.))
                    .child(div().text_size(px(16.)).font_weight(FontWeight::BOLD).child(title))
                    .child(
                        theme::icon_button("close-add-server", icons::X, "Close")
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::close_add_server_clicked)),
                    ),
            )
            .child(self.render_arrival_stage(cx).with_animation(
                ("add-server-stage", stage_index),
                Animation::new(Duration::from_millis(280)).with_easing(ease_out_quint()),
                |stage, delta| stage.opacity(delta),
            ))
            .children(error)
            .with_animation(
                "add-server-in",
                Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
                |card, delta| card.opacity(delta).mt(px(14. * (1. - delta))),
            );

        div()
            .size_full()
            .absolute()
            .top_0()
            .left_0()
            .flex()
            .flex_col()
            .justify_center()
            .items_center()
            .child(backdrop)
            .child(card)
    }

}

/// One line of the identity screen's brand-panel manifesto: an icon chip
/// over the gradient plus a short claim about what corcel is.
pub(super) fn manifesto_row(icon_path: &'static str, line: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(14.))
        .child(
            div()
                .size(px(38.))
                .rounded_lg()
                .flex_none()
                .bg(rgba(0xffffff24))
                .flex()
                .items_center()
                .justify_center()
                .child(theme::icon(icon_path, px(18.)).text_color(rgba(0xffffffee))),
        )
        .child(div().text_size(px(13.5)).text_color(rgba(0xffffffcc)).child(line))
}

/// One selectable row of the Create stage's reach picker (anywhere vs.
/// private network). The selected row gets the focus-ring border and a
/// wash background; the whole row is the click target — callers attach
/// `.on_mouse_up(...)`.
pub(super) fn reach_option_row(
    id: &'static str,
    selected: bool,
    icon_path: &'static str,
    title: &'static str,
    copy: &'static str,
) -> Stateful<Div> {
    let row = div()
        .id(id)
        .rounded(px(10.))
        .border_1()
        .p(px(12.))
        .flex()
        .items_start()
        .gap(px(12.))
        .cursor_pointer()
        .child(
            div()
                .size(px(32.))
                .rounded_md()
                .flex_none()
                .bg(if selected { theme::primary() } else { theme::wash() })
                .flex()
                .items_center()
                .justify_center()
                .child(theme::icon(icon_path, px(15.)).text_color(if selected {
                    theme::primary_foreground()
                } else {
                    theme::muted_foreground()
                })),
        )
        .child(
            div()
                // Without flex_1 + min_w_0 the copy's intrinsic single-line
                // width wins over the card's width and paints clean past
                // the modal — the app-wide overflow bug's poster child.
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(title))
                .child(
                    div()
                        .text_size(px(11.5))
                        .line_height(px(16.))
                        .text_color(theme::muted_foreground())
                        .child(copy),
                ),
        );
    if selected {
        row.border_color(theme::ring()).bg(theme::wash_strong())
    } else {
        row.border_color(theme::glass_edge()).bg(theme::wash()).hover(|style| style.bg(theme::wash_strong()))
    }
}

/// One of the two big illustrated cards at the arrival fork (create vs.
/// join): a gradient "art" header with a badge icon, then title, copy, and
/// an accent-colored call-to-action line. The whole card is the click
/// target — callers attach `.on_mouse_up(...)`.
pub(super) fn arrival_choice_card(
    id: &'static str,
    art: Background,
    icon_path: &'static str,
    title: &'static str,
    copy: &'static str,
    cta: &'static str,
    accent: Rgba,
) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(264.))
        .rounded(px(16.))
        .overflow_hidden()
        .bg(theme::wash())
        .border_1()
        .border_color(theme::glass_edge())
        .shadow_md()
        .cursor_pointer()
        .hover(|style| style.border_color(theme::ring()).bg(theme::wash_strong()))
        .active(|style| style.opacity(0.92))
        .child(
            div().h(px(112.)).w_full().rounded_t(px(15.)).bg(art).flex().items_center().justify_center().child(
                div()
                    .size(px(56.))
                    .rounded_full()
                    .p(px(1.))
                    .bg(theme::bevel_ring())
                    .child(
                        div()
                            .size_full()
                            .rounded_full()
                            .bg(theme::wash_strong())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(theme::icon(icon_path, px(26.)).text_color(theme::foreground())),
                    ),
            ),
        )
        .child(
            div()
                .p(px(18.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(div().text_size(px(15.5)).font_weight(FontWeight::BOLD).child(title))
                .child(
                    div()
                        .text_size(px(12.5))
                        .line_height(px(18.))
                        .text_color(theme::muted_foreground())
                        .child(copy),
                )
                .child(
                    div().mt(px(4.)).text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(accent).child(cta),
                ),
        )
}
