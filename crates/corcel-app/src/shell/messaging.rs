//! Text chat: connecting to a server's relay room, the serverless
//! replication dance (broadcast + history backfill, see [`crate::chat`]),
//! unread tracking, typing indicators, the composer with reply/edit state,
//! and the chat panel that renders it all.

use super::*;

/// The quick-reaction row pinned at the top of the emoji picker — the
/// Discord staples, one click away without scrolling.
const REACTION_PALETTE: &[&str] = &["👍", "❤️", "😂", "😮", "😢", "🎉"];

/// The full picker, by category. Curated rather than exhaustive — a
/// friends-scale set that covers what people actually react with, kept
/// to single-codepoint-ish emoji that every platform's fallback font
/// renders (no ZWJ sequences, whose support is spottier).
const EMOJI_CATEGORIES: &[(&str, &[&str])] = &[
    (
        "Smileys",
        &[
            "😀", "😃", "😄", "😁", "😆", "😅", "😂", "🤣", "😊", "😇", "🙂", "😉", "😌", "😍", "🥰", "😘",
            "😋", "😛", "😜", "🤪", "🤨", "🧐", "🤓", "😎", "🥳", "😏", "😒", "😞", "😔", "😟", "😕", "🙁",
            "😣", "😖", "😫", "😩", "🥺", "😢", "😭", "😤", "😠", "😡", "🤬", "🤯", "😳", "🥵", "🥶", "😱",
            "😨", "😰", "😥", "🤗", "🤔", "🤭", "🤫", "🤥", "😶", "😐", "😑", "😬", "🙄", "😯", "😴", "🤤",
            "😷", "🤒", "🤕", "🤢", "🤮", "🥴", "😵", "🤠", "🤑", "💀", "👻", "👽", "🤖", "💩", "🤡",
        ],
    ),
    (
        "Gestures",
        &[
            "👍", "👎", "👊", "✊", "🤛", "🤜", "👏", "🙌", "👐", "🤲", "🤝", "🙏", "✌️", "🤞", "🤟", "🤘",
            "👌", "🤌", "🤏", "👈", "👉", "👆", "👇", "☝️", "✋", "🤚", "🖐️", "🖖", "👋", "🤙", "💪", "🖕",
        ],
    ),
    (
        "Hearts",
        &[
            "❤️", "🧡", "💛", "💚", "💙", "💜", "🖤", "🤍", "🤎", "💔", "❣️", "💕", "💞", "💓", "💗", "💖",
            "💘", "💝", "💟", "♥️", "🔥", "✨", "⭐", "🌟", "💫", "💥", "💯", "💢", "💦", "💨",
        ],
    ),
    (
        "Animals",
        &[
            "🐶", "🐱", "🐭", "🐹", "🐰", "🦊", "🐻", "🐼", "🐨", "🐯", "🦁", "🐮", "🐷", "🐸", "🐵", "🙈",
            "🙉", "🙊", "🐔", "🐧", "🐦", "🦆", "🦅", "🦉", "🐺", "🐗", "🐴", "🦄", "🐝", "🐛", "🦋", "🐌",
        ],
    ),
    (
        "Food",
        &[
            "🍏", "🍎", "🍐", "🍊", "🍋", "🍌", "🍉", "🍇", "🍓", "🍈", "🍒", "🍑", "🥭", "🍍", "🥥", "🥝",
            "🍅", "🥑", "🌽", "🥕", "🍕", "🍔", "🍟", "🌭", "🥪", "🌮", "🍜", "🍣", "🍦", "🍩", "🍪", "🎂",
            "🍿", "☕", "🍺", "🍻", "🥂", "🍷", "🥃", "🧉",
        ],
    ),
    (
        "Activities",
        &[
            "⚽", "🏀", "🏈", "⚾", "🎾", "🏐", "🎱", "🏓", "🎯", "🎮", "🕹️", "🎲", "🧩", "🎬", "🎤", "🎧",
            "🎸", "🎹", "🥁", "🎺", "🎻", "🎨", "🏆", "🥇", "🥈", "🥉", "🏅", "🎪", "🚀", "✈️", "🚗", "🏁",
        ],
    ),
    (
        "Symbols",
        &[
            "✅", "❌", "❓", "❗", "⚠️", "🚫", "💤", "🔔", "🔕", "🎉", "🎊", "🎈", "🎁", "🔑", "🔒", "🔓",
            "💡", "🔦", "🔧", "🔨", "💣", "🧨", "📌", "📎", "✏️", "📝", "📖", "💰", "💎", "🧠", "👀", "🗿",
        ],
    ),
];

impl Shell {
    pub(super) fn open_text_channel(&mut self, channel: ChannelInfo, cx: &mut Context<Self>) {
        let Screen::Server { id, .. } = &self.screen else { return };
        let id = *id;
        self.chat_messages =
            self.store.as_ref().and_then(|store| store.messages(channel.id).ok()).unwrap_or_default();
        self.reset_composer_state();
        self.stop_video_embeds(cx);
        self.reload_reactions(channel.id);
        self.refresh_threads(channel.id);
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
        self.open_thread = None;
        self.thread_messages.clear();
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
                        .map(|server| server.link.endpoint_addr().ok())
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
                // A (re)connect resets what we know about this server's
                // voice rosters: everyone's peer ids are new, and we missed
                // any join/leave while disconnected. Others repopulate it
                // by reacting to our PeerJoined (below); we re-announce our
                // own presence in case we reconnected mid-call.
                self.clear_voice_occupants(server_id);
                // Same reset for the member panel's online list: its peer
                // ids are from the previous connection too. It repopulates
                // as everyone's Profile cards arrive.
                self.room_members.retain(|(sid, _), _| *sid != server_id);
                if let Some(payload) = self.my_voice_presence(server_id) {
                    self.send_room(server_id, payload, None);
                }
                let my_profile = self.my_profile_payload();
                self.send_room(server_id, my_profile, None);
                let Some(peer) = peers.first().copied() else { return };
                let since = self.history_since(server_id);
                self.send_room(server_id, ChatPayload::HistoryRequest { since }, Some(peer));
            }
            // Someone entered the room: if we're in one of this server's
            // voice channels, tell them directly so late joiners see who's
            // already on a call (broadcasts only reach people already
            // present when we joined the channel). Their arrival is also
            // our chance to catch up *from* them: if this side joined an
            // empty room (nobody online to answer the RoomWelcome history
            // request), this is the first moment history can flow at all —
            // without it, a member who rejoins a quiet server stays empty
            // until their next reconnect. Redundant when we're already
            // caught up: the overlap window keeps the batch tiny and ids
            // dedupe the rest.
            ServerMessage::PeerJoined { peer } => {
                if let Some(payload) = self.my_voice_presence(server_id) {
                    self.send_room(server_id, payload, Some(peer));
                }
                let my_profile = self.my_profile_payload();
                self.send_room(server_id, my_profile, Some(peer));
                let since = self.history_since(server_id);
                self.send_room(server_id, ChatPayload::HistoryRequest { since }, Some(peer));
            }
            // Someone's room connection dropped: whatever voice channel
            // they were in, they can no longer be signaling in it — this is
            // the crash-safety sweep for members who never said
            // `present: false`.
            ServerMessage::PeerLeft { peer } => {
                let before = self.voice_occupants.len() + self.room_members.len();
                self.voice_occupants.retain(|(_, occupant), _| *occupant != peer);
                self.room_members.remove(&(server_id, peer));
                if self.voice_occupants.len() + self.room_members.len() != before {
                    cx.notify();
                }
            }
            ServerMessage::Published { from, payload } => match serde_json::from_value(payload) {
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
                Ok(ChatPayload::VoicePresence { channel, author, present }) => {
                    self.note_voice_presence(channel, from, author, present, cx);
                }
                Ok(ChatPayload::Profile { author, avatar, bio }) => self.absorb_profile(server_id, from, author, avatar, bio, cx),
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
                // The direct copy sent to a peer who entered the room while
                // we were already in a voice channel (see PeerJoined above).
                Ok(ChatPayload::VoicePresence { channel, author, present }) => {
                    self.note_voice_presence(channel, from, author, present, cx);
                }
                // The direct copy of a member's profile card, sent to us
                // when we entered a room they were already in.
                Ok(ChatPayload::Profile { author, avatar, bio }) => self.absorb_profile(server_id, from, author, avatar, bio, cx),
                // Everything else (Typing, Edit, …) is broadcast-only; a
                // direct one is a peer bug, and unparseable JSON is a newer
                // build's payload — both dropped.
                _ => {}
            },
            _ => {}
        }
    }

    /// The `since` a history request for this server should carry: just
    /// behind our newest stored message (see [`chat::HISTORY_OVERLAP_MILLIS`]),
    /// or 0 — "send everything" — when the store is empty (fresh join, or
    /// a rejoin after leaving wiped the server's history).
    fn history_since(&self, server_id: Uuid) -> i64 {
        self.store
            .as_ref()
            .and_then(|store| store.latest_timestamp(server_id).ok().flatten())
            .map(|latest| latest - chat::HISTORY_OVERLAP_MILLIS)
            .unwrap_or(0)
    }

    /// This user's profile card for the wire, with the avatar encoded on
    /// first use and cached for every later room join/peer arrival (the
    /// encode reads and rescales an image file — cheap once, not per peer).
    fn my_profile_payload(&mut self) -> ChatPayload {
        if self.encoded_avatar.is_none() {
            let encoded = self
                .profile
                .as_ref()
                .and_then(|profile| profile.avatar_path.as_deref())
                .and_then(profile::encode_avatar);
            self.encoded_avatar = Some(encoded);
        }
        ChatPayload::Profile {
            author: self.my_name(),
            avatar: self.encoded_avatar.clone().flatten(),
            bio: self.profile.as_ref().and_then(|profile| profile.bio.clone()),
        }
    }

    /// Re-announces this user's card to every connected room — called
    /// after a profile edit so the new photo/bio propagates immediately
    /// instead of waiting for the next room (re)join.
    pub(super) fn broadcast_profile(&mut self) {
        self.encoded_avatar = None; // re-encode: the avatar may have changed
        let payload = self.my_profile_payload();
        let server_ids: Vec<Uuid> = self.rooms.keys().copied().collect();
        for server_id in server_ids {
            self.send_room(server_id, payload.clone(), None);
        }
    }

    /// Caches a peer's replicated card: avatar to disk (see
    /// [`profile::save_peer_avatar`]) and bio in memory. Our own name is
    /// ignored (we already have the originals). A card with no avatar
    /// drops any cached one — that's how photo removal propagates.
    pub(super) fn absorb_profile(
        &mut self,
        server_id: Uuid,
        from: PeerId,
        author: String,
        avatar: Option<String>,
        bio: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if author == self.my_name() {
            return;
        }
        // Their card doubles as "I'm online": every member announces it on
        // room entry and directly to each later arrival, so it reaches us
        // exactly once per session of theirs overlapping ours.
        self.room_members.insert((server_id, from), author.clone());
        match bio {
            Some(bio) => drop(self.peer_bios.insert(author.clone(), bio)),
            None => drop(self.peer_bios.remove(&author)),
        }
        match avatar {
            Some(avatar) => {
                let Some(path) = profile::save_peer_avatar(&author, &avatar) else { return };
                if self.peer_avatars.get(&author) != Some(&path) {
                    self.peer_avatars.insert(author, path);
                }
            }
            None => {
                profile::remove_peer_avatar(&author);
                self.peer_avatars.remove(&author);
            }
        }
        cx.notify();
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
        // Thread replies don't appear in the scrollback, but they change
        // the "N replies" chips and the open panel.
        if let Some(channel_id) = viewing {
            self.refresh_threads(channel_id);
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

    /// Records someone joining or leaving a voice channel, keyed by their
    /// room peer id (see the `voice_occupants` field for why).
    pub(super) fn note_voice_presence(
        &mut self,
        channel: Uuid,
        peer: PeerId,
        author: String,
        present: bool,
        cx: &mut Context<Self>,
    ) {
        if author == self.my_name() {
            return;
        }
        let changed = if present {
            self.voice_occupants.insert((channel, peer), author).is_none()
        } else {
            self.voice_occupants.remove(&(channel, peer)).is_some()
        };
        if changed {
            cx.notify();
        }
    }

    /// Forgets everything known about who's in this server's voice channels
    /// — used when (re)entering its room, since occupant entries are keyed
    /// by peer ids from the previous connection.
    pub(super) fn clear_voice_occupants(&mut self, server_id: Uuid) {
        let channels: Vec<Uuid> = self
            .servers
            .iter()
            .find(|s| s.link.id == server_id)
            .map(|s| s.link.channels.iter().map(|c| c.id).collect())
            .unwrap_or_default();
        self.voice_occupants.retain(|(channel, _), _| !channels.contains(channel));
    }

    /// The VoicePresence payload describing this user's own current call on
    /// `server_id`, if they're connected to one of its voice channels.
    pub(super) fn my_voice_presence(&self, server_id: Uuid) -> Option<ChatPayload> {
        let call = self.call.as_ref()?;
        if call.server_id != server_id || !matches!(call.status, ChannelStatus::Connected(_)) {
            return None;
        }
        Some(ChatPayload::VoicePresence {
            channel: call.channel.id,
            author: self.my_name(),
            present: true,
        })
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
        let reply_to = self.replying_to.take().map(|original| original.id);
        self.send_message_with(body, reply_to, cx);
        self.message_input.update(cx, |input, cx| input.clear(cx));
    }

    /// Sends `body` as this user in the on-screen channel — the shared
    /// tail of the composer, the GIF picker, and anything else that
    /// produces a message.
    pub(super) fn send_message_body(&mut self, body: String, cx: &mut Context<Self>) {
        self.send_message_with(body, None, cx);
    }

    fn send_message_with(&mut self, body: String, reply_to: Option<Uuid>, cx: &mut Context<Self>) {
        let Some((server_id, channel_id)) = self.visible_text_channel() else { return };
        let author = self.profile.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "?".to_string());
        let message = ChatMessage {
            id: Uuid::new_v4(),
            channel: channel_id,
            author,
            sent_at: chat::now_millis(),
            body,
            reply_to,
            edited_at: None,
            deleted: false,
            thread_root: None,
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
        self.chat_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Opens `root`'s thread in the right-hand panel (replacing the member
    /// panel while open) and loads its replies. If the clicked message is
    /// itself a thread reply this flattens to its root — threads are one
    /// level deep, always (Slack's rule).
    pub(super) fn open_thread(&mut self, root: ChatMessage, cx: &mut Context<Self>) {
        let root = match root.thread_root {
            Some(root_id) => self
                .chat_messages
                .iter()
                .find(|message| message.id == root_id)
                .cloned()
                .unwrap_or(root),
            None => root,
        };
        self.thread_messages =
            self.store.as_ref().and_then(|store| store.thread_messages(root.id).ok()).unwrap_or_default();
        self.open_thread = Some(root);
        cx.notify();
    }

    pub(super) fn close_thread(&mut self, cx: &mut Context<Self>) {
        if self.open_thread.take().is_some() {
            self.thread_messages.clear();
            cx.notify();
        }
    }

    /// Reloads the on-screen channel's per-thread reply counts (see
    /// [`Shell::thread_counts`]) and, if a thread panel is open, its
    /// replies — called whenever stored messages change.
    pub(super) fn refresh_threads(&mut self, channel: Uuid) {
        self.thread_counts = self
            .store
            .as_ref()
            .and_then(|store| store.thread_summaries(channel).ok())
            .unwrap_or_default();
        if let Some(root) = &self.open_thread {
            self.thread_messages =
                self.store.as_ref().and_then(|store| store.thread_messages(root.id).ok()).unwrap_or_default();
        }
    }

    /// Sends the thread composer's draft as a reply in the open thread —
    /// a normal message with `thread_root` set, stored and broadcast
    /// exactly like [`Self::send_chat_message`]'s.
    pub(super) fn send_thread_message(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.open_thread.clone() else { return };
        let body = self.thread_input.read(cx).content.trim().to_string();
        if body.is_empty() {
            return;
        }
        let Some((server_id, _)) = self.visible_text_channel() else { return };
        let author = self.profile.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "?".to_string());
        let message = ChatMessage {
            id: Uuid::new_v4(),
            channel: root.channel,
            author,
            sent_at: chat::now_millis(),
            body,
            reply_to: None,
            edited_at: None,
            deleted: false,
            thread_root: Some(root.id),
        };
        if let Some(store) = &self.store {
            let _ = store.insert_message(server_id, &message);
        }
        self.send_room(server_id, ChatPayload::Message(message.clone()), None);
        self.thread_messages.push(message);
        self.refresh_threads(root.channel);
        self.thread_input.update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    /// The `@word` currently being typed under the composer's caret, as
    /// `(byte offset of the '@', query so far)` — the trigger for the
    /// mention autocomplete. The `@` must start a word, and the query runs
    /// caret-backwards to it with no whitespace in between.
    pub(super) fn active_mention(&self, cx: &Context<Self>) -> Option<(usize, String)> {
        let input = self.message_input.read(cx);
        let content = input.content.to_string();
        let head = content.get(..input.cursor())?;
        let at = head.rfind('@')?;
        if head[..at].chars().last().is_some_and(|c| c.is_alphanumeric()) {
            return None;
        }
        let query = &head[at + 1..];
        if query.contains(char::is_whitespace) {
            return None;
        }
        Some((at, query.to_string()))
    }

    /// Everyone this server knows who matches `query` (case-insensitive
    /// prefix): online room members first, then everyone who ever wrote a
    /// stored message. Capped so the popup stays a popup.
    pub(super) fn mention_candidates(&self, server_id: Uuid, query: &str) -> Vec<String> {
        let query = query.to_lowercase();
        let mut names: Vec<String> = self
            .room_members
            .iter()
            .filter(|((sid, _), _)| *sid == server_id)
            .map(|(_, name)| name.clone())
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        names.dedup();
        let offline = self
            .store
            .as_ref()
            .and_then(|store| store.authors(server_id).ok())
            .unwrap_or_default();
        for name in offline {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let me = self.my_name();
        names.retain(|name| *name != me && name.to_lowercase().starts_with(&query));
        names.truncate(8);
        names
    }

    /// Replaces the `@query` being typed with `@name ` and refocuses the
    /// caret (at the end — fine in practice, mentions are typed at the
    /// tail of a draft).
    pub(super) fn complete_mention(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some((at, _)) = self.active_mention(cx) else { return };
        let (content, cursor) = {
            let input = self.message_input.read(cx);
            (input.content.to_string(), input.cursor())
        };
        let completed = format!("{}@{} {}", &content[..at], name, &content[cursor..]);
        self.message_input.update(cx, |input, cx| input.set_content(completed, cx));
        self.mention_selected = 0;
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
                        .max_w(px(600.))
                        .text_center()
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
                // This user's avatar comes from their profile; everyone
                // else's from the replicated cache (see
                // [`Shell::absorb_profile`]), falling back to an initial.
                let is_self = message.author == profile_name;
                let avatar = if is_self {
                    profile_avatar.clone()
                } else {
                    self.peer_avatars.get(&message.author).cloned()
                };
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

                // The emoji picker, opened by the smile action: the staple
                // quick row pinned on top, then every category in one
                // scrollable grid. The extra flex wrapper keeps it
                // content-sized instead of stretching across the column.
                let palette = palette_open.then(|| {
                    let cell = |emoji: &'static str, cx: &mut Context<Self>| {
                        div()
                            .size(px(30.))
                            .rounded_md()
                            .flex()
                            .flex_none()
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
                    };
                    let quick_row = div()
                        .flex()
                        .gap(px(2.))
                        .pb(px(4.))
                        .border_b_1()
                        .border_color(theme::border())
                        .children(REACTION_PALETTE.iter().map(|emoji| cell(emoji, cx)));
                    // Collected eagerly: a lazy iterator would hold the
                    // `cx` borrow into the container build below.
                    let sections: Vec<_> = EMOJI_CATEGORIES.iter().map(|(category, emojis)| {
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .pt(px(6.))
                                    .pb(px(2.))
                                    .text_size(px(10.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::muted_foreground())
                                    .child(*category),
                            )
                            .child(
                                div().flex().flex_wrap().gap(px(2.)).children(
                                    emojis.iter().map(|emoji| cell(emoji, cx)).collect::<Vec<_>>(),
                                ),
                            )
                    }).collect();
                    div().flex().mt(px(4.)).child(
                        div()
                            .id(SharedString::from(format!("emoji-picker-{message_id}")))
                            .flex()
                            .flex_col()
                            .w(px(292.))
                            .h(px(260.))
                            .p(px(6.))
                            .rounded_lg()
                            .bg(theme::raised_fill())
                            .border_1()
                            .border_color(theme::glass_edge())
                            .shadow_md()
                            .on_mouse_down_out(cx.listener(|shell, _, _window, cx| {
                                if shell.reacting_to.take().is_some() {
                                    cx.notify();
                                }
                            }))
                            .child(quick_row)
                            .child(
                                div()
                                    .id(SharedString::from(format!("emoji-scroll-{message_id}")))
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .flex()
                                    .flex_col()
                                    .children(sections),
                            )
                            .with_animation(
                                SharedString::from(format!("emoji-picker-in-{message_id}")),
                                Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint()),
                                |picker, delta| picker.opacity(delta).mt(px(6. * (1. - delta))),
                            ),
                    )
                });

                // The Slack-style thread indicator: "N replies · last at"
                // under any message a thread hangs off. Click to open the
                // panel. Derived counts only — nothing stored on the root.
                let thread_chip = (!message.deleted)
                    .then(|| self.thread_counts.get(&message_id).copied())
                    .flatten()
                    .map(|(count, last_at)| {
                        let root_for_click = message.clone();
                        div().flex().mt(px(2.)).child(
                            div()
                                .id(SharedString::from(format!("thread-chip-{message_id}")))
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .px(px(8.))
                                .py(px(3.))
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|style| style.bg(theme::wash_strong()))
                                .child(
                                    theme::icon(icons::MESSAGE_SQUARE, px(13.))
                                        .text_color(theme::primary()),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme::primary())
                                        .child(if count == 1 {
                                            "1 reply".to_string()
                                        } else {
                                            format!("{count} replies")
                                        }),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(theme::muted_foreground())
                                        .child(format_timestamp(last_at)),
                                )
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(move |shell, _, _window, cx| {
                                        shell.open_thread(root_for_click.clone(), cx);
                                    }),
                                ),
                        )
                    });

                // The hover action bar: reply + react on every message,
                // edit/delete only on this user's own. Deleted messages get
                // no actions at all.
                let actions = (!message.deleted).then(|| {
                    let reply_message = message.clone();
                    let thread_message = message.clone();
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
                        .child(message_action(icons::MESSAGE_SQUARE, false).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |shell, _, _window, cx| {
                                shell.open_thread(thread_message.clone(), cx);
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

                let menu_message = message.clone();
                let menu_is_self = is_self;
                div()
                    .relative()
                    .group(group_name)
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |shell, event, _window, cx| {
                            if menu_message.deleted {
                                return;
                            }
                            let mut items = vec![
                                ContextMenuItem::new(
                                    "Reply",
                                    icons::REPLY,
                                    ContextAction::Reply { message: menu_message.clone() },
                                ),
                                ContextMenuItem::new(
                                    "Reply in thread",
                                    icons::MESSAGE_SQUARE,
                                    ContextAction::ReplyInThread { message: menu_message.clone() },
                                ),
                                ContextMenuItem::new(
                                    "Add reaction",
                                    icons::SMILE_PLUS,
                                    ContextAction::AddReaction { message: menu_message.id },
                                ),
                                ContextMenuItem::new(
                                    "Copy text",
                                    icons::LINK,
                                    ContextAction::CopyMessageText { body: menu_message.body.clone() },
                                ),
                            ];
                            if menu_is_self {
                                items.push(ContextMenuItem::new(
                                    "Edit message",
                                    icons::PENCIL,
                                    ContextAction::EditMessage { message: menu_message.clone() },
                                ));
                                items.push(
                                    ContextMenuItem::new(
                                        "Delete message",
                                        icons::TRASH,
                                        ContextAction::DeleteMessage { message: menu_message.id },
                                    )
                                    .destructive(),
                                );
                            }
                            shell.open_context_menu(event, items, cx);
                        }),
                    )
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
                            .children(thread_chip)
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
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
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
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .child(div().flex_none().child("Replying to"))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
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

        // The @mention autocomplete, floating as a card just above the
        // composer whenever an `@word` is under the caret and someone
        // matches it. ↑/↓ move, Enter/Tab complete, a click completes too.
        let mention_popup = self
            .visible_text_channel()
            .and_then(|(server_id, _)| {
                let (_, query) = self.active_mention(cx)?;
                let candidates = self.mention_candidates(server_id, &query);
                if candidates.is_empty() {
                    return None;
                }
                let selected = self.mention_selected.min(candidates.len() - 1);
                let rows: Vec<_> = candidates
                    .iter()
                    .enumerate()
                    .map(|(index, name)| {
                        let avatar = self.peer_avatars.get(name).cloned();
                        let initial = name
                            .trim()
                            .chars()
                            .next()
                            .map(|c| c.to_uppercase().to_string())
                            .unwrap_or_else(|| "?".to_string());
                        let name_for_click = name.clone();
                        div()
                            .id(SharedString::from(format!("mention-{index}")))
                            .px(px(8.))
                            .py(px(5.))
                            .rounded_md()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .cursor_pointer()
                            .when(index == selected, |row| row.bg(theme::wash_strong()))
                            .hover(|style| style.bg(theme::wash_strong()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |shell, _, _window, cx| {
                                    shell.complete_mention(&name_for_click, cx);
                                }),
                            )
                            .child(theme::avatar(avatar, initial, px(22.)))
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_size(px(13.))
                                    .child(name.clone()),
                            )
                    })
                    .collect();
                Some(
                    div().mx(px(16.)).flex().child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .p(px(4.))
                            .mb(px(4.))
                            .min_w(px(220.))
                            .rounded_lg()
                            .bg(theme::raised_fill())
                            .border_1()
                            .border_color(theme::glass_edge())
                            .shadow_md()
                            .children(rows)
                            .with_animation(
                                "mention-popup-in",
                                Animation::new(Duration::from_millis(120)).with_easing(ease_out_quint()),
                                |popup, delta| popup.opacity(delta).mt(px(4. * (1. - delta))),
                            ),
                    ),
                )
            });

        let composer = div()
            .flex_none()
            .px(px(16.))
            .pb(px(16.))
            .pt(px(2.))
            .on_key_down(cx.listener(|shell, event: &KeyDownEvent, _window, cx| {
                let key = event.keystroke.key.as_str();
                // While the mention popup is up it owns Enter/Tab/↑/↓;
                // everything else falls through to the normal composer
                // behavior (the input itself already handled the edit).
                if let Some((server_id, _)) = shell.visible_text_channel() {
                    if let Some((_, query)) = shell.active_mention(cx) {
                        let candidates = shell.mention_candidates(server_id, &query);
                        if !candidates.is_empty() {
                            let selected = shell.mention_selected.min(candidates.len() - 1);
                            match key {
                                "enter" | "tab" => {
                                    let pick = candidates[selected].clone();
                                    shell.complete_mention(&pick, cx);
                                    return;
                                }
                                "up" => {
                                    shell.mention_selected = selected.saturating_sub(1);
                                    cx.notify();
                                    return;
                                }
                                "down" => {
                                    shell.mention_selected = (selected + 1).min(candidates.len() - 1);
                                    cx.notify();
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                if key == "enter" {
                    shell.send_chat_message(cx);
                } else {
                    shell.maybe_send_typing(cx);
                }
            }))
            .child(
                div().flex().items_center().gap(px(8.)).child(div().flex_1().min_w_0().child(self.message_input.clone())).child(
                    div()
                        .id("gif-picker-button")
                        .flex_none()
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(9.))
                        .bg(if self.gif_picker_open { theme::wash_strong() } else { theme::wash() })
                        .border_1()
                        .border_color(theme::border())
                        .text_size(px(11.5))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::muted_foreground())
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::wash_strong()).text_color(theme::foreground()))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|shell, _, _window, cx| shell.toggle_gif_picker(cx)),
                        )
                        .child("GIF"),
                ),
            );

        let gif_panel = self.gif_picker_open.then(|| self.render_gif_picker(cx));

        let chat_column = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(header)
            .child(scrollback)
            .child(typing_strip)
            .children(reply_bar)
            .children(mention_popup)
            .children(gif_panel)
            .child(composer);

        // The thread panel takes the member panel's slot while open —
        // both at once would crowd the chat out of narrow windows.
        let side_panel: AnyElement = match self.open_thread.clone() {
            Some(root) => self
                .render_thread_panel(root, profile, cx)
                .with_animation(
                    "thread-panel-in",
                    Animation::new(Duration::from_millis(220)).with_easing(ease_out_quint()),
                    |panel, delta| panel.w(px(320. * delta)),
                )
                .into_any_element(),
            None => self.render_member_panel(profile, cx).into_any_element(),
        };

        div().flex_1().min_w_0().h_full().flex().child(chat_column).child(side_panel)
    }

    /// The Slack-style thread panel: the root message in full on top, its
    /// replies below, and a composer of its own — the channel stays
    /// visible and usable to its left.
    fn render_thread_panel(&mut self, root: ChatMessage, profile: &Profile, cx: &mut Context<Self>) -> Div {
        let render_entry = |shell: &Self, message: &ChatMessage, is_root: bool| {
            let is_self = message.author == profile.name;
            let avatar = if is_self {
                profile.avatar_path.clone()
            } else {
                shell.peer_avatars.get(&message.author).cloned()
            };
            let initial = message
                .author
                .trim()
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "?".to_string());
            let body: AnyElement = if message.deleted {
                div()
                    .text_size(px(13.))
                    .text_color(theme::faint_foreground())
                    .italic()
                    .child("message deleted")
                    .into_any_element()
            } else {
                div().text_size(px(13.5)).child(message.body.clone()).into_any_element()
            };
            div()
                .px(px(12.))
                .py(px(6.))
                .flex()
                .gap(px(8.))
                .when(is_root, |entry| entry.pb(px(10.)))
                .child(div().flex_none().child(theme::avatar(avatar, initial, px(28.))))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .items_baseline()
                                .gap(px(6.))
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_size(px(13.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(message.author.clone()),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(10.5))
                                        .text_color(theme::faint_foreground())
                                        .child(format_timestamp(message.sent_at)),
                                ),
                        )
                        .child(body),
                )
        };

        let reply_count = self.thread_messages.len();
        let replies: Vec<_> =
            self.thread_messages.iter().map(|message| render_entry(self, message, false)).collect();

        let header = div()
            .h(px(44.))
            .flex_none()
            .px(px(12.))
            .border_b_1()
            .border_color(theme::border())
            .flex()
            .items_center()
            .justify_between()
            .child(div().text_size(px(14.)).font_weight(FontWeight::BOLD).child("Thread"))
            .child(
                theme::icon_button("close-thread", icons::X, "Close thread").on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|shell, _, _window, cx| shell.close_thread(cx)),
                ),
            );

        let divider = div()
            .px(px(12.))
            .py(px(4.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().text_size(px(11.)).text_color(theme::muted_foreground()).child(if reply_count == 1 {
                "1 reply".to_string()
            } else {
                format!("{reply_count} replies")
            }))
            .child(div().flex_1().h(px(1.)).bg(theme::border()));

        let composer = div()
            .flex_none()
            .px(px(10.))
            .pb(px(10.))
            .pt(px(4.))
            .on_key_down(cx.listener(|shell, event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key == "enter" {
                    shell.send_thread_message(cx);
                }
            }))
            .child(self.thread_input.clone());

        div()
            .w(px(320.))
            .h_full()
            .flex_none()
            .overflow_hidden()
            .border_l_1()
            .border_color(theme::border())
            .bg(theme::card())
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("thread-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .pt(px(6.))
                    .child(render_entry(self, &root, true))
                    .child(divider)
                    .children(replies),
            )
            .child(composer)
    }

    /// The member panel on a text channel's right edge. A serverless
    /// server has no member registry, so "members" is what this machine
    /// can actually vouch for: everyone whose Profile card arrived over
    /// the live room is Online; everyone else who has ever written a
    /// stored message shows under Offline. Bios (when replicated) ride
    /// along as tooltips.
    fn render_member_panel(&mut self, profile: &Profile, cx: &mut Context<Self>) -> Div {
        let Screen::Server { id: server_id, .. } = self.screen else { return div() };

        let mut online: Vec<String> = self
            .room_members
            .iter()
            .filter(|((sid, _), _)| *sid == server_id)
            .map(|(_, name)| name.clone())
            .collect();
        // This user is always in the online section: the app being open
        // *is* their presence, and gating it on the room connection's
        // current state made them vanish from their own member list
        // whenever the relay was still (re)connecting.
        online.push(profile.name.clone());
        online.sort_by_key(|name| name.to_lowercase());
        online.dedup();

        let mut offline: Vec<String> = self
            .store
            .as_ref()
            .and_then(|store| store.authors(server_id).ok())
            .unwrap_or_default();
        offline.retain(|name| !online.contains(name));

        let section_header = |label: String| {
            div()
                .px(px(10.))
                .pt(px(14.))
                .pb(px(4.))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::muted_foreground())
                .child(label)
        };
        let mut row_index = 0u64;
        let mut member_row = |name: String, is_online: bool, shell: &Self, cx: &mut Context<Self>| {
            let is_self = name == profile.name;
            let avatar = if is_self {
                profile.avatar_path.clone()
            } else {
                shell.peer_avatars.get(&name).cloned()
            };
            let initial = name
                .trim()
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "?".to_string());
            let bio = if is_self {
                profile.bio.clone()
            } else {
                shell.peer_bios.get(&name).cloned()
            };
            row_index += 1;
            let name_for_menu = name.clone();
            let row = div()
                .id(SharedString::from(format!("member-{row_index}")))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |shell, event, _window, cx| {
                        shell.open_context_menu(
                            event,
                            vec![
                                ContextMenuItem::new(
                                    "Mention",
                                    icons::REPLY,
                                    ContextAction::MentionUser { author: name_for_menu.clone() },
                                ),
                                ContextMenuItem::new(
                                    "View profile",
                                    icons::USER,
                                    ContextAction::ViewProfile { author: name_for_menu.clone() },
                                )
                                .soon(),
                            ],
                            cx,
                        );
                    }),
                )
                .mx(px(6.))
                .px(px(6.))
                .py(px(3.))
                .rounded_md()
                .flex()
                .items_center()
                .gap(px(8.))
                .hover(|style| style.bg(theme::wash()))
                .child(
                    div()
                        .flex_none()
                        .when(!is_online, |avatar| avatar.opacity(0.45))
                        .child(theme::avatar(avatar, initial, px(26.))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(13.))
                        .text_color(if is_online { theme::foreground() } else { theme::faint_foreground() })
                        .child(name),
                );
            match bio.filter(|bio| !bio.is_empty()) {
                Some(bio) => row.tooltip(theme::tooltip(bio)),
                None => row,
            }
        };

        let online_rows: Vec<_> = online.iter().map(|name| member_row(name.clone(), true, self, cx)).collect();
        let offline_rows: Vec<_> =
            offline.iter().map(|name| member_row(name.clone(), false, self, cx)).collect();

        div()
            .w(px(196.))
            .h_full()
            .flex_none()
            .border_l_1()
            .border_color(theme::border())
            .bg(theme::card())
            .child(
                div()
                    .id("member-panel-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .pb(px(12.))
                    .child(section_header(format!("Online — {}", online.len())))
                    .children(online_rows)
                    .children((!offline.is_empty()).then(|| section_header(format!("Offline — {}", offline.len()))))
                    .children(offline_rows),
            )
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
