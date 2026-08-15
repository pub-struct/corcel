//! The right-click context menu: one shell-owned overlay that any element
//! can spawn at the cursor with its own item list. Items carry a typed
//! [`ContextAction`] dispatched by [`Shell::run_context_action`] — so the
//! menu is one mechanism, and growing it is adding an enum arm plus a call
//! site. Items not yet backed by a real feature render dimmed with a
//! "soon" chip instead of being hidden, sketching the UI we're building
//! toward (channel management, per-user volume, voice moderation).

use gpui::{MouseDownEvent, Pixels, Point};

use super::*;

pub(super) struct ContextMenu {
    pub position: Point<Pixels>,
    pub items: Vec<ContextMenuItem>,
}

pub(super) struct ContextMenuItem {
    pub label: SharedString,
    pub icon: &'static str,
    pub destructive: bool,
    /// `false` renders the row dimmed with a "soon" chip and no handler.
    pub ready: bool,
    pub action: ContextAction,
}

impl ContextMenuItem {
    pub fn new(label: impl Into<SharedString>, icon: &'static str, action: ContextAction) -> Self {
        Self { label: label.into(), icon, destructive: false, ready: true, action }
    }

    pub fn soon(mut self) -> Self {
        self.ready = false;
        self
    }

    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
}

/// Everything a context-menu row can do. Stub arms exist so their rows can
/// already be sketched in the UI; wiring one up = implementing its arm in
/// [`Shell::run_context_action`] and dropping the `.soon()` at the spawn
/// site.
#[derive(Clone)]
pub(super) enum ContextAction {
    MarkChannelRead { channel: Uuid },
    ToggleSelfMute,
    ToggleSelfDeafen,
    Reply { message: ChatMessage },
    ReplyInThread { message: ChatMessage },
    CopyMessageText { body: String },
    AddReaction { message: Uuid },
    EditMessage { message: ChatMessage },
    DeleteMessage { message: Uuid },
    CopyInvite { server: Uuid },
    MarkAllRead { server: Uuid },
    MentionUser { author: String },
    EditChannelName { channel: Uuid },
    MuteChannel { channel: Uuid },
    LeaveServer { server: Uuid },
    ViewProfile { author: String },
    AdjustUserVolume { author: String },
    KickFromVoice { author: String },
}

impl Shell {
    pub(super) fn open_context_menu(
        &mut self,
        event: &MouseDownEvent,
        items: Vec<ContextMenuItem>,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = Some(ContextMenu { position: event.position, items });
        cx.notify();
    }

    pub(super) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn run_context_action(&mut self, action: ContextAction, window: &mut Window, cx: &mut Context<Self>) {
        self.close_context_menu(cx);
        match action {
            ContextAction::MarkChannelRead { channel } => {
                self.mark_channel_read(channel);
                cx.notify();
            }
            ContextAction::ToggleSelfMute => {
                let Some(call) = self.connected_call_mut() else { return };
                let next = !call.muted;
                self.set_muted(next, cx);
                cx.notify();
            }
            ContextAction::ToggleSelfDeafen => self.toggle_deafen(cx),
            ContextAction::Reply { message } => self.start_reply(message, window, cx),
            ContextAction::ReplyInThread { message } => self.open_thread(message, cx),
            ContextAction::CopyMessageText { body } => {
                cx.write_to_clipboard(ClipboardItem::new_string(body));
            }
            ContextAction::AddReaction { message } => {
                self.reacting_to = Some(message);
                cx.notify();
            }
            ContextAction::EditMessage { message } => self.start_edit(&message, window, cx),
            ContextAction::DeleteMessage { message } => self.delete_message(message, cx),
            ContextAction::CopyInvite { server } => {
                if let Some(server) = self.servers.iter().find(|s| s.link.id == server) {
                    cx.write_to_clipboard(ClipboardItem::new_string(server.link.encode()));
                }
            }
            ContextAction::MarkAllRead { server } => {
                let channels: Vec<Uuid> = self
                    .servers
                    .iter()
                    .find(|s| s.link.id == server)
                    .map(|s| s.link.channels.iter().map(|c| c.id).collect())
                    .unwrap_or_default();
                for channel in channels {
                    self.mark_channel_read(channel);
                }
                cx.notify();
            }
            ContextAction::MentionUser { author } => {
                let content = {
                    let input = self.message_input.read(cx);
                    let content = input.content.to_string();
                    if content.is_empty() || content.ends_with(char::is_whitespace) {
                        format!("{content}@{author} ")
                    } else {
                        format!("{content} @{author} ")
                    }
                };
                self.message_input.update(cx, |input, cx| input.set_content(content, cx));
                window.focus(&self.message_input.focus_handle(cx));
            }
            // Sketched, not shipped — their rows render with a "soon" chip
            // and never dispatch (see `ContextMenuItem::soon`).
            ContextAction::EditChannelName { .. }
            | ContextAction::MuteChannel { .. }
            | ContextAction::LeaveServer { .. }
            | ContextAction::ViewProfile { .. }
            | ContextAction::AdjustUserVolume { .. }
            | ContextAction::KickFromVoice { .. } => {}
        }
    }

    pub(super) fn render_context_menu(&mut self, cx: &mut Context<Self>) -> Div {
        let Some(menu) = &self.context_menu else { return div() };
        let position = menu.position;

        let rows: Vec<_> = menu
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let color = if !item.ready {
                    theme::faint_foreground()
                } else if item.destructive {
                    theme::destructive_foreground()
                } else {
                    theme::foreground()
                };
                let row = div()
                    .id(SharedString::from(format!("context-item-{index}")))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(13.))
                    .text_color(color)
                    .child(theme::icon(item.icon, px(14.)).text_color(color))
                    .child(div().flex_1().min_w_0().whitespace_nowrap().child(item.label.clone()));
                if item.ready {
                    let action = item.action.clone();
                    row.cursor_pointer().hover(|style| style.bg(theme::wash_strong())).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |shell, _, window, cx| {
                            shell.run_context_action(action.clone(), window, cx);
                        }),
                    )
                } else {
                    row.child(
                        div()
                            .flex_none()
                            .px(px(5.))
                            .py(px(1.))
                            .rounded_full()
                            .bg(theme::wash())
                            .text_size(px(9.5))
                            .text_color(theme::faint_foreground())
                            .child("soon"),
                    )
                }
            })
            .collect();

        div().absolute().top_0().left_0().size_full().child(
            div()
                .id("context-menu")
                .absolute()
                .left(position.x)
                .top(position.y)
                .min_w(px(190.))
                .flex()
                .flex_col()
                .gap(px(1.))
                .p(px(4.))
                .rounded_lg()
                // More frost than the call bar: GPUI has no per-element
                // backdrop blur, so a see-through menu reads as leaky
                // rather than glassy — near-opaque like Discord's.
                .bg(theme::popover())
                .border_1()
                .border_color(theme::glass_edge())
                .shadow_lg()
                .on_mouse_down_out(cx.listener(|shell, _, _window, cx| shell.close_context_menu(cx)))
                .children(rows)
                .with_animation(
                    "context-menu-in",
                    Animation::new(Duration::from_millis(120)).with_easing(ease_out_quint()),
                    |menu, delta| menu.opacity(delta).mt(px(-4. * (1. - delta))),
                ),
        )
    }
}
