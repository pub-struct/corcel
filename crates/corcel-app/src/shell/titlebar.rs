//! The custom titlebar. The native one is gone on every platform (macOS
//! draws its titlebar transparent, Windows and Linux get a fully custom
//! frame — see `main.rs`'s `TitlebarOptions`), so this strip is the
//! window's drag handle and, off-macOS, its caption buttons too.
//!
//! Per-platform split of responsibilities:
//! - macOS: the native traffic lights float over the bar's left edge
//!   (positioned via `traffic_light_position`), and AppKit itself handles
//!   dragging and double-click-to-zoom in the transparent titlebar region.
//!   We only reserve the space and draw the brand.
//! - Windows: the `window_control_area` hitboxes map straight to native
//!   hit-testing (`HTCAPTION`/`HTCLOSE`/`HTMAXBUTTON`/`HTMINBUTTON`), so
//!   the OS drives dragging, snap layouts, and the button actions; we
//!   just draw the visuals.
//! - Linux: compositors don't hit-test our pixels, so the bar drives the
//!   window manually — `start_window_move` on drag, explicit
//!   minimize/zoom/close handlers on the buttons, `show_window_menu` on
//!   right-click.

use gpui::{MouseDownEvent, WindowControlArea};

use super::*;

/// Bar height. Also the height AppKit's transparent titlebar effectively
/// gets on macOS, so the traffic lights (see `main.rs`) sit centered.
pub(crate) const TITLEBAR_HEIGHT: f32 = 36.;

/// Width reserved on macOS for the native traffic lights.
const TRAFFIC_LIGHTS_WIDTH: f32 = 76.;

impl Shell {
    pub(super) fn render_titlebar(&mut self, _cx: &mut Context<Self>) -> Stateful<Div> {
        let brand = div()
            .flex()
            .items_center()
            .gap(px(7.))
            .child(div().text_size(px(13.)).child("🐴"))
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::muted_foreground())
                    .child("corcel"),
            );

        let mut bar = div()
            .id("titlebar")
            .h(px(TITLEBAR_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .px(px(12.))
            .bg(theme::rail())
            .border_b_1()
            .border_color(theme::border())
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(MouseButton::Left, |event: &MouseDownEvent, window, _cx| {
                // Linux only in practice: on macOS/Windows the platform
                // handles the drag natively (see the module doc) and these
                // calls are no-ops there.
                if event.click_count == 2 {
                    window.titlebar_double_click();
                } else {
                    window.start_window_move();
                }
            })
            .on_mouse_down(MouseButton::Right, |event: &MouseDownEvent, window, _cx| {
                window.show_window_menu(event.position);
            });

        if cfg!(target_os = "macos") {
            bar = bar.child(div().w(px(TRAFFIC_LIGHTS_WIDTH)).flex_none());
        }
        bar = bar.child(brand).child(div().flex_1());

        if !cfg!(target_os = "macos") {
            // Minimize: a short line. Maximize/restore: an outlined square.
            // Close: the X icon. Drawn with plain divs so no new SVG assets
            // are needed at these sizes.
            let minimize_glyph = div().w(px(10.)).h(px(1.)).bg(theme::muted_foreground());
            let maximize_glyph = div().size(px(9.)).border_1().border_color(theme::muted_foreground());
            let close_glyph = theme::icon(icons::X, px(14.)).text_color(theme::muted_foreground());

            bar = bar
                .child(
                    caption_button("titlebar-min", WindowControlArea::Min)
                        .child(minimize_glyph)
                        .on_mouse_up(MouseButton::Left, |_, window, _cx| window.minimize_window()),
                )
                .child(
                    caption_button("titlebar-max", WindowControlArea::Max)
                        .child(maximize_glyph)
                        .on_mouse_up(MouseButton::Left, |_, window, _cx| window.zoom_window()),
                )
                .child(
                    caption_button("titlebar-close", WindowControlArea::Close)
                        .hover(|style| style.bg(theme::destructive()))
                        .child(close_glyph)
                        .on_mouse_up(MouseButton::Left, |_, window, _cx| window.remove_window()),
                );
        }

        bar
    }
}

/// One caption button: full bar height, wide enough to be an easy target,
/// tagged as its window-control area so Windows' native hit-testing (and
/// hover effects like Win11 snap layouts on Max) target it. The click
/// handlers attached by the caller only matter on Linux — on Windows the
/// non-client hit-test consumes the click before GPUI ever sees it.
fn caption_button(id: &'static str, area: WindowControlArea) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(46.))
        .h_full()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .window_control_area(area)
        .hover(|style| style.bg(theme::wash_strong()))
}
