//! Display-side rendering of the tiny inline-markdown subset chat supports:
//! `**bold**`, `*italic*`, `~~strike~~`, `` `code` ``, fenced ``` blocks,
//! bare URLs, and `@mentions`. Hand-rolled on purpose — the subset is small
//! enough that owning the edge cases (an unclosed marker renders literally,
//! nothing nests) beats pulling in a markdown crate whose flavor we'd fight.
//!
//! Messages travel and persist as the raw text the author typed; parsing
//! happens per render. The composer knows nothing about any of this.

use std::ops::Range;

use gpui::{
    AnyElement, FontStyle, FontWeight, HighlightStyle, StrikethroughStyle, StyledText, UnderlineStyle, div,
    prelude::*, px,
};

use crate::theme;

/// How one parsed run of a message body should render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanKind {
    Plain,
    Bold,
    Italic,
    Strike,
    Code,
    Url,
    Mention,
}

/// One run of display text (markers already stripped) and its style.
#[derive(Debug, Clone)]
struct Span {
    text: String,
    kind: SpanKind,
}

/// Parses a message body into display spans. Single left-to-right pass; a
/// marker only takes effect if its closing pair exists with something
/// between them, otherwise the character is literal text. Styles don't nest.
fn parse(body: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut prev: Option<char> = None;
    let mut i = 0;

    let flush = |plain: &mut String, spans: &mut Vec<Span>| {
        if !plain.is_empty() {
            spans.push(Span { text: std::mem::take(plain), kind: SpanKind::Plain });
        }
    };

    while i < body.len() {
        let rest = &body[i..];
        if let Some((consumed, span)) = try_marker(rest, prev) {
            flush(&mut plain, &mut spans);
            prev = span.text.chars().last();
            spans.push(span);
            i += consumed;
            continue;
        }
        let ch = rest.chars().next().expect("i is always on a char boundary");
        plain.push(ch);
        prev = Some(ch);
        i += ch.len_utf8();
    }
    flush(&mut plain, &mut spans);
    spans
}

/// Tries to read one styled span at the start of `rest`. Returns the byte
/// length consumed from the body and the span, or `None` if `rest` starts
/// with plain text.
fn try_marker(rest: &str, prev: Option<char>) -> Option<(usize, Span)> {
    for (marker, kind) in
        [("**", SpanKind::Bold), ("~~", SpanKind::Strike), ("*", SpanKind::Italic), ("`", SpanKind::Code)]
    {
        if let Some(inner_and_beyond) = rest.strip_prefix(marker) {
            if let Some(end) = inner_and_beyond.find(marker) {
                if end > 0 {
                    let inner = &inner_and_beyond[..end];
                    return Some((marker.len() * 2 + end, Span { text: inner.to_string(), kind }));
                }
            }
        }
    }

    if rest.starts_with("http://") || rest.starts_with("https://") {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let url = &rest[..end];
        // A bare scheme isn't a link worth styling.
        if url.len() > "https://".len() {
            return Some((end, Span { text: url.to_string(), kind: SpanKind::Url }));
        }
    }

    // `@word` is a mention only at a word boundary — an email's `@` stays
    // plain text.
    if rest.starts_with('@') && !prev.is_some_and(|c| c.is_alphanumeric()) {
        let name_len = rest[1..]
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(rest.len() - 1);
        if name_len > 0 {
            let text = &rest[..1 + name_len];
            return Some((text.len(), Span { text: text.to_string(), kind: SpanKind::Mention }));
        }
    }

    None
}

/// The inner text of a message that is entirely one fenced ``` code block,
/// or `None` for everything else. An opening-line language tag (```rust) is
/// dropped, Discord-style.
fn fenced_code(body: &str) -> Option<String> {
    let trimmed = body.trim();
    let inner = trimmed.strip_prefix("```")?.strip_suffix("```")?;
    if inner.is_empty() {
        return None;
    }
    let inner = match inner.split_once('\n') {
        Some((first_line, remainder))
            if !remainder.trim().is_empty()
                && !first_line.trim().is_empty()
                && first_line.trim().chars().all(|c| c.is_alphanumeric()) =>
        {
            remainder
        }
        _ => inner,
    };
    Some(inner.trim_matches('\n').to_string())
}

/// Whether `body` `@mention`s this profile name (case-insensitive). This is
/// the precise check behind the message-row highlight; the store's badge
/// query uses a coarser `LIKE` and may over-count — the row never lies.
pub fn mentions_user(body: &str, name: &str) -> bool {
    parse(body)
        .iter()
        .any(|span| span.kind == SpanKind::Mention && span.text[1..].eq_ignore_ascii_case(name))
}

/// Renders a message body: a mono card for a fenced code block, styled
/// inline text for everything else. Inherits text color/size from the
/// surrounding element for plain runs.
pub fn render_body(body: &str) -> AnyElement {
    if let Some(code) = fenced_code(body) {
        return div()
            .my(px(2.))
            .px(px(10.))
            .py(px(8.))
            .rounded_md()
            .bg(theme::rail())
            .border_1()
            .border_color(theme::border())
            .font_family("monospace")
            .text_size(px(13.))
            .child(code)
            .into_any_element();
    }

    let spans = parse(body);
    let mut text = String::new();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for span in &spans {
        let start = text.len();
        text.push_str(&span.text);
        let style = match span.kind {
            SpanKind::Plain => None,
            SpanKind::Bold => {
                Some(HighlightStyle { font_weight: Some(FontWeight::BOLD), ..Default::default() })
            }
            SpanKind::Italic => {
                Some(HighlightStyle { font_style: Some(FontStyle::Italic), ..Default::default() })
            }
            SpanKind::Strike => Some(HighlightStyle {
                strikethrough: Some(StrikethroughStyle { thickness: px(1.), color: None }),
                ..Default::default()
            }),
            SpanKind::Code => Some(HighlightStyle {
                background_color: Some(theme::wash_strong().into()),
                ..Default::default()
            }),
            SpanKind::Url => Some(HighlightStyle {
                color: Some(theme::info().into()),
                underline: Some(UnderlineStyle { thickness: px(1.), color: None, wavy: false }),
                ..Default::default()
            }),
            SpanKind::Mention => {
                let mut background = theme::primary();
                background.a = 0.3;
                Some(HighlightStyle {
                    color: Some(theme::foreground().into()),
                    font_weight: Some(FontWeight::MEDIUM),
                    background_color: Some(background.into()),
                    ..Default::default()
                })
            }
        };
        if let Some(style) = style {
            highlights.push((start..text.len(), style));
        }
    }

    div().text_size(px(14.)).child(StyledText::new(text).with_highlights(highlights)).into_any_element()
}
