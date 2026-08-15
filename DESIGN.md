# corcel UI Revamp — Design Brief

The plan for the Phase-1.5 visual/interaction pass over the server rail, channel
sidebar, and in-call view. Grounded in the classics — Don Norman's *The Design
of Everyday Things* (affordances, feedback, mapping), Steve Krug's *Don't Make
Me Think* (self-evident UI, no instruction labels), *Refactoring UI*
("hierarchy is everything"), the Laws of UX (Fitts, Hick, Jakob, Gestalt), and
Apple's design principles (instant feedback, purpose, familiarity, craft).

## The audit — what's ugly and *why* it's ugly

| # | Problem | Principle violated |
|---|---------|-------------------|
| 1 | Icons are text glyphs: `))` for voice, `▢` share, `◔` camera, `●` mic, `×` hang-up | Recognition over recall; Jakob's law (nobody has seen these before) |
| 2 | Mic circle *looks* like a button but is an indicator — and there is no mute at all | Norman: false affordance; the #1 call control is missing |
| 3 | Hang-up is a 28px `×` — the most consequential action is the smallest target | Fitts's law; error prevention |
| 4 | Call controls live in a small card at the bottom-*left*, far from the stage the user is watching | Fitts (distance to attention), Gestalt proximity (controls far from what they control) |
| 5 | Pending states (share/camera starting) are invisible — clicks appear to do nothing | Norman: feedback gap |
| 6 | "hover a name" caption in member list | Krug: if it needs an instruction label, the design failed |
| 7 | No persistent user footer (avatar/name/status) — Discord's most recognized pattern | Jakob's law: users expect it bottom-left |
| 8 | Voice connection state floats in a dock overlapping two columns, positioned by the magic number `72 + 240 − 24` | Mapping; fragile layout coupling |
| 9 | Channel rows: cramped 6px padding, no icon system, no "connected" state distinct from "viewing" | Hierarchy; state visibility |
| 10 | Server rail icon's only hover feedback is a corner-radius change; indicator pill positioned by `left(-12)` hack | Feedback; craft |
| 11 | Errors are bare red strings that shift layout when they appear | Feedback kinds (status/warn/error); layout stability |
| 12 | No tooltips anywhere; icon-only buttons are unlabeled | Recognition; discoverability |

## Doctrine for the rewrite

1. **Every control is honest** — looks pressable ⇔ is pressable; shows its
   state (idle / pending / active) the moment it changes; pressed feedback on
   mouse-down, not mouse-up (Apple: response).
2. **Hierarchy through the token scale** — three surfaces (rail < card <
   background), one accent, size/weight ladder for text. No raw hex in
   `main.rs`; everything through `theme.rs`.
3. **Controls live next to what they control** — call controls centered at the
   bottom of the stage; voice-connection state above the user footer in the
   sidebar (Discord's mapping); leave/disconnect separated from the toggle
   cluster and colored destructive.
4. **Follow Discord's shape, Apple's finish** — Jakob's law says borrow the
   layout users already know; craft (44px targets, deliberate spacing,
   tooltips, real icons) makes it feel finished.
5. **Show only what the app truly knows** — no fake rosters, no dead controls.
   (Which is why mute gets *wired*, not painted: `session.rs` gains a
   mute flag checked by the mic-upload task.)

## The spec

- **Icon system** — Lucide SVGs (ISC) embedded via a gpui `AssetSource`
  (`assets.rs` + `crates/corcel-app/assets/icons/`), tinted by text color.
  volume-2, mic, mic-off, video, video-off, monitor-up, phone, link, plus, x,
  users, log-out, user.
- **Server rail** — pill indicator (hidden → 20px on hover → 40px active) flush
  to the rail edge; circle→rounded-square morph on hover/active; tooltips
  (server name, "Add a Server"); solid + button.
- **Channel sidebar** — header with truncated server name + Host badge +
  copy-invite icon button; roomier channel rows (34px, volume icon, rounded,
  hover/active/connected states); connected channel nests a self-participant
  row (avatar + name + mic/share state icons). Bottom: **voice panel**
  (status-colored "Voice Connected" + channel name + disconnect icon button)
  above a **user footer** (avatar with online dot, name, status, mute toggle,
  leave-server) on the darkest surface.
- **In-call view** — header toolbar (channel icon + name, status pill, members
  toggle icon button). Dark stage; video edge-to-edge or an avatar tile when
  audio-only; failed state gets a Try Again. **Centered floating control bar**:
  mute / camera / share (44px circles: idle = neutral, pending = dimmed,
  active = colored) + separated red hang-up pill with rotated phone icon.
  Tooltips on everything. Errors surface as a dismissible toast at the top of
  the stage.
- **Perf fixes that ride along** (from the core-analysis pass): drop the old
  GPU texture when a new video frame replaces it (`cx.drop_image`); move the
  video frame into its own `VideoSurface` entity so 30fps frames stop
  re-rendering the whole shell; single-copy RTP marshal in `corcel-media`.

## Out of scope (unchanged)

Onboarding/profile setup, Home screen, add-server modal (already fine per
product owner), text chat, real participant roster (needs signaling support),
TURN, non-Linux targets.
