//! The composer's GIF picker: click the GIF button, search Tenor, click a
//! result and it lands in the channel as a message whose body is the GIF's
//! URL — the existing media-embed pipeline renders it inline for everyone
//! (including old builds, which already embed linked GIFs).
//!
//! Search needs a Tenor v2 API key (free, via Google Cloud). It's read
//! from the `tenor.key` file next to `profile.json`, or the
//! `CORCEL_TENOR_KEY` env var; without one the picker opens to
//! instructions instead of results.

use std::io::Read;

use super::*;

const TENOR_SEARCH: &str = "https://tenor.googleapis.com/v2/search";
const TENOR_FEATURED: &str = "https://tenor.googleapis.com/v2/featured";
const RESULT_LIMIT: usize = 24;

/// One search hit: the lightweight preview the grid renders and the full
/// GIF URL that gets sent.
#[derive(Clone)]
pub(super) struct GifResult {
    pub preview: String,
    pub full: String,
}

fn tenor_key() -> Option<String> {
    if let Ok(key) = std::env::var("CORCEL_TENOR_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Some(key);
        }
    }
    let path = crate::profile::config_dir().join("tenor.key");
    let key = std::fs::read_to_string(path).ok()?.trim().to_string();
    (!key.is_empty()).then_some(key)
}

/// Blocking Tenor call — parked on the blocking pool by the caller, same
/// pattern as the image-embed fetches.
fn fetch_gifs(key: &str, query: &str) -> anyhow::Result<Vec<GifResult>> {
    let endpoint = if query.is_empty() { TENOR_FEATURED } else { TENOR_SEARCH };
    let mut request = ureq::get(endpoint)
        .timeout(Duration::from_secs(15))
        .query("key", key)
        .query("limit", &RESULT_LIMIT.to_string())
        .query("media_filter", "tinygif,gif");
    if !query.is_empty() {
        request = request.query("q", query);
    }
    let mut body = String::new();
    request.call()?.into_reader().take(4 * 1024 * 1024).read_to_string(&mut body)?;
    let json: serde_json::Value = serde_json::from_str(&body)?;
    let results = json["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .filter_map(|hit| {
                    let formats = &hit["media_formats"];
                    let full = formats["gif"]["url"].as_str()?.to_string();
                    let preview =
                        formats["tinygif"]["url"].as_str().map(str::to_string).unwrap_or_else(|| full.clone());
                    Some(GifResult { preview, full })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(results)
}

impl Shell {
    pub(super) fn toggle_gif_picker(&mut self, cx: &mut Context<Self>) {
        if self.gif_picker_open {
            self.gif_picker_open = false;
            cx.notify();
            return;
        }
        self.gif_picker_open = true;
        self.gif_error = None;
        if tenor_key().is_none() {
            self.gif_results.clear();
            cx.notify();
            return;
        }
        // Open on Tenor's featured feed so the panel is never blank while
        // the user thinks of a search.
        if self.gif_results.is_empty() {
            self.search_gifs(String::new(), cx);
        }
        cx.notify();
    }

    pub(super) fn search_gifs(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(key) = tenor_key() else { return };
        self.gif_loading = true;
        self.gif_error = None;
        cx.notify();
        let rx = runtime::spawn_and_send(async move {
            tokio::task::spawn_blocking(move || fetch_gifs(&key, &query)).await
        });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await;
            let _ = this.update(cx, |shell, cx| {
                shell.gif_loading = false;
                match outcome {
                    Ok(Ok(Ok(results))) => shell.gif_results = results,
                    Ok(Ok(Err(err))) => shell.gif_error = Some(format!("{err:#}")),
                    _ => shell.gif_error = Some("search task was dropped".to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn send_gif(&mut self, url: String, cx: &mut Context<Self>) {
        self.gif_picker_open = false;
        self.send_message_body(url, cx);
    }

    /// The panel floating above the composer: a query field and the result
    /// grid (previews go through the same image cache the embeds use).
    pub(super) fn render_gif_picker(&mut self, cx: &mut Context<Self>) -> Div {
        let has_key = tenor_key().is_some();

        let body: AnyElement = if !has_key {
            div()
                .p(px(14.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .text_size(px(12.5))
                .text_color(theme::muted_foreground())
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme::foreground()).child(
                    "GIF search needs a (free) Tenor API key",
                ))
                .child("1. Create a key at developers.google.com/tenor (Google Cloud, free tier).")
                .child(format!(
                    "2. Save it as {} — one line, just the key.",
                    crate::profile::config_dir().join("tenor.key").display()
                ))
                .child("3. Reopen this picker.")
                .into_any_element()
        } else if self.gif_loading {
            div()
                .h(px(120.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.5))
                .text_color(theme::muted_foreground())
                .child("Searching…")
                .into_any_element()
        } else if let Some(error) = &self.gif_error {
            div()
                .p(px(14.))
                .text_size(px(12.))
                .text_color(theme::destructive_foreground())
                .child(format!("GIF search failed: {error}"))
                .into_any_element()
        } else {
            // Previews ride the embed image cache: ensure_image_embed
            // kicks off fetches, and rows render as they arrive.
            let urls: Vec<String> = self.gif_results.iter().map(|gif| gif.preview.clone()).collect();
            for url in &urls {
                self.ensure_image_embed(url, cx);
            }
            let tiles: Vec<_> = self
                .gif_results
                .iter()
                .enumerate()
                .map(|(index, gif)| {
                    let full = gif.full.clone();
                    let preview: AnyElement = match self.image_embeds.get(&gif.preview) {
                        Some(ImageEmbed::Ready(image)) => img(ImageSource::Render(image.clone()))
                            .size_full()
                            .rounded_md()
                            .object_fit(ObjectFit::Cover)
                            .into_any_element(),
                        _ => div().size_full().rounded_md().bg(theme::wash()).into_any_element(),
                    };
                    div()
                        .id(SharedString::from(format!("gif-{index}")))
                        .w(px(118.))
                        .h(px(88.))
                        .rounded_md()
                        .overflow_hidden()
                        .cursor_pointer()
                        .hover(|style| style.opacity(0.85))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |shell, _, _window, cx| shell.send_gif(full.clone(), cx)),
                        )
                        .child(preview)
                })
                .collect();
            div()
                .id("gif-grid")
                .h(px(268.))
                .overflow_y_scroll()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .p(px(8.))
                .children(tiles)
                .into_any_element()
        };

        div().mx(px(16.)).flex().child(
            div()
                .id("gif-picker")
                .flex()
                .flex_col()
                .w(px(400.))
                .mb(px(4.))
                .rounded_lg()
                .bg(theme::raised_fill())
                .border_1()
                .border_color(theme::glass_edge())
                .shadow_md()
                .on_mouse_down_out(cx.listener(|shell, _, _window, cx| {
                    if shell.gif_picker_open {
                        shell.gif_picker_open = false;
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .p(px(8.))
                        .border_b_1()
                        .border_color(theme::border())
                        .on_key_down(cx.listener(|shell, event: &KeyDownEvent, _window, cx| {
                            if event.keystroke.key == "enter" {
                                let query = shell.gif_input.read(cx).content.trim().to_string();
                                shell.search_gifs(query, cx);
                            }
                        }))
                        .child(self.gif_input.clone()),
                )
                .child(body)
                .with_animation(
                    "gif-picker-in",
                    Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint()),
                    |picker, delta| picker.opacity(delta).mt(px(6. * (1. - delta))),
                ),
        )
    }
}
