# corcel — Discord-features implementation plan

Ten features from the research pass, fully designed and ordered into five
batches. Every design decision is already made here; an implementation pass
should be able to execute a batch mechanically, compile, run the test
checklist, and move on. **Do the batches in order** — each one builds on the
previous one's plumbing.

---

## Context for the implementer (read first)

Workspace crates and the files each batch touches:

- `crates/corcel-app/src/main.rs` — the whole UI (`Shell` entity). Key
  state: `servers: Vec<SavedServer>`, `screen: Screen { Home | Server { id,
  view: Lobby|Text|Voice } }`, `call: Option<ActiveCall>`, `chat:
  Option<ChatRoom>` + `chat_generation: u64`, `chat_messages:
  Vec<ChatMessage>`, `message_input: Entity<TextInput>`.
- `crates/corcel-app/src/chat.rs` — `ChatMessage { id: Uuid, channel,
  author: String, sent_at: i64, body }`, `ChatPayload { Message,
  HistoryRequest { since }, HistoryBatch { messages } }` (serde
  `tag = "kind"`, snake_case), `HISTORY_OVERLAP_MILLIS`, `now_millis()`.
- `crates/corcel-app/src/store.rs` — SQLite (`rusqlite` bundled), owned by
  the GPUI main thread only. Tables `servers`, `messages`. All replication
  convergence rests on `insert_message`'s idempotence.
- `crates/corcel-app/src/session.rs` — `host`/`rehost`/`join`/
  `join_as_host`/`open_room`; `CallSession { pc, remote_video, hang_up:
  watch::Sender<bool>, mute: watch::Sender<bool>, local_video }`.
  `attach_media` owns the mic-upload loop (drops packets while muted) and
  spawns one `AudioPlayback` per incoming audio track.
- `crates/corcel-media/src/capture.rs` — GStreamer capture pipelines
  (`microphone()` returns Opus RTP packets). `playback.rs` —
  `AudioPlayback` (RTP → speakers), `VideoPlayback`.
- `crates/corcel-signal` — QUIC-over-TLS relay. Rooms already exist:
  `ClientMessage::{Room, Publish, Direct}` /
  `ServerMessage::{RoomWelcome, Published, Direct}` route opaque
  `serde_json::Value` payloads per server id.
- `crates/corcel-app/src/text_input.rs` — custom `TextInput`; no change
  callback. Enter-to-send works by catching the bubbled `KeyDownEvent` on
  the composer wrapper — piggyback there for typing detection too.

GPUI gotchas already learned the hard way:

- `svg()` does **not** inherit text color — `theme::icon()` sets a default;
  recolor at call sites or via `group_hover` (group names must be unique).
- Stateful divs: `.id(...)` first, then `.overflow_y_scroll()` /
  `.track_scroll(&handle)`. `ScrollHandle::scroll_to_bottom()` is deferred —
  safe to call anytime.
- Overlays render via `deferred(...).with_priority(1)` at the root.
- Async: tokio work goes through `runtime::spawn_and_send(fut)` → await the
  oneshot inside `cx.spawn`. Guard every re-entry into `Shell` with a
  staleness check (`chat_generation`, or "does `self.call` still match").
- New icons: drop a monochrome `currentColor` Lucide SVG in
  `assets/icons/`, add a const in `assets.rs::icons` **and** an entry in the
  `ASSETS` array.

### Ground rules for every batch

1. **Wire compatibility.** Old and new builds will coexist on friends'
   machines. Only *add* serde fields, always `#[serde(default)]`; never
   rename `ChatPayload` tags. Unknown JSON fields are ignored by default —
   rely on that.
2. **Convergence over ordering.** Anything replicated must be an upsert
   keyed by stable ids, monotone (tombstones never resurrect), and carried
   by history backfill so offline peers converge. If a feature can't satisfy
   that, it's ephemeral (typing) or local (unread) — never "mostly synced".
3. **Local first.** Every write lands in SQLite before the network hears
   about it. The network failing must never make the UI wrong.
4. **One migration story.** Schema changes go through `PRAGMA user_version`
   (Batch 0). Never edit an existing migration; append.
5. Before starting: `git init` + an initial commit, and a commit per batch.
   (Also unlocks agent worktrees for any future parallel work.)

---

## Batch 0 — Foundations (do this before any feature)

### 0a. Real schema migrations

`store.rs::open()`: read `PRAGMA user_version`. `0` → create current schema,
set to `1`. Each later batch appends a numbered migration block (`ALTER
TABLE` / `CREATE TABLE`) and bumps the version. Structure it as a
`const MIGRATIONS: &[fn(&Connection) -> rusqlite::Result<()>]` walked from
the stored version.

### 0b. Always-on rooms — one per saved server

**Why this is foundational:** today only the *active* server holds an open
chat room (`Shell::chat: Option<ChatRoom>`), so messages for other servers
don't replicate until you click into them — which makes unread badges
(Batch 1) and background mentions impossible, and breaks "every user is a
host" whenever the app is open but showing a different server.

Change `Shell`:

```rust
rooms: HashMap<Uuid, ChatRoom>,          // replaces chat: Option<ChatRoom>
struct ChatRoom { outbound: UnboundedSender<ClientMessage>, generation: u64 }
```

- `connect_chat(server_id)` becomes per-server: keep the per-room generation
  (store it in the `ChatRoom` and in a `room_generations: HashMap<Uuid,
  u64>` bumped on reconnect/removal); the pump loop's staleness check
  compares against that server's entry.
- Call `connect_chat` for **every** saved server in `Shell::new` (and for a
  server added by host/join). `open_server` no longer reconnects — it only
  switches the view. `leave_server_clicked` bumps that server's generation
  and removes the entry.
- Reconnect policy: when a pump ends and the entry is still current, retry
  with backoff (5s, 15s, 60s, then every 60s) instead of giving up — hosts
  restart, laptops sleep. Keep the existing 5×1s fast retry at startup.
- `handle_room_message` / `absorb_messages` / `send_room` all take the
  server id they already receive and look up `rooms` — messages arriving
  for a *background* server must be absorbed into SQLite even though no UI
  shows them (that's the whole point).

**Test:** run two instances (second with `XDG_CONFIG_HOME` pointed
elsewhere), join the same server, look at *different* servers on each, send
messages — both databases must receive them (visible on click-in without any
backfill delay).

---## Batch 1 — Local-only wins (no protocol changes, no compat risk)

### 1. Unread markers + mention badges

- Migration: `CREATE TABLE last_read (channel_id BLOB PRIMARY KEY,
  last_read INTEGER NOT NULL)`. Store methods: `last_read(channel)`,
  `set_last_read(channel, ts)`, and `unread_counts(server_id) ->
  Vec<(ChannelId, u32, u32)>` (unread, mentions) — one query joining
  messages against last_read; a message mentions you if body matches
  `@name` (LIKE is fine at friends scale).
- `Shell` keeps `unread: HashMap<ChannelId, (u32, u32)>` refreshed from the
  store whenever `absorb_messages` stores something new, and zeroed for a
  channel in `open_text_channel` (+ `set_last_read(now)`); also bump
  last_read on new messages arriving *while viewing* that channel.
- UI: channel row — unread channels render `foreground()` + SEMIBOLD
  instead of muted, with a small white dot pill on the row's left edge;
  mention count is a red `destructive()` badge on the right. Rail bubble —
  white dot (any unread) lower-right, red count badge if any mentions,
  aggregated across the server's channels.

### 2. Quick switcher (Ctrl+K)

- Root `on_key_down` (next to the F11 handler): `key == "k" &&
  modifiers.control` toggles `switcher_open: bool`; Escape closes.
- Overlay via `deferred(...).with_priority(2)` (above the add-server
  modal): centered card, a `TextInput` (focus it on open), and a result
  list over all servers + channels. Scoring: simple case-insensitive
  subsequence match, rank by (starts-with > contains > subsequence), no
  crate needed. `switcher_selected: usize` moved by up/down arrows on the
  same key handler; Enter opens (server → `open_server`; text channel →
  `open_server` + `open_text_channel`; voice → `open_server` +
  `enter_voice_channel`).

### 3. Markdown rendering (display only)

- New module `richtext.rs`: hand-rolled single-pass inline parser for
  `**bold**`, `*italic*`, `~~strike~~`, `` `code` ``, and bare URLs. Output:
  `Vec<Span { text, style: SpanStyle }>`. No markdown crate — the subset is
  tiny and we control edge cases (unclosed markers render literally).
- Render with `gpui::StyledText::new(full_text).with_runs(...)` building a
  `TextRun` per span (bold → weight, italic → `FontStyle::Italic`, code →
  mono font + `wash()` background, strike → strikethrough, URL →
  `info()` color + underline). Fenced ``` blocks: if the body starts and
  ends with ```, render the inner text in a mono `wash()` card instead.
- The composer stays plain text — this is display-side only.

### 4. Mentions (highlight)

- In `richtext.rs`, tokenize `@word` into a `Mention` span; render as
  `primary()`-tinted text on a translucent primary background (pill-ish
  run). If the mention equals *your* profile name, give the whole message
  row a faint yellow-tinted left border + background wash (Discord's
  mention highlight). No autocomplete popup in this batch.

**Batch 1 test:** messages with `**bold** *italic* `code`` render styled;
`@YourName` highlights the row; Ctrl+K fuzzy-jumps everywhere; unread dot
appears on a background server when the other instance sends, clears on
open.

---

## Batch 2 — Typing indicators (ephemeral, tiny)

- `ChatPayload::Typing { channel: ChannelId, author: String }` — published,
  **never stored**, ignored by old builds (unknown tag → serde error →
  already dropped silently; verify the `Err(_) => {}` arm).
- Sending: in the composer's `on_key_down`, if now − `last_typing_sent` >
  3s and the input is non-empty, `send_room(Typing…, None)` on the active
  server's room and stamp it.
- Receiving: `Shell.typing: HashMap<(ChannelId, String), std::time::Instant>`
  updated in `handle_room_message` (ignore your own name); a 2s repeating
  timer task (spawn once in `Shell::new`) prunes entries older than 6s and
  notifies if it removed any.
- UI: 18px strip between scrollback and composer: "Alice is typing…" /
  "Alice and Bob are typing…" / "Several people are typing…", muted, only
  for the on-screen channel.

---

## Batch 3 — Replicated message features (the schema batch)

One migration for all of it:

```sql
ALTER TABLE messages ADD COLUMN reply_to BLOB;
ALTER TABLE messages ADD COLUMN edited_at INTEGER;
ALTER TABLE messages ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0;
CREATE TABLE reactions (
  message_id BLOB NOT NULL, emoji TEXT NOT NULL, author TEXT NOT NULL,
  at INTEGER NOT NULL, removed INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (message_id, emoji, author)
);
CREATE INDEX reactions_by_time ON reactions (at);
```

`ChatMessage` gains `#[serde(default)] reply_to: Option<Uuid>`,
`#[serde(default)] edited_at: Option<i64>`, `#[serde(default)] deleted:
bool` — old peers ignore the fields; new peers get defaults from old peers.

**`insert_message` becomes a merge (this is the heart of the batch):**

```sql
INSERT INTO messages (...) VALUES (...)
ON CONFLICT(id) DO UPDATE SET
  body      = excluded.body,
  edited_at = excluded.edited_at,
  deleted   = MAX(messages.deleted, excluded.deleted)
WHERE COALESCE(excluded.edited_at, 0) > COALESCE(messages.edited_at, 0)
   OR excluded.deleted > messages.deleted
```

Return "changed?" via `conn.changes() > 0`. Properties: last-writer-wins on
`edited_at`; delete is monotone (tombstone never un-deletes); and because
`HistoryBatch` ships full merged rows, backfill reconciles every ordering —
including an edit that raced ahead of its original.

### 6. Reply-to

- UI: hover action row on each message (right-aligned, visible via
  `group_hover` — give each row `group(("msg", id))`): Reply, React, and
  (own messages only) Edit / Delete icons. New icons: `reply.svg`,
  `smile-plus.svg`, `pencil.svg`, `trash-2.svg`.
- Reply sets `Shell.replying_to: Option<ChatMessage>`; composer shows a
  "Replying to **Alice** — ✕" bar above the input; send stamps `reply_to`
  and clears it; Escape in the composer clears it.
- Render: a compact quoted line above the message (spine + author +
  first ~80 chars, muted, from a `HashMap<Uuid, &ChatMessage>` built during
  render; "Original message not loaded" fallback).

### 7. Edit / delete

- Payloads: `ChatPayload::Edit { message_id: Uuid, channel: ChannelId,
  edited_at: i64, body: String }`, `ChatPayload::Delete { message_id: Uuid,
  channel: ChannelId, deleted_at: i64 }`. Apply = load row, merge via the
  upsert above (an Edit for an unknown id is dropped — backfill fixes it).
- Edit UX: pencil swaps the message body for an inline `TextInput`
  (`Shell.editing: Option<(Uuid, Entity<TextInput>)>`), Enter commits
  (update local row, publish Edit), Escape cancels. Render `(edited)` in
  faint 10px after the timestamp when `edited_at.is_some()`.
- Delete UX: trash → store `deleted = 1` locally, publish Delete. Deleted
  rows render as an italic muted "message deleted" stub (tombstones must
  stay visible as rows so reply quotes don't dangle). No confirm dialog —
  it's your own message and the stub is honest.

### 5. Reactions

- Payload: `ChatPayload::Reaction { message_id: Uuid, channel: ChannelId,
  emoji: String, author: String, at: i64, removed: bool }`. Store upsert on
  the `(message_id, emoji, author)` key, LWW by `at` (same WHERE pattern).
- Backfill: `HistoryBatch` gains `#[serde(default)] reactions:
  Vec<ReactionRow>`; `HistoryRequest.since` filters reactions by `at` too.
  Old peers just omit/ignore the field.
- UI: React hover-action opens a small deferred popover with a fixed
  palette (👍 ❤️ 😂 😮 😢 🎉 — plain text children, no emoji picker).
  Existing reactions render as chips under the body (`emoji + count`),
  `wash()` bg, `primary()` border when you're among the reactors; clicking
  a chip toggles yours (publish with `removed` flipped).
- Store method: `reactions_for(channel) -> HashMap<Uuid, Vec<(String,
  Vec<String>)>>` loaded alongside `messages(channel)` into a
  `Shell.chat_reactions` map, refreshed by the same absorb path.

**Batch 3 test (two instances):** edit converges (and `(edited)` shows),
delete tombstones on both and survives restart + backfill, reactions toggle
and converge, offline instance receives all three after reconnect via
backfill, replies quote correctly.

---

## Batch 4 — Voice presence (media plumbing)

### 2b. Deafen

- `CallSession` gains `deafen: watch::Sender<bool>` (create next to `mute`
  in `attach_media`); every per-track audio playback loop checks
  `*deafen_rx.borrow()` before pushing packets into `AudioPlayback` —
  exactly the pattern the mic loop uses for mute. Discord rule: deafen
  implies mute (UI enforces: deafening sets muted too; undeafen restores
  the previous mute state — keep `muted_before_deafen: bool` on
  `ConnectedCall`).
- UI: headphones icon (`headphones.svg` / `headphone-off.svg`) next to the
  mic in the sidebar footer and the call control bar.

### 2c. Push-to-talk (in-window)

- Settings-lite: `ptt_enabled: bool` on `ConnectedCall` toggled from a
  small toggle in the voice panel. While enabled the mic is mute-by-default;
  root-level `on_key_down`/`on_key_up` for `space` (only when no `TextInput`
  is focused — check `window.focused(cx)` against the inputs) flips
  `mute` false/true. Label the limitation honestly in the tooltip: works
  while the window is focused (global hotkeys are OS-specific; out of
  scope).

### 1. Speaking indicators

- **Detection** (`corcel-media/capture.rs::microphone()`): insert
  GStreamer's `level` element (`audioconvert ! level interval=100000000 !
  …` before the Opus encoder) and watch bus messages for the `level`
  element's RMS; expose `speaking: watch::Receiver<bool>` on `Capture` —
  true when RMS > ~-40 dB, with 300ms hang-over before dropping to false
  (prevents flicker between words).
- **Broadcast**: over the *server room* (Batch 0 guarantees it's always
  connected): `ChatPayload::Speaking { channel: ChannelId, author: String,
  speaking: bool }` — ephemeral, never stored. The mic loop in
  `attach_media` can't see the room, so surface `speaking` on
  `CallSession`, and let `Shell` (which owns both the call and the rooms)
  spawn a watcher task that publishes on changes, suppressed while
  muted/deafened-implied-mute.
- **UI**: `Shell.speaking: HashMap<String, Instant>` (author → last true),
  same pruning timer as typing (expire 1s). Green `success()` 2px ring
  around the avatar in the voice participant row and on stage tiles.
  Your own ring drives directly off your local `speaking` receiver — zero
  latency, and it doubles as a mic-works indicator.

### Stretch: per-user volume

Add a `volume` element to `AudioPlayback`'s pipeline and expose
`set_volume(f64)`; keep the per-track playback handles in a map keyed by
track id on `ConnectedCall`; a slider in the participant row hover. Only
attempt after the rest of Batch 4 lands — it requires keeping handles the
current code deliberately fire-and-forgets.

---

## Explicitly out of scope (don't drift into these)

- Roles/permissions (needs a trust authority — a real design problem for
  serverless; do a dedicated design pass first).
- File/image attachments (large-blob replication story needed).
- Global (out-of-window) push-to-talk hotkeys; emoji picker; message
  pagination (revisit when a channel exceeds a few thousand rows —
  `messages(channel)` currently loads everything).
- E2EE DMs — great P2P fit, separate design pass.

## Suggested commit points

`git init` → "batch 0: migrations + always-on rooms" → "batch 1: unread,
switcher, markdown, mentions" → "batch 2: typing" → "batch 3: replies,
edit/delete, reactions" → "batch 4: deafen, PTT, speaking".
