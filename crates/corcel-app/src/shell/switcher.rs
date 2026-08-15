//! The Ctrl+K quick switcher: fuzzy matching over every server and channel,
//! keyboard-driven selection, and the palette overlay itself.

use super::*;

/// How well `candidate` matches the switcher query — higher is better,
/// `None` filters the row out. Empty queries match everything, so the
/// switcher opens showing the full list.
pub(super) fn match_score(candidate: &str, query: &str) -> Option<u8> {
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

pub(super) fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|needed| chars.any(|c| c == needed))
}

impl Shell {
    /// The filtered, ranked switcher rows for a query: all servers and all
    /// of their channels, best matches first (ties keep rail/sidebar order,
    /// the sort is stable).
    pub(super) fn switcher_items(&self, query: &str) -> Vec<SwitcherItem> {
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

    pub(super) fn open_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.switcher_open = true;
        self.switcher_selected = 0;
        self.switcher_input.update(cx, |input, cx| input.clear(cx));
        window.focus(&self.switcher_input.focus_handle(cx));
        cx.notify();
    }

    pub(super) fn close_switcher(&mut self, cx: &mut Context<Self>) {
        self.switcher_open = false;
        cx.notify();
    }

    /// Enter (or a row click): jump to the selected result — a server's
    /// lobby, a text channel, or straight into a voice channel.
    pub(super) fn switcher_activate(&mut self, cx: &mut Context<Self>) {
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

    /// The Ctrl+K quick switcher overlay: a centered card with a query
    /// input and the ranked result list. Rendered above every other overlay
    /// (priority 2 — the add-server modal is 1).
    pub(super) fn render_quick_switcher(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
            )
            .with_animation(
                "switcher-in",
                Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint()),
                |card, delta| card.opacity(delta).mt(px(-10. * (1. - delta))),
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

}
