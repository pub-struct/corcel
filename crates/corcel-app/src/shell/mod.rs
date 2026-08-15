//! The application shell: one GPUI entity ([`Shell`]) owning all UI state,
//! split across feature modules that each contribute an `impl Shell` block —
//! [`onboarding`] (profile setup + the add-server flow), [`workspace`] (rail,
//! sidebar, app chrome), [`messaging`] (chat replication + the chat panel),
//! [`call`] (voice/video/screen-share), and [`switcher`] (the Ctrl+K quick
//! switcher). This file keeps what they all share: the state types, the
//! constructor, navigation, and the root render + app-wide key handling.

mod call;
mod embeds;
mod messaging;
mod onboarding;
mod switcher;
mod titlebar;
mod workspace;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt, AnyElement, App, Background, ClipboardItem, Context, Div, Entity, Focusable, FontWeight,
    ImageSource, KeyDownEvent, KeyUpEvent, MouseButton, MouseUpEvent, ObjectFit, PathPromptOptions, Render,
    RenderImage, Rgba, ScrollHandle, SharedString, Stateful, Transformation, Window, deferred, div, ease_out_quint,
    img, linear_color_stop, linear_gradient, prelude::*, px, radians, relative, rgba,
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use corcel_signal::{ClientMessage, PeerId, Reach, RelayIdentity, ServerMessage};

use crate::assets::icons;
use crate::chat::{self, ChatMessage, ChatPayload, ReactionRow};
use crate::invite::{ChannelInfo, ChannelKind, ServerLink};
use crate::profile::{self, Profile};
use crate::session::{self, CallSession, CameraHandle, ScreenShareHandle};
use crate::store::{self, SavedServer};
use crate::text_input::TextInput;
use crate::theme::{self, ButtonVariant};
use crate::{richtext, runtime};

use call::{speaking_avatar, stage_tile};
use embeds::{ImageEmbed, VideoEmbed};

/// How long a remote peer's speaking ring survives without a fresh
/// `Speaking { true }`. Transitions are explicit (unlike typing, silence
/// isn't the signal here — `false` is), so this is only the safety net for
/// a peer that disconnects mid-word and never sends its `false`.
const SPEAKING_EXPIRY: Duration = Duration::from_secs(3);

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
/// Holds one tile per live video feed (each remote track and each local
/// self-preview has its own feed id — see [`session::VideoEvent`]), laid
/// out as a grid: one feed fills the stage, two split it, three or four
/// quarter it. Insertion order is arrival order, so tiles don't reshuffle
/// mid-call.
struct VideoSurface {
    frames: Vec<(u64, Arc<RenderImage>)>,
    /// Profile bits for the audio-only placeholder tile, copied in at
    /// creation so this view renders without reaching back into the shell.
    name: SharedString,
    avatar: Option<PathBuf>,
    initial: SharedString,
    /// Whether this side's mic is currently audible — drives the green ring
    /// on the placeholder tile's avatar. Fed by the call's speaking watcher
    /// (see [`Shell::enter_voice_channel`]); living here means the ~word-rate
    /// flicker of speech re-renders only this view, like `frame` does.
    speaking: bool,
}

impl VideoSurface {
    /// Replaces (or adds) a feed's frame, returning the texture that must
    /// be dropped by the caller via `cx.drop_image` — `RenderImage`s are
    /// never evicted from the sprite atlas on their own.
    fn set_frame(&mut self, feed: u64, image: Arc<RenderImage>) -> Option<Arc<RenderImage>> {
        match self.frames.iter_mut().find(|(id, _)| *id == feed) {
            Some((_, slot)) => Some(std::mem::replace(slot, image)),
            None => {
                self.frames.push((feed, image));
                None
            }
        }
    }

    fn remove_feed(&mut self, feed: u64) -> Option<Arc<RenderImage>> {
        let index = self.frames.iter().position(|(id, _)| *id == feed)?;
        Some(self.frames.remove(index).1)
    }
}

impl Render for VideoSurface {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match self.frames.len() {
            count if count > 0 => {
                // 1 feed fills the stage; 2 sit side by side; 3-4 quarter
                // it. (More than 4 video feeds at friends scale means
                // someone is showing off; they wrap into the same grid.)
                let (tile_w, tile_h) = match count {
                    1 => (1.0, 1.0),
                    2 => (0.5, 1.0),
                    _ => (0.5, 0.5),
                };
                div()
                    .size_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_center()
                    .children(self.frames.iter().map(|(_, frame)| {
                        div()
                            .w(relative(tile_w))
                            .h(relative(tile_h))
                            .p(px(2.))
                            .child(
                                img(ImageSource::Render(frame.clone()))
                                    .size_full()
                                    .object_fit(ObjectFit::Contain),
                            )
                    }))
                    .into_any_element()
            }
            _ => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(stage_tile(
                    self.avatar.clone(),
                    self.initial.clone(),
                    self.name.clone(),
                    "No one is sharing video — audio only",
                    self.speaking,
                ))
                .into_any_element(),
        }
    }
}

/// The live state of a channel once [`session::join`]
/// connects: the connection handle (to start screen sharing later), the
/// video surface entity the frame-pump task feeds, this side's screen-share,
/// camera, and mute state, and the stop signal for the call's background
/// media tasks (see [`session::CallSession::hang_up`]) — needed on leave, or
/// mic upload and incoming playback just keep running.
struct ConnectedCall {
    pc: corcel_net::CallHandle,
    remote_surface: Entity<VideoSurface>,
    sharing: SharingState,
    /// Whether the quality picker (720p/1080p/2K) is open above the share
    /// button — sharing starts from one of its options, not from the
    /// button itself.
    share_quality_menu: bool,
    camera: CameraState,
    /// Mirror of the value last sent through `mute`, kept here because the
    /// UI needs a synchronous read every render and `watch::Sender::borrow`
    /// would work but hides the "this is UI state" intent.
    muted: bool,
    mute: watch::Sender<bool>,
    /// Mirror of `deafen`, same reasoning as `muted`.
    deafened: bool,
    deafen: watch::Sender<bool>,
    /// What `muted` was when deafen last switched on — deafen implies mute
    /// (Discord's rule), and undeafening restores this instead of blindly
    /// unmuting someone who was already muted on their own.
    muted_before_deafen: bool,
    /// Push-to-talk: while on, the mic idles muted and holding Space (with
    /// no text input focused) unmutes for the duration of the hold.
    ptt_enabled: bool,
    /// Whether this side's mic is currently audible to the room — the local
    /// speech detector's flag gated by `!muted`. Drives our own green ring
    /// with zero network latency (and doubles as a mic-works indicator).
    self_speaking: bool,
    /// A second handle on the mic's speaking flag for synchronous reads —
    /// the primary receiver is consumed by the watcher task. Lets unmute
    /// paths (button, deafen restore, push-to-talk) light the ring
    /// immediately when the mic is already hot, instead of waiting for the
    /// detector's next off→on edge.
    speaking_rx: watch::Receiver<bool>,
    hang_up: watch::Sender<bool>,
    /// Sender into the stage's frame channel, handed to
    /// [`session::start_screen_share`]/[`session::start_camera`] so the
    /// sharer's own stream shows up on their own stage too (see
    /// [`session::CallSession::local_video`]).
    local_video: mpsc::Sender<session::VideoEvent>,
}

/// Stops a connected call's background media tasks (mic upload, incoming
/// audio/video playback) and closes its peer connection — otherwise leaving
/// a channel doesn't actually stop hearing everyone else, since none of
/// that is tied to `ConnectedCall`'s lifetime on its own. Fire-and-forget:
/// the caller (a UI click handler) doesn't wait for this.
fn hang_up(call: &ConnectedCall) {
    let _ = call.hang_up.send(true);
    call.pc.close();
}

/// Drops every stage tile (and its GPU texture). The per-feed `Closed`
/// event normally removes tiles one by one; this is the hang-up sweep so
/// leaving a call can't leak the last frames' textures.
fn clear_stage(surface: &Entity<VideoSurface>, cx: &mut gpui::App) {
    surface.update(cx, |surface, cx| {
        for (_, old) in surface.frames.drain(..) {
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

/// Which step of the add-server journey is on screen. The same three-step
/// flow backs two surfaces: the first-run fullscreen "arrival" (no servers
/// yet) and the rail-"+" modal — so the polished path is the only path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AddServerStage {
    /// The fork: create your own vs join a friend's.
    Choice,
    /// Naming a server this machine will host.
    Create,
    /// Pasting an invite link.
    Join,
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
            name_input: cx.new(|cx| TextInput::new_hero("Your name", cx)),
            bio_input: cx.new(|cx| TextInput::new("A line about you (optional)", cx)),
            avatar_path: None,
            banner_path: None,
            error: None,
        }
    }
}

pub(crate) struct Shell {
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
    /// Who's audibly speaking where: `(voice channel, author) → last
    /// Speaking { true } seen`. Cleared by their explicit `false`, or aged
    /// out by the janitor if that never arrives (see [`SPEAKING_EXPIRY`]).
    speaking: HashMap<(Uuid, String), Instant>,
    /// Who's connected to which voice channel, as learned over the chat
    /// room: `(voice channel, room peer) → display name`. Keyed by peer id
    /// (not name) so a member whose app dies without sending
    /// `VoicePresence { present: false }` is swept out when the relay
    /// reports their room connection gone (`PeerLeft`).
    voice_occupants: HashMap<(Uuid, PeerId), String>,
    /// Replicated peer avatars: author name → cached PNG on disk (see
    /// [`profile::save_peer_avatar`]). Seeded from disk at startup,
    /// updated whenever a `ChatPayload::Profile` arrives.
    peer_avatars: HashMap<String, PathBuf>,
    /// Replicated peer bios, by author name. Memory-only — cheap to
    /// re-receive with the next `ChatPayload::Profile`, unlike avatars.
    peer_bios: HashMap<String, String>,
    /// Who's online in each server's room: `(server, room peer) → display
    /// name`, learned from their `ChatPayload::Profile` (everyone sends one
    /// on entry and to each later arrival). Keyed by peer id so `PeerLeft`
    /// can sweep an entry without knowing the name — the same crash-safety
    /// scheme as [`Self::voice_occupants`]. Drives the member panel's
    /// online section.
    room_members: HashMap<(Uuid, PeerId), String>,
    /// Whether the edit-profile modal (the onboarding identity card,
    /// reopened) is up.
    edit_profile_open: bool,
    /// This user's avatar as it goes on the wire (base64 PNG), computed
    /// lazily on the first room join and reused after — the outer `None`
    /// means "not encoded yet", the inner one "user has no avatar".
    encoded_avatar: Option<Option<String>>,
    /// The message the composer is currently replying to — stamped onto the
    /// next send as its `reply_to`, shown as a bar above the composer, and
    /// cleared by Escape, ✕, or sending.
    replying_to: Option<ChatMessage>,
    /// The message being edited inline, with the dedicated input that
    /// replaces its body. Enter commits, Escape cancels.
    editing: Option<(Uuid, Entity<TextInput>)>,
    /// The message whose inline emoji palette is open, if any.
    reacting_to: Option<Uuid>,
    /// The thread open in the right-hand panel: the root (channel) message
    /// it hangs off, or `None` when no thread is open. Slack-style — a
    /// thread is implied by replies carrying `thread_root`, not an entity
    /// (see [`chat::ChatMessage::thread_root`]).
    open_thread: Option<ChatMessage>,
    /// The open thread's replies, oldest first. Loaded from the store when
    /// the panel opens and refreshed as replies arrive.
    thread_messages: Vec<ChatMessage>,
    /// Per-root `(reply count, newest reply sent_at)` for the on-screen
    /// channel — draws the "N replies" chip under thread roots. Derived
    /// from the store, never replicated, so peers can't diverge on it.
    thread_counts: HashMap<Uuid, (u32, i64)>,
    /// The thread panel's composer.
    thread_input: Entity<TextInput>,
    /// Which row of the composer's @mention autocomplete is highlighted.
    /// Clamped against the current candidate list at use; the popup itself
    /// is derived state (an `@word` under the caret with matches).
    mention_selected: usize,
    /// Live reactions of the on-screen text channel: message → chips in
    /// first-reacted order, each an emoji with everyone who reacted with it.
    /// Rebuilt from the store whenever the channel (re)loads or a reaction
    /// lands (see [`Shell::reload_reactions`]).
    chat_reactions: HashMap<Uuid, Vec<(String, Vec<String>)>>,
    /// Fetched/decoded images for chat embeds, by URL, kept for the app's
    /// lifetime (see [`embeds`]).
    image_embeds: HashMap<String, ImageEmbed>,
    /// Video embeds the user pressed play on, by URL. Torn down on any
    /// navigation via [`Shell::stop_video_embeds`].
    video_embeds: HashMap<String, VideoEmbed>,
    add_server_open: bool,
    /// Where the add-server flow currently stands (see [`AddServerStage`]).
    /// Reset to `Choice` whenever the flow is (re)entered.
    add_server_stage: AddServerStage,
    /// `true` from clicking "Create server" until the async host attempt
    /// resolves — renders the CTA dimmed/inert so the click visibly took.
    hosting_pending: bool,
    /// The reach picked in the Create stage — what [`session::host`] is
    /// called with. Resets to `Global` (the recommended choice) each time
    /// the add-server flow is opened.
    server_reach: Reach,
    /// Briefly `true` after "Copy Invite Link" so the button itself can
    /// confirm the copy happened — clipboard writes are otherwise invisible.
    link_copied: bool,
    error: Option<String>,
}

impl Shell {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
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
            server_name_input: cx.new(|cx| TextInput::new_hero("The Clubhouse", cx)),
            join_link_input: cx.new(|cx| TextInput::new("corcel1…", cx)),
            unread: HashMap::new(),
            switcher_open: false,
            switcher_input: cx.new(|cx| TextInput::new("Where would you like to go?", cx)),
            switcher_selected: 0,
            typing: HashMap::new(),
            last_typing_sent: None,
            speaking: HashMap::new(),
            voice_occupants: HashMap::new(),
            peer_avatars: profile::load_peer_avatars(),
            peer_bios: HashMap::new(),
            room_members: HashMap::new(),
            edit_profile_open: false,
            encoded_avatar: None,
            replying_to: None,
            editing: None,
            open_thread: None,
            thread_messages: Vec::new(),
            thread_counts: HashMap::new(),
            thread_input: cx.new(|cx| TextInput::new("Reply in thread…", cx)),
            mention_selected: 0,
            reacting_to: None,
            chat_reactions: HashMap::new(),
            image_embeds: HashMap::new(),
            video_embeds: HashMap::new(),
            add_server_open: false,
            add_server_stage: AddServerStage::Choice,
            hosting_pending: false,
            server_reach: Reach::default(),
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

        // The ephemeral-presence janitor: whoever stops typing just goes
        // silent, so their entry has to age out here; speaking entries are
        // normally cleared by an explicit `false` and this is their crash
        // safety net. Lives for the whole app; the notify only fires when
        // something actually expired.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let alive = this.update(cx, |shell, cx| {
                    let now = Instant::now();
                    let before = shell.typing.len() + shell.speaking.len();
                    shell.typing.retain(|_, seen| now.duration_since(*seen) < Duration::from_secs(6));
                    shell.speaking.retain(|_, seen| now.duration_since(*seen) < SPEAKING_EXPIRY);
                    if shell.typing.len() + shell.speaking.len() != before {
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
    /// across restarts with the same identity, hence the same endpoint id,
    /// so old invite links keep working from any network.
    fn start_rehosts(&mut self, cx: &mut Context<Self>) {
        for server in &mut self.servers {
            if !server.is_host {
                continue;
            }
            // A hosted row that loaded identity-less was written by the
            // pre-iroh transport (its stored key was a TLS key the new
            // transport rejects). Mint a fresh iroh key and persist it: the
            // server keeps its id, name, and message history, but comes up
            // under a new endpoint id — links shared before the transport
            // switch are dead either way (they carried a LAN address, not
            // an endpoint id), so members need a fresh invite regardless.
            if server.identity.is_none() {
                match RelayIdentity::generate() {
                    Ok(identity) => {
                        server.identity = Some(identity);
                        if let Some(store) = &self.store {
                            let _ = store.save_server(server);
                        }
                    }
                    Err(err) => {
                        eprintln!("corcel-app: couldn't mint a relay identity: {err:#}");
                        continue;
                    }
                }
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
        self.stop_video_embeds(cx);
        self.error = None;
        cx.notify();
    }

    fn my_name(&self) -> String {
        self.profile.as_ref().map(|p| p.name.clone()).unwrap_or_default()
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
            // Onboarding keys: Enter submits the visible step, Escape backs
            // out one stage. These surfaces sit above (or instead of) the
            // chat UI, so returning early here can't eat a composer key.
            if self.profile.is_none() {
                if key == "enter" {
                    self.submit_profile(cx);
                }
                return;
            }
            let arrival_visible = self.add_server_open
                || (self.servers.is_empty() && matches!(self.screen, Screen::Home));
            if arrival_visible {
                match (key, self.add_server_stage) {
                    ("enter", AddServerStage::Create) => self.submit_host(cx),
                    ("enter", AddServerStage::Join) => self.submit_join(cx),
                    ("escape", AddServerStage::Create | AddServerStage::Join) => {
                        self.arrival_go(AddServerStage::Choice, window, cx);
                    }
                    ("escape", AddServerStage::Choice) if self.add_server_open => {
                        self.add_server_open = false;
                        cx.notify();
                    }
                    _ => {}
                }
                return;
            }
            // Push-to-talk: Space held (and not typing anywhere) opens the
            // mic. Key auto-repeat re-fires this, which `set_muted`'s
            // no-change guard absorbs.
            if key == "space" && !self.text_input_focused(window, cx) {
                let holding =
                    self.connected_call_mut().is_some_and(|call| call.ptt_enabled && !call.deafened);
                if holding {
                    self.set_muted(false, cx);
                    return;
                }
            }
            // Escape unwinds in-flight state, most-specific first: the
            // edit-profile modal, an open message edit, then the emoji
            // palette, then the reply setup.
            if key == "escape" {
                if self.edit_profile_open {
                    self.edit_profile_open = false;
                    cx.notify();
                } else if self.editing.is_some() {
                    self.cancel_edit(cx);
                } else if self.reacting_to.take().is_some() || self.replying_to.take().is_some() {
                    cx.notify();
                } else if self.open_thread.is_some() {
                    self.close_thread(cx);
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

    /// The release half of push-to-talk. Runs even if a text input has
    /// focus by the time the key comes up — if PTT never unmuted (the press
    /// landed in a composer), re-muting is a no-op anyway.
    fn root_key_up(&mut self, event: &KeyUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key.as_str() != "space" {
            return;
        }
        let holding = self.connected_call_mut().is_some_and(|call| call.ptt_enabled && !call.deafened);
        if holding {
            self.set_muted(true, cx);
        }
    }

    /// Whether any of the app's text inputs has keyboard focus — the guard
    /// that keeps push-to-talk's Space from firing while typing a message
    /// (or a server name, or a switcher query...).
    /// Called once right after the window opens: on a first run (no profile
    /// yet) the identity screen's name field starts focused, so the user
    /// can just type.
    pub(crate) fn focus_first_run(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.profile.is_none() {
            window.focus(&self.profile_form.name_input.focus_handle(cx));
        }
    }

    fn text_input_focused(&self, window: &Window, cx: &App) -> bool {
        let mut inputs = vec![
            &self.message_input,
            &self.server_name_input,
            &self.join_link_input,
            &self.switcher_input,
            &self.profile_form.name_input,
            &self.profile_form.bio_input,
        ];
        if let Some((_, input)) = &self.editing {
            inputs.push(input);
        }
        inputs.iter().any(|input| input.focus_handle(cx).is_focused(window))
    }

    fn dismiss_error_clicked(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        cx.notify();
    }

}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.profile.is_none() {
            self.render_profile_setup(cx).into_any_element()
        } else if self.servers.is_empty() && matches!(self.screen, Screen::Home) {
            self.render_arrival(cx).into_any_element()
        } else {
            self.render_app_shell(cx)
        };

        let modal = (self.profile.is_some() && self.add_server_open)
            .then(|| deferred(self.render_add_server_modal(cx)).with_priority(1));
        let edit_profile = (self.profile.is_some() && self.edit_profile_open)
            .then(|| deferred(self.render_edit_profile_modal(cx)).with_priority(1));
        let switcher = (self.profile.is_some() && self.switcher_open)
            .then(|| deferred(self.render_quick_switcher(cx)).with_priority(2));

        // App-wide keys (F11, Ctrl+K, the open switcher's navigation) are
        // registered on the outermost element so it's always an ancestor of
        // whatever's focused (a text input, a button, ...) and sees the
        // bubbled event.
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            // Theme ground truth for the whole tree: every screen and —
            // crucially — every `deferred` overlay inherits these, so a
            // modal can never fall back to the window default (black text)
            // just because its own subtree forgot to set a color.
            .bg(theme::background())
            .text_color(theme::foreground())
            .on_key_down(cx.listener(Self::root_key_down))
            .on_key_up(cx.listener(Self::root_key_up))
            // The custom titlebar sits above everything (the native one is
            // hidden/transparent on every platform — see shell/titlebar.rs);
            // the app fills the rest. Modals/overlays are absolute against
            // this root, so they may cover the bar — that's fine, it has no
            // state a modal could hide.
            .child(self.render_titlebar(cx))
            .child(div().flex_1().min_h_0().w_full().relative().child(content))
            .children(modal)
            .children(edit_profile)
            .children(switcher)
    }
}
