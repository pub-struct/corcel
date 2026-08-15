//! Entry point: initializes GStreamer and GPUI, opens the window, and hands
//! everything to [`shell::Shell`] — the single entity that owns the UI.

mod assets;
mod chat;
mod invite;
mod profile;
mod richtext;
mod runtime;
mod session;
mod shell;
mod store;
mod text_input;
mod theme;

use gpui::{App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};

use shell::Shell;

fn main() {
    profile::migrate_legacy_config_dir();
    corcel_media::init().expect("failed to initialize GStreamer");

    Application::new().with_assets(assets::Assets).run(|cx: &mut App| {
        text_input::init(cx);

        // Open at 90% of the primary display, centered — the window stays
        // freely resizable (GPUI's default) down to the min size below.
        let display_size = cx
            .primary_display()
            .map(|display| display.bounds().size)
            .unwrap_or_else(|| size(px(1920.0), px(1080.0)));
        let bounds = Bounds::centered(None, size(display_size.width * 0.9, display_size.height * 0.9), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions { title: Some("corcel".into()), ..Default::default() }),
                    // The app-shell's server-rail + channel-sidebar alone take
                    // ~312px of fixed-width chrome, so keep the window from
                    // shrinking small enough to crush the video panel. The
                    // window is resizable (GPUI's default) up to whatever size
                    // the user wants above this floor.
                    window_min_size: Some(size(px(720.0), px(480.0))),
                    ..Default::default()
                },
                |_window, cx| cx.new(Shell::new),
            )
            .unwrap();

        window
            .update(cx, |shell, window, cx| {
                shell.focus_first_run(window, cx);
                cx.activate(true);
            })
            .unwrap();
    });
}
