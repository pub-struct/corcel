//! Display-side rendering of the markdown subset chat supports. Inline:
//! `**bold**`, `*italic*`, `~~strike~~`, `` `code` ``, bare URLs,
//! `[label](url)` links, and `@mentions`. Block-level: fenced ``` code
//! blocks, `> ` quotes, and `#`/`##`/`###` headings. Hand-rolled on purpose
//! — the subset is small enough that owning the edge cases (an unclosed
//! marker renders literally, nothing nests) beats pulling in a markdown
//! crate whose flavor we'd fight.
//!
//! Messages travel and persist as the raw text the author typed; parsing
//! happens per render. The composer knows nothing about any of this.
//!
//! Links do double duty: every URL span is clickable (opens in the system
//! browser), and [`media_links`] surfaces the ones that point at an image or
//! video file so the chat panel can render inline embeds under the message.

use std::ops::Range;

use gpui::{
    AnyElement, ElementId, FontStyle, FontWeight, HighlightStyle, InteractiveText, SharedString, StrikethroughStyle,
    StyledText, UnderlineStyle, div, prelude::*, px,
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

/// One run of display text (markers already stripped) and its style. `link`
/// is the click target for URL spans — the span text itself for a bare URL,
/// the parenthesized target for a `[label](url)` link.
#[derive(Debug, Clone)]
struct Span {
    text: String,
    kind: SpanKind,
    link: Option<String>,
}

/// One block-level run of a message body, produced by [`blocks`].
enum Block {
    /// Regular text, inline-styled by [`parse`].
    Paragraph(String),
    /// `# ` / `## ` / `### ` heading and its level (1–3).
    Heading(usize, String),
    /// Consecutive `> ` lines, markers stripped, joined back with newlines.
    Quote(String),
    /// A fenced ``` block's inner text, opening-line language tag dropped
    /// (Discord-style). No inline parsing happens inside.
    Code(String),
}

/// Splits a message body into block-level runs, line by line. A fence left
/// unclosed swallows the rest of the message as code — the author clearly
/// meant a code block, and rendering the fence chars literally helps nobody.
fn blocks(body: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut quote: Vec<&str> = Vec::new();
    let mut code: Option<Vec<&str>> = None;

    fn flush(lines: &mut Vec<&str>, blocks: &mut Vec<Block>, build: fn(String) -> Block) {
        if !lines.is_empty() {
            blocks.push(build(lines.join("\n")));
            lines.clear();
        }
    }

    for line in body.lines() {
        if let Some(code_lines) = &mut code {
            if line.trim() == "```" {
                blocks.push(Block::Code(code_lines.join("\n")));
                code = None;
            } else {
                code_lines.push(line);
            }
            continue;
        }
        if let Some(rest) = line.trim_start().strip_prefix("```") {
            flush(&mut quote, &mut blocks, Block::Quote);
            flush(&mut paragraph, &mut blocks, Block::Paragraph);
            // ```code``` on one line closes immediately; otherwise the rest
            // of the opening line is a language tag, dropped.
            match rest.strip_suffix("```").filter(|inner| !inner.is_empty()) {
                Some(inner) => blocks.push(Block::Code(inner.to_string())),
                None => code = Some(Vec::new()),
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("> ").or_else(|| line.strip_prefix('>')) {
            flush(&mut paragraph, &mut blocks, Block::Paragraph);
            quote.push(rest);
            continue;
        }
        flush(&mut quote, &mut blocks, Block::Quote);
        if let Some((level, text)) = heading(line) {
            flush(&mut paragraph, &mut blocks, Block::Paragraph);
            blocks.push(Block::Heading(level, text.to_string()));
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut paragraph, &mut blocks, Block::Paragraph);
            continue;
        }
        paragraph.push(line);
    }
    if let Some(code_lines) = code {
        blocks.push(Block::Code(code_lines.join("\n")));
    }
    flush(&mut quote, &mut blocks, Block::Quote);
    flush(&mut paragraph, &mut blocks, Block::Paragraph);
    blocks
}

/// `# `/`## `/`### ` at the start of a line, with non-empty text after it.
fn heading(line: &str) -> Option<(usize, &str)> {
    for (marker, level) in [("### ", 3), ("## ", 2), ("# ", 1)] {
        if let Some(text) = line.strip_prefix(marker) {
            if !text.trim().is_empty() {
                return Some((level, text));
            }
        }
    }
    None
}

/// Parses one block's text into display spans. Single left-to-right pass; a
/// marker only takes effect if its closing pair exists with something
/// between them, otherwise the character is literal text. Styles don't nest.
fn parse(body: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut prev: Option<char> = None;
    let mut i = 0;

    let flush = |plain: &mut String, spans: &mut Vec<Span>| {
        if !plain.is_empty() {
            spans.push(Span { text: std::mem::take(plain), kind: SpanKind::Plain, link: None });
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
                    return Some((
                        marker.len() * 2 + end,
                        Span { text: inner.to_string(), kind, link: None },
                    ));
                }
            }
        }
    }

    // `[label](https://...)` — a markdown link. Only http(s) targets count;
    // anything else renders literally rather than becoming a dead link.
    if rest.starts_with('[') {
        if let Some(close) = rest.find("](") {
            let label = &rest[1..close];
            if !label.is_empty() && !label.contains('\n') && !label.contains('[') {
                let after = &rest[close + 2..];
                if let Some(end) = after.find(')') {
                    let url = &after[..end];
                    if is_http_url(url) && !url.contains(char::is_whitespace) {
                        return Some((
                            close + 2 + end + 1,
                            Span { text: label.to_string(), kind: SpanKind::Url, link: Some(url.to_string()) },
                        ));
                    }
                }
            }
        }
    }

    if rest.starts_with("http://") || rest.starts_with("https://") {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let url = &rest[..end];
        // A bare scheme isn't a link worth styling.
        if url.len() > "https://".len() {
            return Some((
                end,
                Span { text: url.to_string(), kind: SpanKind::Url, link: Some(url.to_string()) },
            ));
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
            return Some((text.len(), Span { text: text.to_string(), kind: SpanKind::Mention, link: None }));
        }
    }

    None
}

fn is_http_url(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://")) && url.len() > "https://".len()
}

/// What kind of inline embed a media URL should get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
}

/// A URL in a message body that points at a media file (see [`media_links`]).
pub struct MediaLink {
    pub url: String,
    pub kind: MediaKind,
}

/// Every URL in `body` that looks like an image or video file, in order of
/// appearance, deduplicated. Code blocks don't count — a URL inside a code
/// sample wasn't shared to be looked at.
pub fn media_links(body: &str) -> Vec<MediaLink> {
    let mut links: Vec<MediaLink> = Vec::new();
    for block in blocks(body) {
        let text = match block {
            Block::Code(_) => continue,
            Block::Paragraph(text) | Block::Quote(text) | Block::Heading(_, text) => text,
        };
        for span in parse(&text) {
            let Some(url) = span.link else { continue };
            let Some(kind) = classify_media(&url) else { continue };
            if !links.iter().any(|link| link.url == url) {
                links.push(MediaLink { url, kind });
            }
        }
    }
    links
}

/// Classifies a URL by the file extension of its path (query string and
/// fragment ignored). `None` for anything that isn't an obvious media file.
fn classify_media(url: &str) -> Option<MediaKind> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next()?;
    let (_, ext) = name.rsplit_once('.')?;
    match ext.to_ascii_lowercase().as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => Some(MediaKind::Image),
        "mp4" | "webm" | "mov" | "mkv" | "m4v" => Some(MediaKind::Video),
        _ => None,
    }
}

/// Whether `body` `@mention`s this profile name (case-insensitive). This is
/// the precise check behind the message-row highlight; the store's badge
/// query uses a coarser `LIKE` and may over-count — the row never lies.
/// Mentions inside code blocks don't count.
pub fn mentions_user(body: &str, name: &str) -> bool {
    blocks(body).into_iter().any(|block| {
        let text = match block {
            Block::Code(_) => return false,
            Block::Paragraph(text) | Block::Quote(text) | Block::Heading(_, text) => text,
        };
        parse(&text)
            .iter()
            .any(|span| span.kind == SpanKind::Mention && span.text[1..].eq_ignore_ascii_case(name))
    })
}

/// Renders a message body as its block-level runs stacked vertically. `seed`
/// makes the clickable-link element ids unique per message — pass something
/// stable for the message (its id), so GPUI's element state tracks across
/// re-renders.
///
/// `hidden_links` is the caller telling us which URLs render as media
/// embeds under this message: a bare-pasted URL in that list is dropped
/// from the text (the media speaks for itself), and a block left with
/// nothing but whitespace disappears entirely — so a message that *is* just
/// a GIF link shows just the GIF. Only bare URLs are dropped; a
/// `[label](url)` link keeps its label, which is content the author wrote.
pub fn render_body(seed: u64, body: &str, hidden_links: &[String]) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .text_size(px(14.))
        .children(
            blocks(body)
                .into_iter()
                .enumerate()
                .filter_map(|(ix, block)| render_block(seed, ix, block, hidden_links)),
        )
        .into_any_element()
}

fn render_block(seed: u64, ix: usize, block: Block, hidden_links: &[String]) -> Option<AnyElement> {
    match block {
        Block::Code(code) => Some(
            div()
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
                .into_any_element(),
        ),
        Block::Quote(text) => inline_text(seed, ix, &text, hidden_links).map(|inline| {
            div()
                .pl(px(10.))
                .border_l_2()
                .border_color(theme::wash_strong())
                .text_color(theme::muted_foreground())
                .child(inline)
                .into_any_element()
        }),
        Block::Heading(level, text) => inline_text(seed, ix, &text, hidden_links).map(|inline| {
            let size = match level {
                1 => 20.,
                2 => 17.,
                _ => 15.,
            };
            div()
                .mt(px(4.))
                .text_size(px(size))
                .font_weight(FontWeight::BOLD)
                .child(inline)
                .into_any_element()
        }),
        Block::Paragraph(text) => inline_text(seed, ix, &text, hidden_links)
            .map(|inline| div().child(inline).into_any_element()),
    }
}

/// One block's inline-styled text. Plain styled text when there's nothing to
/// click; wrapped in an [`InteractiveText`] when the block contains links,
/// so clicking a link's range opens it in the system browser. `None` when
/// dropping the block's embedded-media URLs (see [`render_body`]) leaves
/// nothing visible.
fn inline_text(seed: u64, block_ix: usize, source: &str, hidden_links: &[String]) -> Option<AnyElement> {
    let spans = visible_spans(source, hidden_links);
    if spans.iter().all(|span| span.text.trim().is_empty()) {
        return None;
    }
    let mut text = String::new();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut link_ranges: Vec<Range<usize>> = Vec::new();
    let mut link_urls: Vec<String> = Vec::new();
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
        if let Some(url) = &span.link {
            link_ranges.push(start..text.len());
            link_urls.push(url.clone());
        }
    }

    let styled = StyledText::new(text).with_highlights(highlights);
    if link_urls.is_empty() {
        return Some(styled.into_any_element());
    }
    let id: ElementId = SharedString::from(format!("richtext-{seed:x}-{block_ix}")).into();
    Some(
        InteractiveText::new(id, styled)
            .on_click(link_ranges, move |range_ix, _window, cx| {
                if let Some(url) = link_urls.get(range_ix) {
                    cx.open_url(url);
                }
            })
            .into_any_element(),
    )
}

/// A block's spans minus bare URLs that render as embeds under the message.
/// "Bare" means the visible text *is* the URL — the only case where hiding
/// it loses nothing the author typed.
fn visible_spans(source: &str, hidden_links: &[String]) -> Vec<Span> {
    parse(source)
        .into_iter()
        .filter(|span| {
            !span
                .link
                .as_deref()
                .is_some_and(|url| span.text == url && hidden_links.iter().any(|hidden| hidden == url))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_split_paragraphs_code_quotes_headings() {
        let body = "# Title\nplain text\n> quoted\n> more\n```rust\nlet x = 1;\n```\ntail";
        let blocks = blocks(body);
        assert_eq!(blocks.len(), 5);
        assert!(matches!(&blocks[0], Block::Heading(1, text) if text == "Title"));
        assert!(matches!(&blocks[1], Block::Paragraph(text) if text == "plain text"));
        assert!(matches!(&blocks[2], Block::Quote(text) if text == "quoted\nmore"));
        assert!(matches!(&blocks[3], Block::Code(text) if text == "let x = 1;"));
        assert!(matches!(&blocks[4], Block::Paragraph(text) if text == "tail"));
    }

    #[test]
    fn unclosed_fence_swallows_the_rest_as_code() {
        let blocks = blocks("```\nno closing fence");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], Block::Code(text) if text == "no closing fence"));
    }

    #[test]
    fn markdown_link_parses_label_and_target() {
        let spans = parse("see [the docs](https://example.com/x) ok");
        let link = spans.iter().find(|span| span.kind == SpanKind::Url).expect("a link span");
        assert_eq!(link.text, "the docs");
        assert_eq!(link.link.as_deref(), Some("https://example.com/x"));
    }

    #[test]
    fn media_links_classify_and_skip_code() {
        let body = "look https://a.io/cat.png and [clip](https://a.io/clip.mp4?t=1)\n```\nhttps://a.io/ignored.png\n```";
        let links = media_links(body);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].url, "https://a.io/cat.png");
        assert_eq!(links[0].kind, MediaKind::Image);
        assert_eq!(links[1].url, "https://a.io/clip.mp4?t=1");
        assert_eq!(links[1].kind, MediaKind::Video);
    }

    #[test]
    fn embedded_bare_urls_hide_but_labeled_links_stay() {
        let hidden = vec!["https://a.io/cat.gif".to_string()];
        // The bare paste disappears; surrounding text survives.
        let spans = visible_spans("look https://a.io/cat.gif", &hidden);
        assert_eq!(spans.iter().map(|s| s.text.as_str()).collect::<String>(), "look ");
        // A labeled link keeps its label even when the URL is embedded.
        let spans = visible_spans("[cat](https://a.io/cat.gif)", &hidden);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "cat");
        // URLs without an embed are untouched.
        let spans = visible_spans("https://a.io/dog.gif", &[]);
        assert_eq!(spans[0].text, "https://a.io/dog.gif");
    }

    #[test]
    fn mentions_ignore_code_and_match_case_insensitively() {
        assert!(mentions_user("hey @Sam look", "sam"));
        assert!(!mentions_user("mail me at sam@example.com", "sam"));
        assert!(!mentions_user("```\n@sam\n```", "sam"));
    }
}
