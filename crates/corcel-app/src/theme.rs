//! Color and component tokens for the "Corcel Glass" look: Discord's
//! three-tier surface hierarchy (rail < sidebar/card < main content) kept
//! for its UX, reskinned Frutiger-Aero-glass — translucent
//! charcoal surfaces (the icons' own #151515-#575757 ramp) over a
//! system-blurred window, hairline light borders, azure
//! gradient accents, matching the Nucleo Glass icon set.
//!
//! GPUI's `rounded_*` presets are a fixed rem scale (sm=4px, md=6px,
//! lg=8px, xl=12px, 2xl=16px); each radius used below picks the nearest
//! preset for the intended weight (hairline rows vs. cards vs. avatars).

use std::path::PathBuf;

use gpui::{
    AnyView, App, Context, Div, FontWeight, ImageSource, Pixels, Render, Rgba, SharedString, Stateful, Svg, Styled,
    Window, div, img, prelude::*, px, rgb, rgba, svg,
};

/// The server-icon rail's background — the darkest surface.
// ── Corcel Glass ────────────────────────────────────────────────────
// Frutiger-Aero-descended, anchored to the Nucleo Glass icons' own
// ramp (#151515-#575757): near-neutral charcoal surfaces, cool but
// never blue, with real translucency (the window background is
// system-blurred — see main.rs), hairline white borders standing in for
// glass edges, white-wash hovers instead of gray, and azure accents.
// Alphas are deliberate: chrome (rail) is the most transparent, floating
// surfaces (popover) the most opaque so text always stays readable.

pub fn rail() -> Rgba {
    rgba(0x111111d9)
}

/// Channel sidebar / member list background — one step lighter than the
/// rail.
pub fn card() -> Rgba {
    rgba(0x181818d9)
}

/// Elevated surfaces: modals, the call dock, hover profile cards.
pub fn popover() -> Rgba {
    rgba(0x1d1d1df2)
}

/// Main content background (chat, video panel, empty states). Opaque on
/// purpose: per Apple's Liquid Glass layer model, glass belongs to the
/// chrome floating *above* content — content itself stays a solid, calm
/// ground the glass can refract.
pub fn background() -> Rgba {
    rgb(0x121212)
}

/// Hover / active row background within a sidebar or list.
pub fn wash() -> Rgba {
    rgba(0xffffff0d)
}

pub fn wash_strong() -> Rgba {
    rgba(0xffffff1a)
}

pub fn border() -> Rgba {
    rgba(0xffffff0f)
}

pub fn input_border() -> Rgba {
    rgba(0xffffff1f)
}

/// The focus-ring color, used for the one border that's meant to stand out
/// — e.g. a field the user should notice.
pub fn ring() -> Rgba {
    rgba(0x6fc3ffcc)
}

pub fn foreground() -> Rgba {
    rgb(0xf2f3f5)
}

pub fn muted_foreground() -> Rgba {
    rgb(0x949ba4)
}

pub fn faint_foreground() -> Rgba {
    rgb(0x80848e)
}

/// The azure brand accent — used sparingly (primary buttons, the
/// active-server rail indicator), always available as a gradient via
/// [`primary_gradient`].
pub fn primary() -> Rgba {
    rgb(0x2e8fff)
}

pub fn primary_hover() -> Rgba {
    rgb(0x2374d8)
}

pub fn primary_foreground() -> Rgba {
    rgb(0xffffff)
}

pub fn destructive() -> Rgba {
    rgb(0xf23f42)
}

pub fn destructive_foreground() -> Rgba {
    rgb(0xfa777a)
}

/// Online / connected-and-live green.
pub fn success() -> Rgba {
    rgb(0x2ec06c)
}

/// Screen-share / stream accent.
pub fn info() -> Rgba {
    rgb(0x00a8fc)
}

/// Discord's mention gold — the wash + left border on a message row that
/// mentions you, and the base color for `@name` pills.
pub fn mention() -> Rgba {
    rgb(0xfaa61a)
}

/// The dimming backdrop behind modals — the one translucent color in the
/// theme, so it can't be built with `rgb()` like the rest.
pub fn scrim() -> Rgba {
    gpui::rgba(0x000000a0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PillVariant {
    Connecting,
    Live,
    Failed,
}

/// A status pill — used for `ChannelStatus::{Connecting,Connected,Failed}`.
pub fn pill(variant: PillVariant, label: impl Into<SharedString>) -> Div {
    let dot_color = match variant {
        PillVariant::Connecting => faint_foreground(),
        PillVariant::Live => success(),
        PillVariant::Failed => destructive(),
    };
    let text_color = match variant {
        PillVariant::Connecting => muted_foreground(),
        PillVariant::Live => foreground(),
        PillVariant::Failed => destructive_foreground(),
    };

    div()
        .flex()
        .items_center()
        .gap_2()
        .px(gpui::px(10.))
        .py(gpui::px(5.))
        .rounded_full()
        .border_1()
        .border_color(border())
        .bg(raised_fill())
        .text_size(gpui::px(11.5))
        .text_color(text_color)
        .child(div().size(gpui::px(6.)).rounded_full().bg(dot_color))
        .child(label.into())
}

/// The Liquid Glass fill for floating controls (the call bar and friends):
/// noticeably more translucent than [`popover`] so the content behind
/// shows through the pane, per Apple's "controls float above content"
/// layer model. Pair with [`glass_edge`] for the light rim.
pub fn glass() -> Rgba {
    rgba(0x1d1d1da8)
}

/// The hairline light rim of a glass pane — brighter than [`border`], it
/// reads as the pane's lit edge.
pub fn glass_edge() -> Rgba {
    rgba(0xffffff2e)
}

/// The hover/selection edge: [`glass_edge`] turned up. Neutral on
/// purpose — coss hovers brighten, they don't tint.
pub fn edge_strong() -> Rgba {
    rgba(0xffffff52)
}

/// The 1px "border" ring that makes a component read as 3D: light where
/// light would hit (top), falling to shadow at the bottom. GPUI only has
/// one border color per element, so the ring is a gradient-filled wrapper
/// a hair larger than its content — the classic gradient-border trick.
pub fn bevel_ring() -> gpui::Background {
    gpui::linear_gradient(
        180.,
        gpui::linear_color_stop(gpui::rgba(0xffffff29), 0.),
        gpui::linear_color_stop(gpui::rgba(0x00000059), 1.),
    )
}

/// The matching surface fill: faintly lighter at the top than the bottom,
/// so the face of the component curves with the same light as its ring.
pub fn raised_fill() -> gpui::Background {
    gpui::linear_gradient(
        180.,
        gpui::linear_color_stop(gpui::rgba(0x232323ff), 0.),
        gpui::linear_color_stop(gpui::rgba(0x151515ff), 1.),
    )
}

/// Wraps `inner` in a [`bevel_ring`] so it reads as a raised 3D piece.
/// `radius` is the outer radius; the inner element should carry its own
/// fill and a radius ~1px smaller.
pub fn raised(radius: Pixels, inner: Div) -> Div {
    div().rounded(radius).p(px(1.)).bg(bevel_ring()).shadow_md().child(inner)
}

/// The glossy azure fill for primary actions — a vertical light-to-deep
/// gradient, the same idiom as the icon set's glass layers.
/// coss.com/ui's signature primary: a light (near-white) face with dark
/// text — the one high-contrast element on the near-black ground. The
/// vertical falloff plus [`primary_ring`] give it the raised-3D read.
pub fn primary_gradient() -> gpui::Background {
    gpui::linear_gradient(
        180.,
        gpui::linear_color_stop(gpui::rgba(0xffffffff), 0.),
        gpui::linear_color_stop(gpui::rgba(0xdededeff), 1.),
    )
}

/// [`bevel_ring`] for the light primary: bright lit top edge into a
/// gray shadow edge.
pub fn primary_ring() -> gpui::Background {
    gpui::linear_gradient(
        180.,
        gpui::linear_color_stop(gpui::rgba(0xffffffff), 0.),
        gpui::linear_color_stop(gpui::rgba(0x8a8a8aff), 1.),
    )
}

/// The aurora ground the whole app sits on: a charcoal diagonal drift,
/// visible wherever surfaces are translucent. Subtle on purpose — it
/// tints the blur, it doesn't compete with content.
pub fn aurora() -> gpui::Background {
    gpui::linear_gradient(
        150.,
        gpui::linear_color_stop(gpui::rgba(0x1a1a1aff), 0.),
        gpui::linear_color_stop(gpui::rgba(0x0f0f0fff), 1.),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    Ghost,
}

/// A themed button shell — callers still attach `.on_mouse_up(...)` and any
/// layout tweaks (`.mt_4()`, ...) on the returned `Div`.
pub fn button(variant: ButtonVariant, label: impl Into<SharedString>) -> Div {
    let base = div()
        .px(gpui::px(16.))
        .py(gpui::px(9.))
        .text_size(gpui::px(14.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label.into());

    match variant {
        ButtonVariant::Primary => div().rounded(px(10.)).p(px(1.)).bg(primary_ring()).shadow_sm().child(
            base.rounded(px(9.))
                .bg(primary_gradient())
                .text_color(gpui::rgb(0x1a1a1c))
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|style| style.bg(gpui::rgb(0xe6e6e8))),
        ),
        ButtonVariant::Ghost => base
            .rounded_md()
            .text_color(muted_foreground())
            .hover(|style| style.bg(wash()).text_color(foreground())),
    }
}

/// A monochrome icon from the embedded Lucide set (see [`crate::assets`]).
/// GPUI tints the rasterized SVG with the *element's own* text color — and,
/// unlike real text, an `svg()` does NOT inherit its parent's text color
/// (`Svg::paint` reads `style.text.color` from its own style, which is
/// `None` unless set here), so an icon with no explicit color renders as
/// nothing at all. Hence the `foreground()` default: callers override with
/// `.text_color(...)` (and `.group_hover(...)` for hover recoloring), but a
/// forgotten color yields a white icon instead of an invisible one.
pub fn icon(path: &'static str, size: Pixels) -> Svg {
    svg().path(path).size(size).flex_none().text_color(foreground())
}

/// The small dark label shown by [`tooltip`] — GPUI positions it next to the
/// cursor after its standard hover delay.
struct Tooltip {
    label: SharedString,
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(6.))
            .rounded_md()
            .bg(rail())
            .border_1()
            .border_color(input_border())
            .shadow_lg()
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(foreground())
            .child(self.label.clone())
    }
}

/// Builds the closure `.tooltip(...)` wants for a plain text label — every
/// icon-only control in the app gets one, so nothing is left unlabeled.
pub fn tooltip(label: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let label = label.into();
    move |_window, cx| cx.new(|_| Tooltip { label: label.clone() }).into()
}

/// A small square ghost icon button (32px hit target, 18px icon) with hover,
/// pressed, and tooltip states — the workhorse for header/toolbar actions.
/// Callers attach `.on_mouse_up(...)` on the returned element.
///
/// `id` doubles as the button's hover-group name (which is why it's a
/// `&'static str` rather than any `ElementId`): the icon can't inherit the
/// button's hover text color (see [`icon`]), so it recolors itself via
/// `group_hover` on the button's group instead.
pub fn icon_button(id: &'static str, icon_path: &'static str, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .group(id)
        .size(px(32.))
        .rounded_md()
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .cursor_pointer()
        .hover(|style| style.bg(wash()))
        .active(|style| style.bg(wash_strong()))
        .tooltip(tooltip(label))
        .child(
            icon(icon_path, px(18.))
                .text_color(muted_foreground())
                .group_hover(id, |style| style.text_color(foreground())),
        )
}

/// A circular avatar: the picked image if there is one, otherwise a flat
/// fill showing `fallback_initial`.
pub fn avatar(path: Option<PathBuf>, fallback_initial: impl Into<SharedString>, size: gpui::Pixels) -> Div {
    // The rounding lives on the content itself: GPUI's `overflow_hidden`
    // clips to the rectangle, not the corner radius, so a rounded wrapper
    // around a square img still painted a square photo.
    let content: gpui::AnyElement = match path {
        Some(path) => img(ImageSource::from(path))
            .size_full()
            .rounded_full()
            .object_fit(gpui::ObjectFit::Cover)
            .into_any_element(),
        None => div()
            .size_full()
            .rounded_full()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .bg(primary())
            .text_color(primary_foreground())
            .font_weight(FontWeight::BOLD)
            .child(fallback_initial.into())
            .into_any_element(),
    };

    div().size(size).rounded_full().bg(wash_strong()).child(content)
}
