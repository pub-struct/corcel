//! Text chat: connecting to a server's relay room, the serverless
//! replication dance (broadcast + history backfill, see [`crate::chat`]),
//! unread tracking, typing indicators, the composer with reply/edit state,
//! and the chat panel that renders it all.

use super::*;

/// The fixed reaction palette — no emoji picker yet, these six cover the
/// Discord staples. Each renders as an inline button under a message when
/// its "add reaction" action is clicked.
const REACTION_PALETTE: &[&str] = &["👍", "❤️", "😂", "😮", "😢", "🎉"];

impl Shell {
    pub(super) fn open_text_channel(&mut self, channel: ChannelInfo, cx: &mut Context<Self>) {
        let Screen::Server { id, .. } = &self.screen else { return };
        let id = *id;
        self.chat_messages =
            self.store.as_ref().and_then(|store| store.messages(channel.id).ok()).unwrap_or_default();
        self.reset_composer_state();
        self.stop_video_embeds(cx);
        self.reload_reactions(channel.id);
        self.mark_channel_read(channel.id);
        self.screen = Screen::Server { id, view: ServerView::Text { channel } };
        self.chat_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Drops every bit of in-flight composer/message-action state — a reply
    /// half-set-up, an edit in progress, an open emoji palette. Called on
    /// any navigation, since all of it is meaningless in another channel.
    pub(super) fn reset_composer_state(&mut self) {
        self.replying_to = None;
        self.editing = None;
        self.reacting_to = None;
        self.chat_reactions.clear();
    }

    /// Rebuilds [`Shell::chat_reactions`] for a channel from the store's
    /// live (non-removed) reaction rows.
    pub(super) fn reload_reactions(&mut self, channel: Uuid) {
        self.chat_reactions.clear();
        let Some(store) = &self.store else { return };
        let Ok(rows) = store.reactions_for(channel) else { return };
        for (message_id, emoji, author) in rows {
            let chips = self.chat_reactions.entry(message_id).or_default();
            match chips.iter_mut().find(|(chip, _)| *chip == emoji) {
                Some((_, authors)) => {
                    if !authors.contains(&author) {
                        authors.push(author);
                    }
                }
                None => chips.push((emoji, vec![author])),
            }
        }
    }

    /// Marks a channel read as of now: everything currently stored for it
    /// stops counting as unread, durably and in the in-memory badge map.
    pub(super) fn mark_channel_read(&mut self, channel: Uuid) {
        if let Some(store) = &self.store {
            let _ = store.set_last_read(channel, chat::now_millis());
        }
        self.unread.remove(&channel);
    }

    /// Reloads one server's unread/mention counts from the store into
    /// [`Shell::unread`]. Called on startup and whenever new messages land
    /// for the server (including for *background* servers — that's what the
    /// badges are for).
    pub(super) fn refresh_unread(&mut self, server_id: Uuid) {
        let channel_ids: Vec<Uuid> = self
            .servers
            .iter()
            .find(|server| server.link.id == server_id)
            .map(|server| server.link.channels.iter().map(|channel| channel.id).collect())
            .unwrap_or_default();
        for id in &channel_ids {
            self.unread.remove(id);
        }
        let name = self.my_name();
        let Some(store) = &self.store else { return };
        let Ok(counts) = store.unread_counts(server_id, &name) else { return };
        for (channel, unread, mentions) in counts {
            self.unread.insert(channel, (unread, mentions));
        }
    }

    pub(super) fn room_generation(&self, server_id: Uuid) -> u64 {
        self.room_generations.get(&server_id).copied().unwrap_or(0)
    }

    /// Starts (or restarts) a server's chat-room loop: the long-lived relay
    /// connection that carries message broadcasts and history exchanges
    /// (see [`chat`] for the replication scheme). The loop never gives up —
    /// it reconnects with backoff for as long as the server stays saved,
    /// because hosts restart and laptops sleep. Until it connects, chat
    /// quietly stays read-only-from-local-history — local first, the
    /// network is optional.
    pub(super) fn connect_chat(&mut self, server_id: Uuid, cx: &mut Context<Self>) {
        let generation = {
            let entry = self.room_generations.entry(server_id).or_insert(0);
            *entry += 1;
            *entry
        };
        self.rooms.remove(&server_id);

        cx.spawn(async move |this, cx| {
            // Fast burst first (on launch the host's own relay may still be
            // respawning — see start_rehosts), then patient forever.
            const BACKOFF_SECS: &[u64] = &[1, 1, 1, 1, 1, 5, 15, 60];
            // Where a *re*connect resumes in the schedule: skip the startup
            // burst, a dropped connection warrants patience, not hammering.
            const RECONNECT_ATTEMPT: usize = 6;
            let mut attempt = 0usize;
            loop {
                if attempt > 0 {
                    let delay = BACKOFF_SECS[(attempt - 1).min(BACKOFF_SECS.len() - 1)];
                    cx.background_executor().timer(Duration::from_secs(delay)).await;
                }
                attempt += 1;

                // Re-read the link every attempt — on launch a legacy
                // hosted server may still be waiting for start_rehosts to
                // mint its endpoint id. Host and guest dial the same id;
                // the signal client short-circuits same-process relays to
                // loopback itself, so no hairpin special-casing here.
                // Outer Option: is this room loop still current (and its
                // server still saved)? Inner: does the link have a dialable
                // endpoint id yet?
                let target = this.update(cx, |shell, _| {
                    if shell.room_generation(server_id) != generation {
                        return None;
                    }
                    shell
                        .servers
                        .iter()
                        .find(|s| s.link.id == server_id)
                        .map(|server| server.link.endpoint_id().ok())
                });
                let relay = match target {
                    Ok(Some(Some(relay))) => relay,
                    Ok(Some(None)) => continue, // legacy link, id not minted yet
                    _ => return,                // superseded, removed, or shell gone
                };

                let Ok(Ok(mut conn)) =
                    runtime::spawn_and_send(session::open_room(relay, server_id)).await
                else {
                    continue;
                };

                let outbound = conn.outbound.clone();
                let registered = this.update(cx, |shell, cx| {
                    if shell.room_generation(server_id) != generation {
                        return false;
                    }
                    shell.rooms.insert(server_id, ChatRoom { outbound });
                    cx.notify();
                    true
                });
                if !matches!(registered, Ok(true)) {
                    return;
                }

                // The pump: every room event lands here until the
                // connection drops or the room is superseded (generation
                // mismatch). `recv` is just a future, so it works fine on
                // the GPUI executor even though the channel is fed from
                // tokio.
                while let Some(message) = conn.inbound.recv().await {
                    let keep_going = this.update(cx, |shell, cx| {
                        if shell.room_generation(server_id) != generation {
                            return false;
                        }
                        shell.handle_room_message(server_id, message, cx);
                        true
                    });
                    if !matches!(keep_going, Ok(true)) {
                        return;
                    }
                }

                // Connection dropped — forget the room and go around again.
                let still_current = this.update(cx, |shell, cx| {
                    if shell.room_generation(server_id) != generation {
                        return false;
                    }
                    shell.rooms.remove(&server_id);
                    cx.notify();
                    true
                });
                if !matches!(still_current, Ok(true)) {
                    return;
                }
                attempt = RECONNECT_ATTEMPT;
            }
        })
        .detach();
    }

    pub(super) fn handle_room_message(&mut self, server_id: Uuid, message: ServerMessage, cx: &mut Context<Self>) {
        match message {
            // Just entered the room: catch up on whatever this user missed
            // by asking *any* online member — the first peer is as good as
            // any, since everyone replicates everything.
            ServerMessage::RoomWelcome { peers, .. } => {
                let Some(peer) = peers.first().copied() else { return };
                let since = self
                    .store
                    .as_ref()
                    .and_then(|store| store.latest_timestamp(server_id).ok().flatten())
                    .map(|latest| latest - chat::HISTORY_OVERLAP_MILLIS)
                    .unwrap_or(0);
                self.send_room(server_id, ChatPayload::HistoryRequest { since }, Some(peer));
            }
            ServerMessage::Published { payload, .. } => match serde_json::from_value(payload) {
                Ok(ChatPayload::Message(message)) => self.absorb_messages(server_id, vec![message], cx),
                Ok(ChatPayload::Typing { channel, author }) => self.note_typing(channel, author, cx),
                Ok(ChatPayload::Edit { message_id, channel, edited_at, body }) => {
                    self.absorb_edit(server_id, message_id, channel, edited_at, body, cx);
                }
                Ok(ChatPayload::Delete { message_id, channel, .. }) => {
                    self.absorb_delete(server_id, message_id, channel, cx);
                }
                Ok(ChatPayload::Reaction(reaction)) => self.absorb_reactions(server_id, vec![reaction], cx),
                Ok(ChatPayload::Speaking { channel, author, speaking }) => {
                    self.note_speaking(channel, author, speaking, cx);
                }
                _ => {}
            },
            ServerMessage::Direct { from, payload } => match serde_json::from_value(payload) {
                // Someone else is catching up — serve them from our store.
                // Every member can do this; that's what makes the owner's
                // machine unnecessary for history.
                Ok(ChatPayload::HistoryRequest { since }) => {
                    let Some(store) = &self.store else { return };
                    let Ok(messages) = store.messages_since(server_id, since) else { return };
                    let reactions = store.reactions_since(server_id, since).unwrap_or_default();
                    self.send_room(server_id, ChatPayload::HistoryBatch { messages, reactions }, Some(from));
                }
                Ok(ChatPayload::HistoryBatch { messages, reactions }) => {
                    self.absorb_messages(server_id, messages, cx);
                    self.absorb_reactions(server_id, reactions, cx);
                }
                Ok(ChatPayload::Message(message)) => self.absorb_messages(server_id, vec![message], cx),
                // Everything else (Typing, Edit, …) is broadcast-only; a
                // direct one is a peer bug, and unparseable JSON is a newer
                // build's payload — both dropped.
                _ => {}
            },
            _ => {}
        }
    }

    pub(super) fn send_room(&self, server_id: Uuid, payload: ChatPayload, to: Option<PeerId>) {
        let Some(room) = self.rooms.get(&server_id) else { return };
        let Ok(payload) = serde_json::to_value(&payload) else { return };
        let message = match to {
            Some(to) => ClientMessage::Direct { to, payload },
            None => ClientMessage::Publish { payload },
        };
        let _ = room.outbound.send(message);
    }

    /// Stores received messages (deduped by id — see
    /// [`store::Store::insert_message`]) and, if any were actually new and
    /// their channel is on screen, refreshes the visible list.
    pub(super) fn absorb_messages(&mut self, server_id: Uuid, messages: Vec<ChatMessage>, cx: &mut Context<Self>) {
        // An arrived message beats any "…is typing" from its author.
        for message in &messages {
            self.typing.remove(&(message.channel, message.author.clone()));
        }
        let mut any_new = false;
        {
            let Some(store) = self.store.as_ref() else { return };
            for message in &messages {
                if store.insert_message(server_id, message).unwrap_or(false) {
                    any_new = true;
                }
            }
        }
        if !any_new {
            return;
        }
        // The channel on screen is read-as-they-arrive; everything else
        // gains badges. Marking read must precede the recount or the
        // on-screen channel would count its own fresh messages.
        let viewing = match &self.screen {
            Screen::Server { id, view: ServerView::Text { channel } } if *id == server_id => Some(channel.id),
            _ => None,
        };
        if let Some(channel_id) = viewing {
            self.mark_channel_read(channel_id);
        }
        self.refresh_unread(server_id);
        let refreshed =
            viewing.and_then(|channel_id| self.store.as_ref().and_then(|store| store.messages(channel_id).ok()));
        if let Some(messages) = refreshed {
            self.chat_messages = messages;
            self.chat_scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Applies a broadcast edit (last-writer-wins in the store) and, if the
    /// edited message is on screen, refreshes the view.
    pub(super) fn absorb_edit(
        &mut self,
        server_id: Uuid,
        message_id: Uuid,
        channel: Uuid,
        edited_at: i64,
        body: String,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = &self.store else { return };
        if !store.apply_edit(message_id, &body, edited_at).unwrap_or(false) {
            return;
        }
        self.refresh_visible_channel(server_id, channel, cx);
    }

    /// Applies a broadcast delete tombstone; refreshes the view if the
    /// message is on screen.
    pub(super) fn absorb_delete(&mut self, server_id: Uuid, message_id: Uuid, channel: Uuid, cx: &mut Context<Self>) {
        let Some(store) = &self.store else { return };
        if !store.apply_delete(message_id).unwrap_or(false) {
            return;
        }
        self.refresh_visible_channel(server_id, channel, cx);
    }

    /// Merges received reaction states (one from a live toggle, many from a
    /// history batch) and refreshes whichever visible channel they touched.
    pub(super) fn absorb_reactions(&mut self, server_id: Uuid, reactions: Vec<ReactionRow>, cx: &mut Context<Self>) {
        let mut changed_channels: Vec<Uuid> = Vec::new();
        {
            let Some(store) = self.store.as_ref() else { return };
            for reaction in &reactions {
                if store.upsert_reaction(reaction).unwrap_or(false) && !changed_channels.contains(&reaction.channel)
                {
                    changed_channels.push(reaction.channel);
                }
            }
        }
        for channel in changed_channels {
            self.refresh_visible_channel(server_id, channel, cx);
        }
    }

    /// If `channel` is the text channel on screen (in `server_id`), reloads
    /// its messages and reactions from the store and re-renders. The no-op
    /// case is the common one — most edits/reactions land for channels the
    /// user isn't looking at, and the store already holds them.
    pub(super) fn refresh_visible_channel(&mut self, server_id: Uuid, channel: Uuid, cx: &mut Context<Self>) {
        let viewing = matches!(
            &self.screen,
            Screen::Server { id, view: ServerView::Text { channel: viewing } }
                if *id == server_id && viewing.id == channel
        );
        if !viewing {
            return;
        }
        self.chat_messages =
            self.store.as_ref().and_then(|store| store.messages(channel).ok()).unwrap_or_default();
        self.reload_reactions(channel);
        cx.notify();
    }

    /// Records someone else's Typing payload; re-renders only if their
    /// channel is the one on screen (the strip is the only UI for it).
    pub(super) fn note_typing(&mut self, channel: Uuid, author: String, cx: &mut Context<Self>) {
        if author == self.my_name() {
            return;
        }
        self.typing.insert((channel, author), Instant::now());
        if matches!(
            &self.screen,
            Screen::Server { view: ServerView::Text { channel: viewing }, .. } if viewing.id == channel
        ) {
            cx.notify();
        }
    }

    /// Records someone else's Speaking transition (or the periodic `true`
    /// refresh that keeps the janitor's expiry at bay during long
    /// stretches of talking). Re-renders only on actual ring changes.
    pub(super) fn note_speaking(&mut self, channel: Uuid, author: String, speaking: bool, cx: &mut Context<Self>) {
        if author == self.my_name() {
            return;
        }
        let changed = if speaking {
            self.speaking.insert((channel, author), Instant::now()).is_none()
        } else {
            self.speaking.remove(&(channel, author)).is_some()
        };
        if changed {
            cx.notify();
        }
    }

    /// Called on every keystroke that lands in the composer: broadcasts a
    /// Typing payload if there's real content and the last one is stale.
    pub(super) fn maybe_send_typing(&mut self, cx: &mut Context<Self>) {
        let (server_id, channel_id) = match &self.screen {
            Screen::Server { id, view: ServerView::Text { channel } } => (*id, channel.id),
            _ => return,
        };
        if self.message_input.read(cx).content.trim().is_empty() {
            return;
        }
        let now = Instant::now();
        if self.last_typing_sent.is_some_and(|at| now.duration_since(at) < Duration::from_secs(3)) {
            return;
        }
        self.last_typing_sent = Some(now);
        let author = self.my_name();
        self.send_room(server_id, ChatPayload::Typing { channel: channel_id, author }, None);
    }

    pub(super) fn send_chat_message(&mut self, cx: &mut Context<Self>) {
        let body = self.message_input.read(cx).content.trim().to_string();
        if body.is_empty() {
            return;
        }
        let (server_id, channel_id) = match &self.screen {
            Screen::Server { id, view: ServerView::Text { channel } } => (*id, channel.id),
            _ => return,
        };
        let author = self.profile.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "?".to_string());
        let message = ChatMessage {
            id: Uuid::new_v4(),
            channel: channel_id,
            author,
            sent_at: chat::now_millis(),
            body,
            reply_to: self.replying_to.take().map(|original| original.id),
            edited_at: None,
            deleted: false,
        };

        // Local first: the message is durably ours before the network hears
        // about it. If nobody's online right now, peers will pick it up via
        // history backfill next time we share a room with them.
        if let Some(store) = &self.store {
            let _ = store.insert_message(server_id, &message);
        }
        self.send_room(server_id, ChatPayload::Message(message.clone()), None);
        self.chat_messages.push(message);
        // The sent message supersedes "…is typing"; the next keystroke of a
        // new draft should announce immediately again.
        self.last_typing_sent = None;
        self.message_input.update(cx, |input, cx| input.clear(cx));
        self.chat_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// The server and channel of the text view on screen — every message
    /// action below is only reachable from there.
    pub(super) fn visible_text_channel(&self) -> Option<(Uuid, Uuid)> {
        match &self.screen {
            Screen::Server { id, view: ServerView::Text { channel } } => Some((*id, channel.id)),
            _ => None,
        }
    }

    pub(super) fn start_reply(&mut self, message: ChatMessage, window: &mut Window, cx: &mut Context<Self>) {
        self.replying_to = Some(message);
        self.editing = None;
        self.reacting_to = None;
        window.focus(&self.message_input.focus_handle(cx));
        cx.notify();
    }

    pub(super) fn start_edit(&mut self, message: &ChatMessage, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| TextInput::new("Edit message…", cx));
        input.update(cx, |input, cx| input.set_content(message.body.clone(), cx));
        window.focus(&input.focus_handle(cx));
        self.editing = Some((message.id, input));
        self.replying_to = None;
        self.reacting_to = None;
        cx.notify();
    }

    /// Enter in the inline editor: persist the edit locally (last-writer-
    /// wins, so our own newer timestamp always applies), broadcast it, and
    /// refresh. An emptied editor just cancels — delete is its own action.
    pub(super) fn commit_edit(&mut self, cx: &mut Context<Self>) {
        let Some((message_id, input)) = self.editing.take() else { return };
        let Some((server_id, channel_id)) = self.visible_text_channel() else {
            cx.notify();
            return;
        };
        let body = input.read(cx).content.trim().to_string();
        if body.is_empty() {
            cx.notify();
            return;
        }
        let edited_at = chat::now_millis();
        if let Some(store) = &self.store {
            let _ = store.apply_edit(message_id, &body, edited_at);
        }
        self.send_room(server_id, ChatPayload::Edit { message_id, channel: channel_id, edited_at, body }, None);
        self.refresh_visible_channel(server_id, channel_id, cx);
        cx.notify();
    }

    pub(super) fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            cx.notify();
        }
    }

    /// Deletes one of this user's own messages: tombstone locally, then
    /// broadcast. No confirmation — the stub it leaves behind makes the
    /// action obvious and the blast radius is one message.
    pub(super) fn delete_message(&mut self, message_id: Uuid, cx: &mut Context<Self>) {
        let Some((server_id, channel_id)) = self.visible_text_channel() else { return };
        if let Some(store) = &self.store {
            let _ = store.apply_delete(message_id);
        }
        self.send_room(
            server_id,
            ChatPayload::Delete { message_id, channel: channel_id, deleted_at: chat::now_millis() },
            None,
        );
        if self.editing.as_ref().is_some_and(|(editing_id, _)| *editing_id == message_id) {
            self.editing = None;
        }
        self.refresh_visible_channel(server_id, channel_id, cx);
        cx.notify();
    }

    /// Flips this user's reaction state for one `(message, emoji)`: on if
    /// they hadn't reacted, off if they had. Store first, then broadcast —
    /// same local-first rule as messages.
    pub(super) fn toggle_reaction(&mut self, message_id: Uuid, emoji: &str, cx: &mut Context<Self>) {
        let Some((server_id, channel_id)) = self.visible_text_channel() else { return };
        let author = self.my_name();
        let removed = self
            .chat_reactions
            .get(&message_id)
            .and_then(|chips| chips.iter().find(|(chip, _)| chip == emoji))
            .is_some_and(|(_, authors)| authors.contains(&author));
        let reaction = ReactionRow {
            message_id,
            channel: channel_id,
            emoji: emoji.to_string(),
            author,
            at: chat::now_millis(),
            removed,
        };
        if let Some(store) = &self.store {
            let _ = store.upsert_reaction(&reaction);
        }
        self.send_room(server_id, ChatPayload::Reaction(reaction), None);
        self.reacting_to = None;
        self.reload_reactions(channel_id);
        cx.notify();
    }

    /// A text channel's panel: `#name` header, scrollback, and the composer
    /// (Enter sends — the wrapper catches the key event bubbling up from
    /// the focused [`TextInput`], same trick as the shell's F11 handler).
    pub(super) fn render_chat_panel(&mut self, profile: &Profile, channel: &ChannelInfo, cx: &mut Context<Self>) -> Div {
        let header = div()
            .h(px(48.))
            .flex_none()
            .px(px(12.))
            .border_b_1()
            .border_color(theme::border())
            .flex()
            .items_center()
            .gap(px(8.))
            .child(theme::icon(icons::HASH, px(18.)).text_color(theme::muted_foreground()))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(15.))
                    .child(channel.name.clone()),
            );

        let empty_state = self.chat_messages.is_empty().then(|| {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .child(
                    div()
                        .size(px(56.))
                        .rounded_full()
                        .bg(theme::card())
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(theme::icon(icons::HASH, px(26.)).text_color(theme::muted_foreground())),
                )
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::BOLD)
                        .child(format!("Welcome to #{}", channel.name)),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::muted_foreground())
                        .child("This is the beginning of the channel. Say something!"),
                )
        });

        let profile_name = profile.name.clone();
        let profile_avatar = profile.avatar_path.clone();
        // Author + snippet of every loaded message, so reply quotes render
        // without a per-row rescan of the list.
        let quotes: HashMap<Uuid, (String, String)> = self
            .chat_messages
            .iter()
            .map(|message| {
                let snippet: String = if message.deleted {
                    "message deleted".to_string()
                } else {
                    message.body.chars().take(90).collect()
                };
                (message.id, (message.author.clone(), snippet))
            })
            .collect();
        // Embed elements are built before the row loop: building them can
        // kick off image fetches and needs `&mut self`, which the row
        // closure can't have while it borrows the message list. The URLs
        // that got an embed are dropped from the body text (the media
        // speaks for itself — see `richtext::render_body`).
        let mut embeds: HashMap<Uuid, Vec<gpui::AnyElement>> = HashMap::new();
        let mut embedded_urls: HashMap<Uuid, Vec<String>> = HashMap::new();
        let bodies: Vec<(Uuid, String)> = self
            .chat_messages
            .iter()
            .filter(|message| !message.deleted)
            .map(|message| (message.id, message.body.clone()))
            .collect();
        for (id, body) in bodies {
            let (elements, urls) = self.render_message_embeds(&body, cx);
            embeds.insert(id, elements);
            embedded_urls.insert(id, urls);
        }
        let message_rows = self
            .chat_messages
            .iter()
            .map(|message| {
                // Only this user's avatar is known locally — everyone else
                // renders as an initial until profiles replicate too.
                let is_self = message.author == profile_name;
                let avatar = if is_self { profile_avatar.clone() } else { None };
                let initial = message
                    .author
                    .trim()
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "?".to_string());
                let mentions_me =
                    !message.deleted && !is_self && richtext::mentions_user(&message.body, &profile_name);
                let message_id = message.id;
                // Each row is its own hover group so its action bar can
                // appear on hover without any per-row state.
                let group_name: SharedString = format!("msg-{message_id}").into();
                let is_editing = self.editing.as_ref().is_some_and(|(id, _)| *id == message_id);
                let palette_open = self.reacting_to == Some(message_id) && !message.deleted;

                // The quoted line above a reply: the original's author and a
                // snippet, or an honest fallback when we don't have it.
                let quote = message.reply_to.map(|original| {
                    let (author, snippet) = quotes
                        .get(&original)
                        .cloned()
                        .unwrap_or_else(|| ("?".to_string(), "Original message not loaded".to_string()));
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(12.))
                        .text_color(theme::muted_foreground())
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(theme::icon(icons::REPLY, px(12.)).text_color(theme::faint_foreground()))
                        .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).child(author))
                        .child(snippet)
                });

                // The body slot: a delete tombstone stub, the inline editor
                // (Enter commits via key bubbling, Escape is handled by the
                // root key handler), or the normal rendered text.
                let body: gpui::AnyElement = if message.deleted {
                    div()
                        .text_size(px(13.))
                        .italic()
                        .text_color(theme::faint_foreground())
                        .child("message deleted")
                        .into_any_element()
                } else if is_editing {
                    let input = self.editing.as_ref().map(|(_, input)| input.clone()).expect("is_editing");
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .on_key_down(cx.listener(|shell, event: &KeyDownEvent, _window, cx| {
                            if event.keystroke.key == "enter" {
                                shell.commit_edit(cx);
                            }
                        }))
                        .child(input)
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme::faint_foreground())
                                .child("enter to save · escape to cancel"),
                        )
                        .into_any_element()
                } else {
                    let hidden = embedded_urls.get(&message.id).map(Vec::as_slice).unwrap_or(&[]);
                    richtext::render_body(message.id.as_u128() as u64, &message.body, hidden)
                };

                // Reaction chips: emoji + count, outlined in blurple when
                // this user is among the reactors. Click toggles.
                let chips = (!message.deleted)
                    .then(|| self.chat_reactions.get(&message_id))
                    .flatten()
                    .filter(|chips| !chips.is_empty())
                    .map(|chips| {
                        div().flex().flex_wrap().gap(px(4.)).mt(px(2.)).children(chips.iter().map(
                            |(emoji, authors)| {
                                let mine = authors.contains(&profile_name);
                                let emoji_for_click = emoji.clone();
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .px(px(7.))
                                    .py(px(2.))
                                    .rounded_full()
                                    .bg(theme::wash())
                                    .border_1()
                                    .border_color(if mine { theme::primary() } else { theme::border() })
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme::wash_strong()))
                                    .text_size(px(12.))
                                    .child(emoji.clone())
                                    .child(
                                        div()
                                            .text_color(theme::muted_foreground())
                                            .child(authors.len().to_string()),
                                    )
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(move |shell, _, _window, cx| {
                                            shell.toggle_reaction(message_id, &emoji_for_click, cx);
                                        }),
                                    )
                            },
                        ))
                    });

                // The inline emoji palette, opened by the smile action. The
                // extra flex wrapper keeps it content-sized instead of
                // stretching across the column.
                let palette = palette_open.then(|| {
                    div().flex().mt(px(4.)).child(
                        div()
                            .flex()
                            .gap(px(2.))
                            .p(px(3.))
                            .rounded_md()
                            .bg(theme::popover())
                            .border_1()
                            .border_color(theme::border())
                            .shadow_md()
                            .children(REACTION_PALETTE.iter().map(|emoji| {
                                let emoji = *emoji;
                                div()
                                    .size(px(30.))
                                    .rounded_md()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme::wash_strong()))
                                    .text_size(px(16.))
                                    .child(emoji)
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(move |shell, _, _window, cx| {
                                            shell.toggle_reaction(message_id, emoji, cx);
                                        }),
                                    )
                            })),
                    )
                });

                // The hover action bar: reply + react on every message,
                // edit/delete only on this user's own. Deleted messages get
                // no actions at all.
                let actions = (!message.deleted).then(|| {
                    let reply_message = message.clone();
                    let edit_message = message.clone();
                    let mut bar = div()
                        .absolute()
                        .top(px(-8.))
                        .right(px(16.))
                        .flex()
                        .gap(px(2.))
                        .p(px(2.))
                        .rounded_md()
                        .bg(theme::popover())
                        .border_1()
                        .border_color(theme::border())
                        .shadow_md()
                        .invisible()
                        .group_hover(group_name.clone(), |style| style.visible())
                        .child(message_action(icons::REPLY, false).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |shell, _, window, cx| {
                                shell.start_reply(reply_message.clone(), window, cx);
                            }),
                        ))
                        .child(message_action(icons::SMILE_PLUS, false).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |shell, _, _window, cx| {
                                shell.reacting_to =
                                    if shell.reacting_to == Some(message_id) { None } else { Some(message_id) };
                                cx.notify();
                            }),
                        ));
                    if is_self {
                        bar = bar
                            .child(message_action(icons::PENCIL, false).on_mouse_up(
                                MouseButton::Left,
                                cx.listener(move |shell, _, window, cx| {
                                    shell.start_edit(&edit_message, window, cx);
                                }),
                            ))
                            .child(message_action(icons::TRASH, true).on_mouse_up(
                                MouseButton::Left,
                                cx.listener(move |shell, _, _window, cx| {
                                    shell.delete_message(message_id, cx);
                                }),
                            ));
                    }
                    bar
                });

                div()
                    .relative()
                    .group(group_name)
                    .flex()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(6.))
                    .hover(|style| style.bg(theme::wash()))
                    // Discord's mention treatment: a gold wash and left
                    // border on any row that @-mentions you.
                    .when(mentions_me, |style| {
                        let mut wash = theme::mention();
                        wash.a = 0.08;
                        style.bg(wash).border_l_2().border_color(theme::mention()).pl(px(14.))
                    })
                    .children(actions)
                    .child(div().flex_none().child(theme::avatar(avatar, initial, px(36.))))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .children(quote)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_size(px(14.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(message.author.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(theme::faint_foreground())
                                            .child(format_timestamp(message.sent_at)),
                                    )
                                    .when(message.edited_at.is_some() && !message.deleted, |header| {
                                        header.child(
                                            div()
                                                .text_size(px(10.))
                                                .text_color(theme::faint_foreground())
                                                .child("(edited)"),
                                        )
                                    }),
                            )
                            .child(body)
                            .children(embeds.remove(&message.id).unwrap_or_default())
                            .children(chips)
                            .children(palette),
                    )
            })
            .collect::<Vec<_>>();

        let scrollback = div()
            .id("chat-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.chat_scroll)
            .flex()
            .flex_col()
            .py(px(12.))
            .children(empty_state)
            .children(message_rows);

        // Always-rendered 18px strip so the composer doesn't jump when the
        // first "…is typing" appears.
        let mut typing_names: Vec<&str> = self
            .typing
            .keys()
            .filter(|(typing_channel, _)| *typing_channel == channel.id)
            .map(|(_, author)| author.as_str())
            .collect();
        typing_names.sort_unstable();
        let typing_label = match typing_names.as_slice() {
            [] => String::new(),
            [one] => format!("{one} is typing…"),
            [one, two] => format!("{one} and {two} are typing…"),
            _ => "Several people are typing…".to_string(),
        };
        let typing_strip = div()
            .flex_none()
            .h(px(18.))
            .px(px(16.))
            .text_size(px(11.5))
            .text_color(theme::muted_foreground())
            .child(typing_label);

        // The "Replying to Alice ✕" bar that docks onto the composer while
        // a reply is being written. Escape (root handler) or ✕ clears it.
        let reply_bar = self.replying_to.as_ref().map(|original| {
            let author = original.author.clone();
            div()
                .flex_none()
                .mx(px(16.))
                .px(px(12.))
                .py(px(6.))
                .rounded_t_md()
                .bg(theme::card())
                .flex()
                .items_center()
                .justify_between()
                .text_size(px(12.))
                .text_color(theme::muted_foreground())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .child("Replying to")
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::foreground())
                                .child(author),
                        ),
                )
                .child(
                    div()
                        .size(px(18.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::wash_strong()))
                        .child(theme::icon(icons::X, px(12.)).text_color(theme::muted_foreground()))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|shell, _, _window, cx| {
                                shell.replying_to = None;
                                cx.notify();
                            }),
                        ),
                )
        });

        let composer = div()
            .flex_none()
            .px(px(16.))
            .pb(px(16.))
            .pt(px(2.))
            .on_key_down(cx.listener(|shell, event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key == "enter" {
                    shell.send_chat_message(cx);
                } else {
                    shell.maybe_send_typing(cx);
                }
            }))
            .child(self.message_input.clone());

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(header)
            .child(scrollback)
            .child(typing_strip)
            .children(reply_bar)
            .child(composer)
    }

}

/// One small icon button in a message's hover action bar — callers attach
/// `.on_mouse_up(...)`. `destructive` tints the icon red (the delete
/// action).
pub(super) fn message_action(icon_path: &'static str, destructive: bool) -> Div {
    let color = if destructive { theme::destructive_foreground() } else { theme::muted_foreground() };
    div()
        .size(px(26.))
        .rounded_md()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|style| style.bg(theme::wash_strong()))
        .child(theme::icon(icon_path, px(15.)).text_color(color))
}

/// A message's send time in the reader's local timezone — "Today at HH:MM"
/// for today, a full date otherwise. The author's clock stamped it (see
/// [`chat::ChatMessage::sent_at`]); at friends scale that's plenty.
pub(super) fn format_timestamp(millis: i64) -> String {
    use chrono::{Local, TimeZone};
    let Some(time) = Local.timestamp_millis_opt(millis).single() else {
        return String::new();
    };
    if time.date_naive() == Local::now().date_naive() {
        format!("Today at {}", time.format("%H:%M"))
    } else {
        time.format("%Y-%m-%d %H:%M").to_string()
    }
}
