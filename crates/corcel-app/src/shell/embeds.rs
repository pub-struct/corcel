//! Inline media embeds under chat messages: images fetched and decoded into
//! cards (animated GIFs included), and click-to-play video via
//! [`corcel_media::UrlPlayer`]. Which URLs get an embed is decided by
//! [`richtext::media_links`]; this module owns the caches, the fetch/decode
//! path, the video frame pump, and the cards' rendering.

use std::io::Read;

use gpui::ElementId;
use image::AnimationDecoder;

use super::*;

/// Refuse to buffer more than this many bytes of a linked image — a link is
/// not consent to eat unbounded memory.
const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;

/// Longest side a decoded still image is kept at. The card renders ~420px
/// wide, so anything sharper than ~3x that is wasted GPU texture memory —
/// and it's texture memory that never comes back, since the image cache
/// lives for the app's lifetime. (Animated GIFs skip the downscale: resizing
/// every frame costs more than it saves, and chat GIFs are small.)
const MAX_DECODE_DIMENSION: u32 = 1200;

/// At most this many embeds render per message; further media links stay
/// plain (clickable) links.
const MAX_EMBEDS_PER_MESSAGE: usize = 3;

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

const EMBED_MAX_WIDTH: f32 = 420.;
const EMBED_MAX_HEIGHT: f32 = 320.;

/// One image URL's place in the embed cache. `Failed` is remembered so a
/// broken link doesn't refetch on every render; a fresh attempt happens
/// naturally next launch (the cache is in-memory only).
pub(super) enum ImageEmbed {
    Loading,
    Ready(Arc<RenderImage>),
    Failed,
}

/// The video pane of a playing embed, its own entity for the same reason as
/// [`VideoSurface`]: the frame pump notifies only this view, not the whole
/// shell, once per frame.
pub(super) struct EmbedFrame {
    frame: Option<Arc<RenderImage>>,
}

impl Render for EmbedFrame {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match self.frame.clone() {
            Some(frame) => div()
                .size_full()
                .child(img(ImageSource::Render(frame)).size_full().object_fit(ObjectFit::Contain))
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
                .text_color(theme::muted_foreground())
                .child("Loading video…")
                .into_any_element(),
        }
    }
}

/// A video embed the user clicked play on. Dropping this drops the player,
/// which tears the GStreamer pipeline down (see [`Shell::stop_video_embeds`]
/// for why that must go through a method, not a bare `remove`).
pub(super) struct VideoEmbed {
    player: corcel_media::UrlPlayer,
    surface: Entity<EmbedFrame>,
    paused: bool,
}

impl Shell {
    /// Builds the embed elements for one message body — called once per
    /// message before the row loop (it needs `&mut self`: seeing an image
    /// URL for the first time is what kicks off its fetch). Also returns
    /// the URLs that got an embed, so [`richtext::render_body`] can drop
    /// them from the message text — a pasted GIF link shows just the GIF.
    pub(super) fn render_message_embeds(
        &mut self,
        body: &str,
        cx: &mut Context<Self>,
    ) -> (Vec<AnyElement>, Vec<String>) {
        let links = richtext::media_links(body);
        let mut elements = Vec::new();
        let mut embedded_urls = Vec::new();
        for link in links.into_iter().take(MAX_EMBEDS_PER_MESSAGE) {
            match link.kind {
                richtext::MediaKind::Image => {
                    self.ensure_image_embed(&link.url, cx);
                    match self.image_embeds.get(&link.url) {
                        Some(ImageEmbed::Ready(image)) => {
                            embedded_urls.push(link.url.clone());
                            elements.push(image_card(link.url, image.clone(), cx));
                        }
                        Some(ImageEmbed::Loading) => {
                            embedded_urls.push(link.url);
                            elements.push(loading_card());
                        }
                        // Failed: no embed and the URL stays visible in the
                        // text — a clickable link is the honest fallback.
                        _ => {}
                    }
                }
                richtext::MediaKind::Video => {
                    embedded_urls.push(link.url.clone());
                    elements.push(self.video_card(link.url, cx));
                }
            }
        }
        (elements, embedded_urls)
    }

    /// Starts fetching an image URL the first time it's seen. The cache
    /// remembers successes *and* failures for the app's lifetime; images are
    /// downscaled at decode (see [`MAX_DECODE_DIMENSION`]) to keep that
    /// affordable.
    fn ensure_image_embed(&mut self, url: &str, cx: &mut Context<Self>) {
        if self.image_embeds.contains_key(url) {
            return;
        }
        self.image_embeds.insert(url.to_string(), ImageEmbed::Loading);
        let fetch_url = url.to_string();
        // ureq is blocking by design — park the fetch on tokio's blocking
        // pool rather than stalling a worker thread.
        let rx = runtime::spawn_and_send(async move {
            tokio::task::spawn_blocking(move || fetch_and_decode(&fetch_url)).await
        });
        let key = url.to_string();
        cx.spawn(async move |this, cx| {
            let embed = match rx.await {
                Ok(Ok(Ok(image))) => ImageEmbed::Ready(image),
                Ok(Ok(Err(err))) => {
                    eprintln!("corcel: image embed failed for {key}: {err:#}");
                    ImageEmbed::Failed
                }
                _ => ImageEmbed::Failed,
            };
            let _ = this.update(cx, |shell, cx| {
                shell.image_embeds.insert(key, embed);
                cx.notify();
            });
        })
        .detach();
    }

    /// Play/pause for a video embed. The first click on a URL creates its
    /// player (click-to-play — no autoplay, these can have sound) and starts
    /// the frame pump; later clicks toggle pause. A stream that ended
    /// reports itself paused, so the same click replays it from the start
    /// (see [`corcel_media::UrlPlayer::set_paused`]).
    pub(super) fn toggle_video_embed(&mut self, url: &str, cx: &mut Context<Self>) {
        if let Some(embed) = self.video_embeds.get_mut(url) {
            embed.paused = !embed.paused;
            embed.player.set_paused(embed.paused);
            cx.notify();
            return;
        }

        let (player, mut events) = match corcel_media::UrlPlayer::new(url) {
            Ok(pair) => pair,
            Err(err) => {
                self.error = Some(format!("couldn't play that video: {err:#}"));
                cx.notify();
                return;
            }
        };
        let surface = cx.new(|_| EmbedFrame { frame: None });
        self.video_embeds.insert(url.to_string(), VideoEmbed { player, surface: surface.clone(), paused: false });

        // Same pump-and-drop pattern as the call stage: each new frame's
        // predecessor has its GPU texture dropped explicitly, or every frame
        // of playback leaks one. Ends when the player is dropped (its
        // channel closes) or the surface entity is released.
        let key = url.to_string();
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                match event {
                    corcel_media::PlayerEvent::Frame(frame) => {
                        let Some(buffer) = image::RgbaImage::from_raw(frame.width, frame.height, frame.data)
                        else {
                            continue;
                        };
                        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
                        let updated = surface.update(cx, |surface, cx| {
                            if let Some(old) = surface.frame.replace(image) {
                                cx.drop_image(old, None);
                            }
                            cx.notify();
                        });
                        if updated.is_err() {
                            return;
                        }
                    }
                    corcel_media::PlayerEvent::Ended => {
                        let updated = this.update(cx, |shell, cx| {
                            if let Some(embed) = shell.video_embeds.get_mut(&key) {
                                embed.paused = true;
                                cx.notify();
                            }
                        });
                        if updated.is_err() {
                            return;
                        }
                    }
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Tears down every playing video embed — called on any navigation away
    /// from the channel. Goes through the surfaces to drop their last
    /// frames' GPU textures; just clearing the map would stop the pipelines
    /// (via `Drop`) but leak one full-resolution texture per video.
    pub(super) fn stop_video_embeds(&mut self, cx: &mut Context<Self>) {
        for (_, embed) in self.video_embeds.drain() {
            embed.surface.update(cx, |surface, cx| {
                if let Some(old) = surface.frame.take() {
                    cx.drop_image(old, None);
                }
            });
        }
    }

    /// One video embed's card: the live surface with a play overlay while
    /// paused, or a dark click-to-play placeholder before the first click.
    fn video_card(&self, url: String, cx: &mut Context<Self>) -> AnyElement {
        let card = div()
            .w(px(EMBED_MAX_WIDTH))
            .h(px(EMBED_MAX_WIDTH * 9. / 16.))
            .mt(px(4.))
            .rounded_lg()
            .overflow_hidden()
            .relative()
            .bg(gpui::rgb(0x000000))
            .border_1()
            .border_color(theme::border())
            .cursor_pointer();

        match self.video_embeds.get(&url) {
            Some(embed) => {
                let paused = embed.paused;
                let surface = embed.surface.clone();
                card.on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |shell, _, _window, cx| shell.toggle_video_embed(&url, cx)),
                )
                .child(div().size_full().rounded_lg().overflow_hidden().child(surface))
                .when(paused, |card| card.child(play_overlay(icons::PLAY)))
                .into_any_element()
            }
            None => {
                let filename = url.rsplit('/').next().unwrap_or(&url).to_string();
                card.on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |shell, _, _window, cx| shell.toggle_video_embed(&url, cx)),
                )
                .child(
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(10.))
                        .child(
                            div()
                                .size(px(52.))
                                .rounded_full()
                                .bg(theme::scrim())
                                .border_1()
                                .border_color(theme::wash_strong())
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(theme::icon(icons::PLAY, px(22.))),
                        )
                        .child(
                            div()
                                .max_w(px(EMBED_MAX_WIDTH - 40.))
                                .text_size(px(12.))
                                .text_color(theme::muted_foreground())
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(filename),
                        ),
                )
                .into_any_element()
            }
        }
    }
}

/// A finished image embed: sized to the decoded image scaled into the embed
/// box, clicking opens the original URL in the browser (the full-resolution
/// source, since decode may have downscaled).
fn image_card(url: String, image: Arc<RenderImage>, cx: &mut Context<Shell>) -> AnyElement {
    let size = image.size(0);
    let (width, height) = (size.width.0.max(1) as f32, size.height.0.max(1) as f32);
    let scale = (EMBED_MAX_WIDTH / width).min(EMBED_MAX_HEIGHT / height).min(1.);
    // The img element only animates (advances GIF frames + requests the
    // next redraw) when it has an element id to keep frame state under —
    // an anonymous img renders frame 0 forever.
    let id: ElementId = SharedString::from(format!("embed-img-{url}")).into();
    div()
        .w(px(width * scale))
        .h(px(height * scale))
        .mt(px(4.))
        .rounded_lg()
        .overflow_hidden()
        .border_1()
        .border_color(theme::border())
        .cursor_pointer()
        .on_mouse_up(MouseButton::Left, cx.listener(move |_shell, _, _window, cx| cx.open_url(&url)))
        .child(img(ImageSource::Render(image)).size_full().rounded_lg().object_fit(ObjectFit::Contain).id(id))
        .into_any_element()
}

/// The skeleton box an image embed shows while its fetch is in flight —
/// fixed-size so the scrollback doesn't jump when the image lands... much.
fn loading_card() -> AnyElement {
    div()
        .w(px(EMBED_MAX_WIDTH))
        .h(px(180.))
        .mt(px(4.))
        .rounded_lg()
        .bg(theme::card())
        .border_1()
        .border_color(theme::border())
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.))
        .text_color(theme::faint_foreground())
        .child("Loading image…")
        .into_any_element()
}

/// The centered ▶/⏸ badge overlaid on a paused video surface.
fn play_overlay(icon_path: &'static str) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .size(px(52.))
                .rounded_full()
                .bg(theme::scrim())
                .border_1()
                .border_color(theme::wash_strong())
                .flex()
                .items_center()
                .justify_center()
                .child(theme::icon(icon_path, px(22.))),
        )
        .into_any_element()
}

/// Downloads and decodes a linked image on the blocking pool: size-capped
/// fetch, GIFs decoded frame-by-frame so they animate, everything swapped to
/// the BGRA byte order `RenderImage` expects (matching what gpui's own
/// asset-image decode path does).
fn fetch_and_decode(url: &str) -> anyhow::Result<Arc<RenderImage>> {
    let response = ureq::get(url).timeout(FETCH_TIMEOUT).call()?;
    let mut bytes = Vec::new();
    response.into_reader().take(MAX_IMAGE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= MAX_IMAGE_BYTES, "image is larger than {}MB", MAX_IMAGE_BYTES / (1024 * 1024));

    let format = image::guess_format(&bytes)?;
    let frames: Vec<image::Frame> = if format == image::ImageFormat::Gif {
        let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&bytes[..]))?;
        let mut frames = decoder.into_frames().collect_frames()?;
        for frame in &mut frames {
            swap_red_blue(frame.buffer_mut());
        }
        frames
    } else {
        let mut buffer = image::load_from_memory_with_format(&bytes, format)?.into_rgba8();
        let (width, height) = buffer.dimensions();
        let longest = width.max(height);
        if longest > MAX_DECODE_DIMENSION {
            let scale = MAX_DECODE_DIMENSION as f32 / longest as f32;
            buffer = image::imageops::thumbnail(
                &buffer,
                ((width as f32 * scale) as u32).max(1),
                ((height as f32 * scale) as u32).max(1),
            );
        }
        swap_red_blue(&mut buffer);
        vec![image::Frame::new(buffer)]
    };
    anyhow::ensure!(!frames.is_empty(), "image decoded to zero frames");
    Ok(Arc::new(RenderImage::new(frames)))
}

fn swap_red_blue(buffer: &mut image::RgbaImage) {
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}
