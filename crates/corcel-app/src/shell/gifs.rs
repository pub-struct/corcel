//! The composer's GIF picker: click the GIF button, search GIPHY, click a
//! result and it lands in the channel as a message whose body is the GIF's
//! URL — the existing media-embed pipeline renders it inline for everyone
//! (including old builds, which already embed linked GIFs).
//!
//! Search runs on GIPHY (Tenor's API was closed to new consumers by
//! Google — "not available to this consumer"). The free API key is read
//! from the `giphy.key` file next to `profile.json`, or the
//! `CORCEL_GIPHY_KEY` env var; without one the picker opens to
//! instructions instead of results.

use std::io::Read;

use super::*;

const GIPHY_SEARCH: &str = "https://api.giphy.com/v1/gifs/search";
const GIPHY_TRENDING: &str = "https://api.giphy.com/v1/gifs/trending";
const RESULT_LIMIT: usize = 24;

/// One search hit: the lightweight preview the grid renders and the full
/// GIF URL that gets sent.
#[derive(Clone)]
pub(super) struct GifResult {
    pub preview: String,
    pub full: String,
}

fn giphy_key() -> Option<String> {
    if let Ok(key) = std::env::var("CORCEL_GIPHY_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Some(key);
        }
    }
    let path = crate::profile::config_dir().join("giphy.key");
    let key = std::fs::read_to_string(path).ok()?.trim().to_string();
    (!key.is_empty()).then_some(key)
}

/// Blocking GIPHY call — parked on the blocking pool by the caller, same
/// pattern as the image-embed fetches.
fn fetch_gifs(key: &str, query: &str) -> anyhow::Result<Vec<GifResult>> {
    let endpoint = if query.is_empty() { GIPHY_TRENDING } else { GIPHY_SEARCH };
    let mut request = ureq::get(endpoint)
        .timeout(Duration::from_secs(15))
        .query("api_key", key)
        .query("limit", &RESULT_LIMIT.to_string())
        .query("rating", "pg-13");
    if !query.is_empty() {
        request = request.query("q", query);
    }
    let mut body = String::new();
    request.call()?.into_reader().take(4 * 1024 * 1024).read_to_string(&mut body)?;
    let json: serde_json::Value = serde_json::from_str(&body)?;
    let results = json["data"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .filter_map(|hit| {
                    let images = &hit["images"];
                    let full = images["original"]["url"].as_str()?.to_string();
                    let preview = images["fixed_height_small"]["url"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| full.clone());
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
        if giphy_key().is_none() {
            self.gif_results.clear();
            cx.notify();
            return;
        }
        // Open on GIPHY's trending feed so the panel is never blank while
        // the user thinks of a search.
        if self.gif_results.is_empty() {
            self.search_gifs(String::new(), cx);
        }
        cx.notify();
    }

    /// Search-as-you-type: bump the generation and schedule a search a
    /// beat later; only the newest keystroke's timer actually fires, and
    /// it reads the input fresh so it always searches what's on screen.
    pub(super) fn gif_query_changed(&mut self, cx: &mut Context<Self>) {
        self.gif_search_generation += 1;
        let generation = self.gif_search_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(350)).await;
            let _ = this.update(cx, |shell, cx| {
                if shell.gif_search_generation != generation || !shell.gif_picker_open {
                    return;
                }
                let query = shell.gif_input.read(cx).content.trim().to_string();
                if query == shell.gif_last_query {
                    return;
                }
                shell.search_gifs(query, cx);
            });
        })
        .detach();
    }

    pub(super) fn search_gifs(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(key) = giphy_key() else { return };
        self.gif_last_query = query.clone();
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
        let has_key = giphy_key().is_some();

        let body: AnyElement = if !has_key {
            div()
                .p(px(14.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .text_size(px(12.5))
                .text_color(theme::muted_foreground())
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme::foreground()).child(
                    "GIF search needs a (free) GIPHY API key",
                ))
                .child("1. Create an app at developers.giphy.com (free, instant key).")
                .child(format!(
                    "2. Save the key as {} — one line, just the key.",
                    crate::profile::config_dir().join("giphy.key").display()
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

        // Floats over the chat, anchored to the composer's right corner —
        // in-flow it occupied a full-width row whose empty left half read
        // as a giant dead block.
        div().absolute().bottom(px(76.)).right(px(16.)).child(
            div()
                .id("gif-picker")
                // Swallow every mouse event over the panel — without this,
                // wheel scrolling the GIF grid also scrolled the chat
                // behind the overlay.
                .occlude()
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
                                // Enter skips the debounce.
                                shell.gif_search_generation += 1;
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
