mod assets;
mod chat;
mod invite;
mod profile;
mod richtext;
mod runtime;
mod session;
mod store;
mod text_input;
mod theme;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt, App, Application, Bounds, ClipboardItem, Context, Div, Entity, Focusable, FontWeight,
    ImageSource, KeyDownEvent, MouseButton, MouseUpEvent, ObjectFit, PathPromptOptions, Render, RenderImage, Rgba,
    ScrollHandle, SharedString, Stateful, TitlebarOptions, Transformation, Window, WindowBounds, WindowOptions,
    deferred, div, img, prelude::*, px, radians, size,
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use assets::icons;
use chat::{ChatMessage, ChatPayload, ReactionRow};
use corcel_signal::{ClientMessage, PeerId, ServerMessage};
use invite::{ChannelInfo, ChannelKind, ServerLink};
use profile::Profile;
use session::{CallSession, CameraHandle, ScreenShareHandle};
use store::SavedServer;
use text_input::TextInput;
use theme::ButtonVariant;

/// The V4L2 device [`Shell::toggle_camera_clicked`] captures from. No device
/// picker yet — first-pass wiring just needs one working camera.
const CAMERA_DEVICE: &str = "/dev/video0";

/// Fixed widths of the shell's chrome columns. Shared constants so nothing
/// (like the old call dock's `72 + 240 - 24` magic width) can silently desync
/// from the columns it's aligned against.
const RAIL_WIDTH: f32 = 72.;
const SIDEBAR_WIDTH: f32 = 240.;

/// The fixed reaction palette — no emoji picker yet, these six cover the
/// Discord staples. Each renders as an inline button under a message when
/// its "add reaction" action is clicked.
const REACTION_PALETTE: &[&str] = &["👍", "❤️", "😂", "😮", "😢", "🎉"];

/// Screen-share state for a connected call. `Pending` covers the window
/// between the user clicking "Share Screen" and [`session::start_screen_share`]
/// actually resolving — that round trip includes the portal's picker dialog,
/// which waits on the user and can take several seconds. Without a distinct
/// `Pending` state, [`Shell::share_screen_clicked`] would only be able to
/// check "is there a handle yet", which stays `false` for that whole window;
/// an impatient extra click would then race a second portal session and a
/// second GStreamer/VAAPI encode pipeline into existence alongside the
/// first, rather than being ignored.
enum SharingState {
    Idle,
    Pending,
    Active(ScreenShareHandle),
}

/// Camera state for a connected call — same shape as [`SharingState`] and
/// for the same reason: [`Shell::toggle_camera_clicked`] needs a
/// synchronous `Pending` flip to close the re-entry window before
/// [`session::start_camera`] resolves, even though that window is normally
/// much shorter than screen share's (no portal picker dialog to wait on).
enum CameraState {
    Idle,
    Pending,
    Active(CameraHandle),
}

/// The stage's video pane, isolated into its own entity so that a 30fps
/// stream of `cx.notify()` calls re-renders *only* this view — not the whole
/// [`Shell`] tree (rail, channel list, member list, ...) once per frame,
/// which is what happened when `remote_frame` lived directly on the shell.
///
/// The frame is a single slot fed by whichever video track last produced a
/// frame (see [`CallSession::remote_video`]) — fine while at most one video
/// track is ever in flight, but once camera and screen share can both be
/// active (from the same peer or different ones) they'll overwrite each
/// other with no way to tell them apart. Not fixed here; revisit if/when
/// that's actually needed.
struct VideoSurface {
    frame: Option<Arc<RenderImage>>,
    /// Profile bits for the audio-only placeholder tile, copied in at
    /// creation so this view renders without reaching back into the shell.
    name: SharedString,
    avatar: Option<PathBuf>,
    initial: SharedString,
}

impl Render for VideoSurface {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match self.frame.clone() {
            Some(frame) => div()
                .size_full()
                .child(img(ImageSource::Render(frame)).size_full().object_fit(ObjectFit::Contain))
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(stage_tile(
                    self.avatar.clone(),
                    self.initial.clone(),
                    self.name.clone(),
                    "No one is sharing video — audio only",
                ))
                .into_any_element(),
        }
    }
}

/// The live state of a channel once [`session::join`]/[`join_as_host`]
/// connects: the connection handle (to start screen sharing later), the
/// video surface entity the frame-pump task feeds, this side's screen-share,
/// camera, and mute state, and the stop signal for the call's background
/// media tasks (see [`session::CallSession::hang_up`]) — needed on leave, or
/// mic upload and incoming playback just keep running.
struct ConnectedCall {
    pc: corcel_net::CallHandle,
    remote_surface: Entity<VideoSurface>,
    sharing: SharingState,
    camera: CameraState,
    /// Mirror of the value last sent through `mute`, kept here because the
    /// UI needs a synchronous read every render and `watch::Sender::borrow`
    /// would work but hides the "this is UI state" intent.
    muted: bool,
    mute: watch::Sender<bool>,
    hang_up: watch::Sender<bool>,
    /// Sender into the stage's frame channel, handed to
    /// [`session::start_screen_share`]/[`session::start_camera`] so the
    /// sharer's own stream shows up on their own stage too (see
    /// [`session::CallSession::local_video`]).
    local_video: mpsc::Sender<corcel_media::VideoFrame>,
}

/// Stops a connected call's background media tasks (mic upload, incoming
/// audio/video playback) and closes its peer connection — otherwise leaving
/// a channel doesn't actually stop hearing everyone else, since none of
/// that is tied to `ConnectedCall`'s lifetime on its own. Fire-and-forget:
/// the caller (a UI click handler) doesn't wait for this.
fn hang_up(call: &ConnectedCall) {
    let _ = call.hang_up.send(true);
    let pc = call.pc.clone();
    // The returned completion receiver is deliberately dropped — the tokio
    // task keeps running to completion regardless; nobody needs its result.
    drop(runtime::spawn_and_send(async move {
        let _ = pc.close().await;
    }));
}

/// Drops the stage's current frame (and its GPU texture) so stopping your
/// own share/camera doesn't leave the last frame frozen on screen looking
/// like you're still sharing. If someone else is still streaming video,
/// their next frame repopulates the stage within a frame interval.
fn clear_stage(surface: &Entity<VideoSurface>, cx: &mut gpui::App) {
    surface.update(cx, |surface, cx| {
        if let Some(old) = surface.frame.take() {
            cx.drop_image(old, None);
        }
        cx.notify();
    });
}

enum ChannelStatus {
    Connecting,
    Connected(ConnectedCall),
    Failed(String),
}

/// The one live voice call, if any. Lives on [`Shell`] rather than inside
/// [`Screen`] so browsing a text channel — or a different server entirely —
/// doesn't tear the call down; the sidebar's voice panel keeps showing it
/// from anywhere, the way Discord's does.
struct ActiveCall {
    server_id: Uuid,
    channel: ChannelInfo,
    status: ChannelStatus,
}

/// One server's live chat-room connection — see [`Shell::connect_chat`].
/// *Every* saved server keeps a room open for the app's whole lifetime
/// (reconnecting with backoff when it drops), because replication must not
/// depend on which server happens to be on screen: messages for background
/// servers land in the database as they arrive, and this user serves
/// history to peers for all their servers — "every user is a host" only
/// holds if membership, not focus, is what keeps the connection alive.
struct ChatRoom {
    outbound: mpsc::UnboundedSender<ClientMessage>,
}

/// One row of the Ctrl+K quick switcher: a server itself (`channel: None`)
/// or one of its channels.
#[derive(Clone)]
struct SwitcherItem {
    server_id: Uuid,
    server_name: String,
    channel: Option<ChannelInfo>,
}

impl SwitcherItem {
    fn label(&self) -> &str {
        self.channel.as_ref().map(|channel| channel.name.as_str()).unwrap_or(&self.server_name)
    }
}

/// How well `candidate` matches the switcher query — higher is better,
/// `None` filters the row out. Empty queries match everything, so the
/// switcher opens showing the full list.
fn match_score(candidate: &str, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let candidate = candidate.to_lowercase();
    let query = query.to_lowercase();
    if candidate.starts_with(&query) {
        Some(3)
    } else if candidate.contains(&query) {
        Some(2)
    } else if is_subsequence(&query, &candidate) {
        Some(1)
    } else {
        None
    }
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|needed| chars.any(|c| c == needed))
}

/// What the main panel shows for the active server.
enum ServerView {
    /// The server's welcome screen — no channel selected yet.
    Lobby,
    /// Reading/writing a text channel.
    Text { channel: ChannelInfo },
    /// Looking at a voice channel's stage (the call itself lives in
    /// [`Shell::call`], not here — see [`ActiveCall`]).
    Voice { channel: ChannelInfo },
}

enum Screen {
    Home,
    /// A saved server is open. The link/host data lives in [`Shell::servers`]
    /// (looked up by id) — the single source of truth, since a startup rehost
    /// can rewrite a hosted server's link while it's on screen.
    Server { id: Uuid, view: ServerView },
}

/// State for the one-time first-run profile form (see [`Shell::profile`] —
/// while that's `None`, [`Shell::render`] shows this instead of `screen`).
struct ProfileForm {
    name_input: Entity<TextInput>,
    bio_input: Entity<TextInput>,
    avatar_path: Option<PathBuf>,
    banner_path: Option<PathBuf>,
    error: Option<String>,
}

impl ProfileForm {
    fn new(cx: &mut Context<Shell>) -> Self {
        Self {
            name_input: cx.new(|cx| TextInput::new("Your name", cx)),
            bio_input: cx.new(|cx| TextInput::new("Add a short bio…", cx)),
            avatar_path: None,
            banner_path: None,
            error: None,
        }
    }
}

struct Shell {
    profile: Option<Profile>,
    profile_form: ProfileForm,
    /// `None` only if the database failed to open — the app still runs, it
    /// just can't remember anything (`error` says so).
    store: Option<store::Store>,
    /// Every saved server, in rail order. This is the source of truth for
    /// links — [`Screen::Server`] only stores an id into it.
    servers: Vec<SavedServer>,
    screen: Screen,
    call: Option<ActiveCall>,
    /// Connected chat rooms, one per saved server (see [`ChatRoom`]).
    /// Absence means "reconnecting" — a [`Shell::connect_chat`] loop is
    /// always alive for every saved server.
    rooms: HashMap<Uuid, ChatRoom>,
    /// Per-server generation counters. Bumped whenever a server's room
    /// should be replaced (reconnect with fresh address) or die (leaving
    /// the server), so in-flight connect attempts and pump loops can detect
    /// they're stale and stop touching the shell.
    room_generations: HashMap<Uuid, u64>,
    /// The messages of the text channel currently on screen, loaded from the
    /// store and appended to live. Cleared/reloaded on channel switch.
    chat_messages: Vec<ChatMessage>,
    chat_scroll: ScrollHandle,
    message_input: Entity<TextInput>,
    server_name_input: Entity<TextInput>,
    join_link_input: Entity<TextInput>,
    /// Per-channel `(unread, mentions)` counts across *all* servers, kept in
    /// sync with the store's `last_read` table (see
    /// [`store::Store::unread_counts`]). Channels absent from the map have
    /// nothing unread. Channel ids are globally unique, so one flat map
    /// serves both the sidebar rows and the rail's per-server aggregates.
    unread: HashMap<Uuid, (u32, u32)>,
    /// The Ctrl+K quick switcher: an overlay listing every server and
    /// channel, filtered by a fuzzy query. `switcher_selected` indexes into
    /// the *filtered* result list and is clamped at use.
    switcher_open: bool,
    switcher_input: Entity<TextInput>,
    switcher_selected: usize,
    /// Who's typing where: `(channel, author) → last Typing payload seen`.
    /// Entries expire on this machine's clock (a repeating prune task
    /// spawned in [`Shell::new`]) — the sender never says "stopped typing",
    /// silence does.
    typing: HashMap<(Uuid, String), Instant>,
    /// When this user last broadcast a Typing payload — throttles the
    /// composer to one broadcast per few seconds, not one per keystroke.
    last_typing_sent: Option<Instant>,
    /// The message the composer is currently replying to — stamped onto the
    /// next send as its `reply_to`, shown as a bar above the composer, and
    /// cleared by Escape, ✕, or sending.
    replying_to: Option<ChatMessage>,
    /// The message being edited inline, with the dedicated input that
    /// replaces its body. Enter commits, Escape cancels.
    editing: Option<(Uuid, Entity<TextInput>)>,
    /// The message whose inline emoji palette is open, if any.
    reacting_to: Option<Uuid>,
    /// Live reactions of the on-screen text channel: message → chips in
    /// first-reacted order, each an emoji with everyone who reacted with it.
    /// Rebuilt from the store whenever the channel (re)loads or a reaction
    /// lands (see [`Shell::reload_reactions`]).
    chat_reactions: HashMap<Uuid, Vec<(String, Vec<String>)>>,
    add_server_open: bool,
    /// Briefly `true` after "Copy Invite Link" so the button itself can
    /// confirm the copy happened — clipboard writes are otherwise invisible.
    link_copied: bool,
    error: Option<String>,
}

impl Shell {
    fn new(cx: &mut Context<Self>) -> Self {
        let (store, servers, error) = match store::Store::open() {
            Ok(store) => match store.servers() {
                Ok(servers) => (Some(store), servers, None),
                Err(err) => (Some(store), Vec::new(), Some(format!("couldn't load saved servers: {err:#}"))),
            },
            Err(err) => (None, Vec::new(), Some(format!("couldn't open the local database: {err:#}"))),
        };

        let mut shell = Self {
            profile: Profile::load(),
            profile_form: ProfileForm::new(cx),
            store,
            servers,
            screen: Screen::Home,
            call: None,
            rooms: HashMap::new(),
            room_generations: HashMap::new(),
            chat_messages: Vec::new(),
            chat_scroll: ScrollHandle::new(),
            message_input: cx.new(|cx| TextInput::new("Send a message…", cx)),
            server_name_input: cx.new(|cx| TextInput::new("My Server", cx)),
            join_link_input: cx.new(|cx| TextInput::new("Paste an invite link...", cx)),
            unread: HashMap::new(),
            switcher_open: false,
            switcher_input: cx.new(|cx| TextInput::new("Where would you like to go?", cx)),
            switcher_selected: 0,
            typing: HashMap::new(),
            last_typing_sent: None,
            replying_to: None,
            editing: None,
            reacting_to: None,
            chat_reactions: HashMap::new(),
            add_server_open: false,
            link_copied: false,
            error,
        };

        shell.start_rehosts(cx);
        // Every saved server gets its chat room immediately — background
        // servers replicate too (see [`ChatRoom`]).
        for id in shell.servers.iter().map(|server| server.link.id).collect::<Vec<_>>() {
            shell.connect_chat(id, cx);
            shell.refresh_unread(id);
        }
        // Land the user back in the server they'd expect instead of an
        // empty Home — the first one in the rail.
        if let Some(id) = shell.servers.first().map(|server| server.link.id) {
            shell.open_server(id, cx);
        }

        // The typing-indicator janitor: whoever stops typing just goes
        // silent, so their entry has to age out here. Lives for the whole
        // app; the notify only fires when something actually expired.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let alive = this.update(cx, |shell, cx| {
                    let now = Instant::now();
                    let before = shell.typing.len();
                    shell.typing.retain(|_, seen| now.duration_since(*seen) < Duration::from_secs(6));
                    if shell.typing.len() != before {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        })
        .detach();

        shell
    }

    /// Brings every hosted server's relay back up on launch (see
    /// [`session::rehost`]) — this is what makes a hosted server persist
    /// across restarts with the same identity/fingerprint, so old invite
    /// links keep working. The returned link (the address/port can change)
    /// replaces the saved one.
    fn start_rehosts(&self, cx: &mut Context<Self>) {
        for server in &self.servers {
            if !server.is_host {
                continue;
            }
            let Some(identity) = server.identity.clone() else { continue };
            let name = server.link.name.clone();
            let rx = runtime::spawn_and_send(session::rehost(server.link.clone(), identity));
            cx.spawn(async move |this, cx| {
                let result = rx.await;
                let _ = this.update(cx, |shell, cx| {
                    let new_link = match result {
                        Ok(Ok(link)) => link,
                        Ok(Err(err)) => {
                            shell.error = Some(format!("couldn't restart hosting \"{name}\": {err:#}"));
                            cx.notify();
                            return;
                        }
                        Err(_) => return,
                    };
                    if let Some(server) = shell.servers.iter_mut().find(|s| s.link.id == new_link.id) {
                        server.link = new_link.clone();
                    }
                    if let Some(store) = &shell.store {
                        let _ = store.update_link(&new_link);
                    }
                    // Restart the room loop so it picks up the (possibly
                    // new) address immediately instead of waiting out
                    // whatever backoff its current attempt is sleeping in.
                    shell.connect_chat(new_link.id, cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    // ---- Profile setup -------------------------------------------------

    fn pick_avatar_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.pick_profile_image(true, cx);
    }

    fn pick_banner_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.pick_profile_image(false, cx);
    }

    fn pick_profile_image(&mut self, is_avatar: bool, cx: &mut Context<Self>) {
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

    fn profile_continue_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let name = self.profile_form.name_input.read(cx).content.trim().to_string();
        if name.is_empty() {
            self.profile_form.error = Some("A name is required.".to_string());
            cx.notify();
            return;
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
            return;
        }
        self.profile_form.error = None;
        self.profile = Some(profile);
        cx.notify();
    }

    // ---- Adding / removing servers ------------------------------------

    fn open_add_server_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.add_server_open = true;
        self.error = None;
        cx.notify();
    }

    fn close_add_server_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.add_server_open = false;
        cx.notify();
    }

    fn host_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let name = self.server_name_input.read(cx).content.to_string();
        let name = if name.trim().is_empty() { "My Server".to_string() } else { name };
        self.error = None;

        let rx = runtime::spawn_and_send(session::host(name));
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |shell, cx| {
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

    fn join_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.join_link_input.read(cx).content.to_string();
        match ServerLink::decode(text.trim()) {
            Ok(link) => {
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
                self.error = None;
                self.join_link_input.update(cx, |input, cx| input.clear(cx));
                self.connect_chat(id, cx);
                self.open_server(id, cx);
            }
            Err(err) => self.error = Some(format!("invalid invite link: {err:#}")),
        }
        cx.notify();
    }

    fn copy_link_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
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
    fn leave_server_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
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
        if let Some(store) = &self.store {
            let _ = store.remove_server(id);
        }
        self.servers.retain(|server| server.link.id != id);
        self.screen = Screen::Home;
        cx.notify();
    }

    // ---- Navigation ----------------------------------------------------

    fn active_server(&self) -> Option<&SavedServer> {
        let Screen::Server { id, .. } = &self.screen else { return None };
        self.servers.iter().find(|server| server.link.id == *id)
    }

    /// Switches the shell to a server (rail click, or right after
    /// hosting/joining one). Pure view change — every saved server's chat
    /// room is already alive (see [`ChatRoom`]), and the current voice call
    /// (possibly in another server) keeps running.
    fn open_server(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if matches!(&self.screen, Screen::Server { id: current, .. } if *current == id) {
            return;
        }
        self.screen = Screen::Server { id, view: ServerView::Lobby };
        self.chat_messages.clear();
        self.reset_composer_state();
        self.error = None;
        cx.notify();
    }

    fn open_text_channel(&mut self, channel: ChannelInfo, cx: &mut Context<Self>) {
        let Screen::Server { id, .. } = &self.screen else { return };
        let id = *id;
        self.chat_messages =
            self.store.as_ref().and_then(|store| store.messages(channel.id).ok()).unwrap_or_default();
        self.reset_composer_state();
        self.reload_reactions(channel.id);
        self.mark_channel_read(channel.id);
        self.screen = Screen::Server { id, view: ServerView::Text { channel } };
        self.chat_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Drops every bit of in-flight composer/message-action state — a reply
    /// half-set-up, an edit in progress, an open emoji palette. Called on
    /// any navigation, since all of it is meaningless in another channel.
    fn reset_composer_state(&mut self) {
        self.replying_to = None;
        self.editing = None;
        self.reacting_to = None;
        self.chat_reactions.clear();
    }

    /// Rebuilds [`Shell::chat_reactions`] for a channel from the store's
    /// live (non-removed) reaction rows.
    fn reload_reactions(&mut self, channel: Uuid) {
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

    // ---- Unread tracking -----------------------------------------------

    fn my_name(&self) -> String {
        self.profile.as_ref().map(|p| p.name.clone()).unwrap_or_default()
    }

    /// Marks a channel read as of now: everything currently stored for it
    /// stops counting as unread, durably and in the in-memory badge map.
    fn mark_channel_read(&mut self, channel: Uuid) {
        if let Some(store) = &self.store {
            let _ = store.set_last_read(channel, chat::now_millis());
        }
        self.unread.remove(&channel);
    }

    /// Reloads one server's unread/mention counts from the store into
    /// [`Shell::unread`]. Called on startup and whenever new messages land
    /// for the server (including for *background* servers — that's what the
    /// badges are for).
    fn refresh_unread(&mut self, server_id: Uuid) {
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

    // ---- Quick switcher ------------------------------------------------

    /// The filtered, ranked switcher rows for a query: all servers and all
    /// of their channels, best matches first (ties keep rail/sidebar order,
    /// the sort is stable).
    fn switcher_items(&self, query: &str) -> Vec<SwitcherItem> {
        let mut scored: Vec<(u8, SwitcherItem)> = Vec::new();
        for server in &self.servers {
            let server_name = server.link.name.clone();
            if let Some(score) = match_score(&server_name, query) {
                scored.push((
                    score,
                    SwitcherItem { server_id: server.link.id, server_name: server_name.clone(), channel: None },
                ));
            }
            for channel in &server.link.channels {
                if let Some(score) = match_score(&channel.name, query) {
                    scored.push((
                        score,
                        SwitcherItem {
                            server_id: server.link.id,
                            server_name: server_name.clone(),
                            channel: Some(channel.clone()),
                        },
                    ));
                }
            }
        }
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, item)| item).collect()
    }

    fn open_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.switcher_open = true;
        self.switcher_selected = 0;
        self.switcher_input.update(cx, |input, cx| input.clear(cx));
        window.focus(&self.switcher_input.focus_handle(cx));
        cx.notify();
    }

    fn close_switcher(&mut self, cx: &mut Context<Self>) {
        self.switcher_open = false;
        cx.notify();
    }

    /// Enter (or a row click): jump to the selected result — a server's
    /// lobby, a text channel, or straight into a voice channel.
    fn switcher_activate(&mut self, cx: &mut Context<Self>) {
        let query = self.switcher_input.read(cx).content.trim().to_string();
        let items = self.switcher_items(&query);
        if items.is_empty() {
            return;
        }
        let item = items[self.switcher_selected.min(items.len() - 1)].clone();
        self.switcher_open = false;
        self.open_server(item.server_id, cx);
        match item.channel {
            Some(channel) if channel.kind == ChannelKind::Text => self.open_text_channel(channel, cx),
            Some(channel) => self.enter_voice_channel(channel, cx),
            None => {}
        }
        cx.notify();
    }

    /// The root key handler — the outermost element sees every key event
    /// bubble up from whatever's focused, which is what makes app-wide keys
    /// (F11, Ctrl+K, and the switcher's navigation keys while it's open)
    /// work without a global keymap.
    fn root_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if key == "f11" {
            window.toggle_fullscreen();
            return;
        }
        if key == "k" && event.keystroke.modifiers.control {
            if self.switcher_open {
                self.close_switcher(cx);
            } else {
                self.open_switcher(window, cx);
            }
            return;
        }
        if !self.switcher_open {
            // Escape unwinds in-flight message state, most-specific first:
            // an open edit, then the emoji palette, then the reply setup.
            if key == "escape" {
                if self.editing.is_some() {
                    self.cancel_edit(cx);
                } else if self.reacting_to.take().is_some() || self.replying_to.take().is_some() {
                    cx.notify();
                }
            }
            return;
        }
        match key {
            "escape" => self.close_switcher(cx),
            "enter" => self.switcher_activate(cx),
            "up" => {
                self.switcher_selected = self.switcher_selected.saturating_sub(1);
                cx.notify();
            }
            "down" => {
                let query = self.switcher_input.read(cx).content.trim().to_string();
                let count = self.switcher_items(&query).len();
                self.switcher_selected = (self.switcher_selected + 1).min(count.saturating_sub(1));
                cx.notify();
            }
            _ => {}
        }
    }

    // ---- Chat ----------------------------------------------------------

    fn room_generation(&self, server_id: Uuid) -> u64 {
        self.room_generations.get(&server_id).copied().unwrap_or(0)
    }

    /// Starts (or restarts) a server's chat-room loop: the long-lived relay
    /// connection that carries message broadcasts and history exchanges
    /// (see [`chat`] for the replication scheme). The loop never gives up —
    /// it reconnects with backoff for as long as the server stays saved,
    /// because hosts restart and laptops sleep. Until it connects, chat
    /// quietly stays read-only-from-local-history — local first, the
    /// network is optional.
    fn connect_chat(&mut self, server_id: Uuid, cx: &mut Context<Self>) {
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

                // Re-read the address every attempt — a rehost may have
                // moved the relay to a new port while we were backing off.
                // The host reaches its own relay over loopback: its link
                // carries the LAN address, which some networks won't
                // hairpin back.
                let target = this.update(cx, |shell, _| {
                    if shell.room_generation(server_id) != generation {
                        return None;
                    }
                    shell.servers.iter().find(|s| s.link.id == server_id).map(|server| {
                        let addr = if server.is_host {
                            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), server.link.addr.port())
                        } else {
                            server.link.addr
                        };
                        (addr, server.link.fingerprint)
                    })
                });
                let Ok(Some((addr, fingerprint))) = target else { return };

                let Ok(Ok(mut conn)) =
                    runtime::spawn_and_send(session::open_room(addr, fingerprint, server_id)).await
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

    fn handle_room_message(&mut self, server_id: Uuid, message: ServerMessage, cx: &mut Context<Self>) {
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

    fn send_room(&self, server_id: Uuid, payload: ChatPayload, to: Option<PeerId>) {
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
    fn absorb_messages(&mut self, server_id: Uuid, messages: Vec<ChatMessage>, cx: &mut Context<Self>) {
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
    fn absorb_edit(
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
    fn absorb_delete(&mut self, server_id: Uuid, message_id: Uuid, channel: Uuid, cx: &mut Context<Self>) {
        let Some(store) = &self.store else { return };
        if !store.apply_delete(message_id).unwrap_or(false) {
            return;
        }
        self.refresh_visible_channel(server_id, channel, cx);
    }

    /// Merges received reaction states (one from a live toggle, many from a
    /// history batch) and refreshes whichever visible channel they touched.
    fn absorb_reactions(&mut self, server_id: Uuid, reactions: Vec<ReactionRow>, cx: &mut Context<Self>) {
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
    fn refresh_visible_channel(&mut self, server_id: Uuid, channel: Uuid, cx: &mut Context<Self>) {
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
    fn note_typing(&mut self, channel: Uuid, author: String, cx: &mut Context<Self>) {
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

    /// Called on every keystroke that lands in the composer: broadcasts a
    /// Typing payload if there's real content and the last one is stale.
    fn maybe_send_typing(&mut self, cx: &mut Context<Self>) {
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

    fn send_chat_message(&mut self, cx: &mut Context<Self>) {
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

    // ---- Message actions (reply / edit / delete / react) ---------------

    /// The server and channel of the text view on screen — every message
    /// action below is only reachable from there.
    fn visible_text_channel(&self) -> Option<(Uuid, Uuid)> {
        match &self.screen {
            Screen::Server { id, view: ServerView::Text { channel } } => Some((*id, channel.id)),
            _ => None,
        }
    }

    fn start_reply(&mut self, message: ChatMessage, window: &mut Window, cx: &mut Context<Self>) {
        self.replying_to = Some(message);
        self.editing = None;
        self.reacting_to = None;
        window.focus(&self.message_input.focus_handle(cx));
        cx.notify();
    }

    fn start_edit(&mut self, message: &ChatMessage, window: &mut Window, cx: &mut Context<Self>) {
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
    fn commit_edit(&mut self, cx: &mut Context<Self>) {
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

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            cx.notify();
        }
    }

    /// Deletes one of this user's own messages: tombstone locally, then
    /// broadcast. No confirmation — the stub it leaves behind makes the
    /// action obvious and the blast radius is one message.
    fn delete_message(&mut self, message_id: Uuid, cx: &mut Context<Self>) {
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
    fn toggle_reaction(&mut self, message_id: Uuid, emoji: &str, cx: &mut Context<Self>) {
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

    // ---- Voice ---------------------------------------------------------

    fn enter_voice_channel(&mut self, channel: ChannelInfo, cx: &mut Context<Self>) {
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
            // Switching calls: tear the old one down first.
            if let ChannelStatus::Connected(connected) = &call.status {
                hang_up(connected);
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

            let CallSession { pc, mut remote_video, hang_up: hang_up_tx, mute, local_video } = session;
            let surface = this.update(cx, |shell, cx| {
                let profile = shell.profile.as_ref();
                let surface = cx.new(|_| VideoSurface {
                    frame: None,
                    name: profile.map(|p| p.name.clone()).unwrap_or_default().into(),
                    avatar: profile.and_then(|p| p.avatar_path.clone()),
                    initial: profile.map(|p| p.initial()).unwrap_or_else(|| "?".to_string()).into(),
                });
                let call = ConnectedCall {
                    pc,
                    remote_surface: surface.clone(),
                    sharing: SharingState::Idle,
                    camera: CameraState::Idle,
                    muted: false,
                    mute,
                    hang_up: hang_up_tx,
                    local_video,
                };
                if let Some(active) = shell.call.as_mut().filter(|active| active.channel.id == channel.id) {
                    active.status = ChannelStatus::Connected(call);
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

    fn leave_channel_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(call) = self.call.take() else { return };
        if let ChannelStatus::Connected(connected) = &call.status {
            hang_up(connected);
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
    fn retry_channel_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
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

    fn connected_call_mut(&mut self) -> Option<&mut ConnectedCall> {
        match self.call.as_mut() {
            Some(ActiveCall { status: ChannelStatus::Connected(call), .. }) => Some(call),
            _ => None,
        }
    }

    fn toggle_mute_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(call) = self.connected_call_mut() {
            call.muted = !call.muted;
            let _ = call.mute.send(call.muted);
            cx.notify();
        }
    }

    fn dismiss_error_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        cx.notify();
    }

    fn share_screen_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
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

    fn stop_sharing_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(call) = self.connected_call_mut() {
            if let SharingState::Active(handle) = std::mem::replace(&mut call.sharing, SharingState::Idle) {
                handle.stop();
                let surface = call.remote_surface.clone();
                clear_stage(&surface, cx);
                cx.notify();
            }
        }
    }

    fn toggle_camera_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
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

    fn stop_camera_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(call) = self.connected_call_mut() {
            if let CameraState::Active(handle) = std::mem::replace(&mut call.camera, CameraState::Idle) {
                handle.stop();
                let surface = call.remote_surface.clone();
                clear_stage(&surface, cx);
                cx.notify();
            }
        }
    }

    // ---- Rendering -----------------------------------------------------

    /// The first-run "set up your profile" screen — shown instead of
    /// `screen` while [`Shell::profile`] is `None`. Mirrors the mockup's
    /// "First run — set up your profile" frame: a dashed banner picker with
    /// an avatar picker overlapping its bottom edge, then Name (required)
    /// and Bio (optional).
    fn render_profile_setup(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.profile_form.name_input.read(cx).content.clone();
        let initial = name
            .trim()
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string());

        let banner = div()
            .h(px(84.))
            .rounded_lg() // --radius-lg (10px, nearest preset)
            .overflow_hidden()
            .bg(theme::wash())
            .border_1()
            .border_dashed()
            .border_color(theme::input_border())
            .cursor_pointer()
            .hover(|style| style.border_color(theme::ring()))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::pick_banner_clicked))
            .child(match &self.profile_form.banner_path {
                Some(path) => {
                    img(ImageSource::from(path.clone())).size_full().object_fit(ObjectFit::Cover).into_any_element()
                }
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(theme::faint_foreground())
                    .child("Add a banner (optional)")
                    .into_any_element(),
            });

        let avatar_picker = div()
            .id("avatar-picker")
            .ml(px(20.))
            .mt(px(-32.))
            .cursor_pointer()
            .on_mouse_up(MouseButton::Left, cx.listener(Self::pick_avatar_clicked))
            .child(theme::avatar(self.profile_form.avatar_path.clone(), initial, px(64.)));

        let error = self.profile_form.error.clone().map(|message| {
            div().text_size(px(12.5)).text_color(theme::destructive_foreground()).child(message)
        });

        div()
            .flex()
            .flex_col()
            .gap_4()
            .w(px(320.))
            .child(div().text_size(px(19.)).font_weight(FontWeight::BOLD).child("corcel"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_size(px(15.)).font_weight(FontWeight::BOLD).child("Set up your profile"))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .text_color(theme::muted_foreground())
                            .child("This is how people in your servers will see you. Only a name is required."),
                    ),
            )
            .child(div().flex().flex_col().child(banner).child(avatar_picker))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .mt(px(-16.))
                    .child(div().text_sm().font_weight(FontWeight::BOLD).text_color(theme::muted_foreground()).child("Name"))
                    .child(self.profile_form.name_input.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::muted_foreground())
                            .child("Bio (optional)"),
                    )
                    .child(self.profile_form.bio_input.clone()),
            )
            .children(error)
            .child(
                theme::button(ButtonVariant::Primary, "Continue")
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::profile_continue_clicked)),
            )
    }

    /// The empty Home screen — no servers yet, just a centered "+" that
    /// opens [`Self::render_add_server_modal`].
    fn render_home_empty_state(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .child(
                div()
                    .id("add-server")
                    .group("home-add")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(56.))
                    .rounded_full()
                    .border_1()
                    .border_dashed()
                    .border_color(theme::input_border())
                    .cursor_pointer()
                    .hover(|style| style.border_color(theme::success()).bg(theme::success()))
                    .active(|style| style.opacity(0.85))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::open_add_server_clicked))
                    .child(
                        theme::icon(icons::PLUS, px(22.))
                            .text_color(theme::success())
                            .group_hover("home-add", |style| style.text_color(theme::primary_foreground())),
                    ),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme::muted_foreground())
                    .child("No servers yet — add one to get started"),
            )
    }

    /// The "Add a Server" modal: a dimmed backdrop behind a popover card
    /// holding Host/Join fields. Dismissed by the "×", or by releasing the
    /// mouse outside the card (same `on_mouse_up_out` trick [`TextInput`]
    /// uses to end a drag started inside it).
    fn render_add_server_modal(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let backdrop = div().size_full().absolute().top_0().left_0().bg(theme::scrim());

        let error = self.error.clone().map(|message| {
            div().text_size(px(12.)).text_color(theme::destructive_foreground()).child(message)
        });

        let card = div()
            .id("add-server-card")
            .w(px(300.))
            .flex()
            .flex_col()
            .gap_4()
            .p(px(20.))
            .bg(theme::popover())
            .border_1()
            .border_color(theme::border())
            .rounded_xl() // --radius-xl (14px, nearest preset)
            .shadow_lg()
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::close_add_server_clicked))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(16.)).font_weight(FontWeight::BOLD).child("Add a Server"))
                    .child(
                        theme::icon_button("close-add-server", icons::X, "Close")
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::close_add_server_clicked)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::muted_foreground())
                            .child("Host a new server"),
                    )
                    .child(self.server_name_input.clone())
                    .child(
                        theme::button(ButtonVariant::Primary, "Host Server")
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::host_clicked)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(10.))
                    .text_color(theme::faint_foreground())
                    .child(div().flex_1().h(px(1.)).bg(theme::border()))
                    .child("or")
                    .child(div().flex_1().h(px(1.)).bg(theme::border())),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::muted_foreground())
                            .child("Join with an invite"),
                    )
                    .child(self.join_link_input.clone())
                    .child(
                        theme::button(ButtonVariant::Secondary, "Join Server")
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::join_clicked)),
                    ),
            )
            .children(error);

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

    /// The Ctrl+K quick switcher overlay: a centered card with a query
    /// input and the ranked result list. Rendered above every other overlay
    /// (priority 2 — the add-server modal is 1).
    fn render_quick_switcher(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.switcher_input.read(cx).content.trim().to_string();
        let items = self.switcher_items(&query);
        let selected = self.switcher_selected.min(items.len().saturating_sub(1));

        let rows = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let is_selected = index == selected;
                let icon = match &item.channel {
                    Some(channel) if channel.kind == ChannelKind::Voice => {
                        theme::icon(icons::VOLUME, px(16.)).text_color(theme::muted_foreground()).into_any_element()
                    }
                    Some(_) => {
                        theme::icon(icons::HASH, px(16.)).text_color(theme::muted_foreground()).into_any_element()
                    }
                    None => div()
                        .size(px(18.))
                        .rounded_full()
                        .bg(theme::wash_strong())
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(10.))
                        .font_weight(FontWeight::BOLD)
                        .child(
                            item.server_name
                                .trim()
                                .chars()
                                .next()
                                .map(|c| c.to_uppercase().to_string())
                                .unwrap_or_else(|| "?".to_string()),
                        )
                        .into_any_element(),
                };
                // Channels carry their server's name on the right so "which
                // #general is this" never needs a second look.
                let context = item.channel.is_some().then(|| {
                    div().text_size(px(11.5)).text_color(theme::faint_foreground()).child(item.server_name.clone())
                });

                div()
                    .id(("switcher-row", index))
                    .h(px(36.))
                    .px(px(10.))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .when(is_selected, |style| style.bg(theme::wash()))
                    .hover(|style| style.bg(theme::wash()))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |shell, _, _window, cx| {
                            shell.switcher_selected = index;
                            shell.switcher_activate(cx);
                        }),
                    )
                    .child(div().flex_none().w(px(18.)).flex().justify_center().child(icon))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(14.))
                            .child(item.label().to_string()),
                    )
                    .children(context)
            })
            .collect::<Vec<_>>();

        let empty = items.is_empty().then(|| {
            div()
                .py(px(16.))
                .flex()
                .justify_center()
                .text_size(px(13.))
                .text_color(theme::muted_foreground())
                .child("No matches")
        });

        let card = div()
            .w(px(480.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(14.))
            .bg(theme::popover())
            .border_1()
            .border_color(theme::border())
            .rounded_xl()
            .shadow_lg()
            .child(self.switcher_input.clone())
            .child(
                div()
                    .id("switcher-results")
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(rows)
                    .children(empty),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme::faint_foreground())
                    .child("↑↓ to navigate · Enter to jump · Esc to close"),
            );

        div()
            .size_full()
            .absolute()
            .top_0()
            .left_0()
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .size_full()
                    .absolute()
                    .top_0()
                    .left_0()
                    .bg(theme::scrim())
                    .on_mouse_up(MouseButton::Left, cx.listener(|shell, _, _window, cx| shell.close_switcher(cx))),
            )
            .child(div().mt(px(120.)).child(card))
    }

    /// The server-icon rail: one bubble per saved server (initial + tooltip,
    /// with Discord's pill indicator — full height on the active one, a
    /// short hover hint on the rest) and the "+" action below a divider.
    fn render_server_rail(&mut self, cx: &mut Context<Self>) -> Div {
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
                            .size(px(48.))
                            .rounded(if is_active { px(16.) } else { px(24.) })
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
                    .size(px(48.))
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
    fn render_channel_sidebar(&mut self, profile: &Profile, server: &SavedServer, cx: &mut Context<Self>) -> Div {
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
        let (self_muted, self_sharing) = match &self.call {
            Some(ActiveCall { status: ChannelStatus::Connected(call), .. }) => {
                (call.muted, matches!(call.sharing, SharingState::Active(_)))
            }
            _ => (false, false),
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
            })
            .collect::<Vec<_>>();

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
                    div()
                        .mx(px(8.))
                        .pl(px(26.))
                        .pr(px(8.))
                        .py(px(3.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(theme::avatar(profile.avatar_path.clone(), profile.initial(), px(20.)))
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
                        .children(
                            self_muted
                                .then(|| theme::icon(icons::MIC_OFF, px(14.)).text_color(theme::destructive_foreground())),
                        )
                        .children(
                            self_sharing.then(|| theme::icon(icons::MONITOR_UP, px(14.)).text_color(theme::info())),
                        )
                });

                div().flex().flex_col().child(row).children(participant)
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
                    .relative()
                    .flex_none()
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
                            if self_muted {
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

    /// A text channel's panel: `#name` header, scrollback, and the composer
    /// (Enter sends — the wrapper catches the key event bubbling up from
    /// the focused [`TextInput`], same trick as the shell's F11 handler).
    fn render_chat_panel(&mut self, profile: &Profile, channel: &ChannelInfo, cx: &mut Context<Self>) -> Div {
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
                    richtext::render_body(&message.body)
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

    /// The main panel for the active server: lobby welcome, a text channel's
    /// chat, or a voice channel's stage (header toolbar, video/tile stage,
    /// floating control bar). The error toast overlays whichever is shown.
    fn render_main_panel(&mut self, profile: &Profile, server: &SavedServer, cx: &mut Context<Self>) -> Div {
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
                            .child(theme::icon(icons::VOLUME, px(28.)).text_color(theme::muted_foreground())),
                    )
                    .child(
                        div()
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

        div().flex_1().min_w_0().h_full().relative().flex().flex_col().child(content).children(error_toast)
    }

    /// A voice channel's stage — driven by [`Shell::call`] when it matches
    /// this channel; a plain join prompt otherwise (e.g. after the call was
    /// disconnected from elsewhere while this view stayed up).
    fn render_voice_panel(&mut self, profile: &Profile, channel: &ChannelInfo, cx: &mut Context<Self>) -> Div {
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
                                .child(camera_btn)
                                .child(share_btn)
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

    /// The composed layout once any server exists: server rail, then — for
    /// the active server — channel sidebar and main panel, or a "pick a
    /// server" hint when none is open.
    fn render_app_shell(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let profile = self.profile.clone().expect("render_app_shell is only reached once a profile exists");
        let rail = self.render_server_rail(cx);

        let root = div()
            .relative()
            .size_full()
            .flex()
            .overflow_hidden()
            .bg(theme::background())
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

/// A participant tile for the stage's audio-only states: avatar, name, and
/// a one-line caption saying what's (not) happening.
fn stage_tile(
    avatar: Option<PathBuf>,
    initial: impl Into<SharedString>,
    name: impl Into<SharedString>,
    caption: impl Into<SharedString>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(12.))
        .child(theme::avatar(avatar, initial.into(), px(80.)))
        .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).text_color(theme::foreground()).child(name.into()))
        .child(div().text_size(px(12.5)).text_color(theme::muted_foreground()).child(caption.into()))
}

/// A message's send time in the reader's local timezone — "Today at HH:MM"
/// for today, a full date otherwise. The author's clock stamped it (see
/// [`chat::ChatMessage::sent_at`]); at friends scale that's plenty.
/// One small icon button in a message's hover action bar — callers attach
/// `.on_mouse_up(...)`. `destructive` tints the icon red (the delete
/// action).
fn message_action(icon_path: &'static str, destructive: bool) -> Div {
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

fn format_timestamp(millis: i64) -> String {
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

/// A round 44px call-control button for the in-call bar. `pending` renders
/// it dimmed and inert (the state between clicking and the async start
/// resolving) so a click visibly *did something* even before the pipeline
/// is up. Callers attach `.on_mouse_up(...)`.
fn call_button(
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

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.profile.is_none() {
            div()
                .flex()
                .flex_col()
                .justify_center()
                .items_center()
                .size_full()
                .bg(theme::background())
                .text_color(theme::foreground())
                .child(self.render_profile_setup(cx))
                .into_any_element()
        } else if self.servers.is_empty() && matches!(self.screen, Screen::Home) {
            // The error line hides while the modal is up — the modal card
            // shows it inline instead.
            let error = (!self.add_server_open)
                .then(|| self.error.clone())
                .flatten()
                .map(|message| div().text_size(px(12.5)).text_color(theme::destructive_foreground()).child(message));

            div()
                .flex()
                .flex_col()
                .gap_4()
                .justify_center()
                .items_center()
                .size_full()
                .p(px(24.))
                .bg(theme::background())
                .text_color(theme::foreground())
                .child(div().text_size(px(19.)).font_weight(FontWeight::BOLD).child("corcel"))
                .child(self.render_home_empty_state(cx))
                .children(error)
                .into_any_element()
        } else {
            self.render_app_shell(cx)
        };

        let modal = (self.profile.is_some() && self.add_server_open)
            .then(|| deferred(self.render_add_server_modal(cx)).with_priority(1));
        let switcher = (self.profile.is_some() && self.switcher_open)
            .then(|| deferred(self.render_quick_switcher(cx)).with_priority(2));

        // App-wide keys (F11, Ctrl+K, the open switcher's navigation) are
        // registered on the outermost element so it's always an ancestor of
        // whatever's focused (a text input, a button, ...) and sees the
        // bubbled event.
        div()
            .size_full()
            .relative()
            .on_key_down(cx.listener(Self::root_key_down))
            .child(content)
            .children(modal)
            .children(switcher)
    }
}

fn main() {
    profile::migrate_legacy_config_dir();
    corcel_media::init().expect("failed to initialize GStreamer");

    Application::new().with_assets(assets::Assets).run(|cx: &mut App| {
        text_input::init(cx);

        // Open at 90% of the primary display, centered — the window stays
        // freely resizable (GPUI's default) down to the min size below.
        let display_size = cx
            .primary_display()
            .map(|display| display.bounds().size)
            .unwrap_or_else(|| size(px(1920.0), px(1080.0)));
        let bounds = Bounds::centered(None, size(display_size.width * 0.9, display_size.height * 0.9), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions { title: Some("corcel".into()), ..Default::default() }),
                    // The app-shell's server-rail + channel-sidebar alone take
                    // ~312px of fixed-width chrome, so keep the window from
                    // shrinking small enough to crush the video panel. The
                    // window is resizable (GPUI's default) up to whatever size
                    // the user wants above this floor.
                    window_min_size: Some(size(px(720.0), px(480.0))),
                    ..Default::default()
                },
                |_window, cx| cx.new(Shell::new),
            )
            .unwrap();

        window
            .update(cx, |shell, window, cx| {
                if shell.profile.is_none() {
                    window.focus(&shell.profile_form.name_input.focus_handle(cx));
                }
                cx.activate(true);
            })
            .unwrap();
    });
}
