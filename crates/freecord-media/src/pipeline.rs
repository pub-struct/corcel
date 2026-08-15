//! Shared plumbing for building a `gst-launch`-syntax pipeline and reaching
//! into it by element name. Nothing here needs a GLib main loop: pipelines
//! are driven by `appsink`/`appsrc`'s own blocking calls on dedicated
//! threads (see [`crate::capture`], [`crate::playback`]), and this module's
//! own bus watch is a plain thread as well.

use anyhow::Context;
use gstreamer::glib::object::{Cast, IsA};
use gstreamer::prelude::*;

pub fn build(description: &str) -> anyhow::Result<gstreamer::Pipeline> {
    let element = gstreamer::parse::launch(description)
        .with_context(|| format!("failed to build pipeline: {description}"))?;
    element.downcast::<gstreamer::Pipeline>().map_err(|_| {
        anyhow::anyhow!("pipeline description did not produce a top-level Pipeline: {description}")
    })
}

pub fn element<T: IsA<gstreamer::Element>>(
    pipeline: &gstreamer::Pipeline,
    name: &str,
) -> anyhow::Result<T> {
    pipeline
        .by_name(name)
        .with_context(|| format!("pipeline has no element named {name}"))?
        .downcast::<T>()
        .map_err(|_| anyhow::anyhow!("element {name} is not the expected type"))
}

/// Logs the pipeline's first error (or its EOS) to stderr from a dedicated
/// thread, then stops watching — the minimum viable bus watch for a
/// pipeline nothing else is polling. Blocks on `timed_pop_filtered` until
/// one of those arrives, or until [`stop`] flushes the bus — a plain
/// `set_state(Null)` teardown posts neither, so callers must go through
/// [`stop`] rather than dropping the pipeline directly, or this thread
/// leaks for the process's lifetime.
pub fn watch(pipeline: &gstreamer::Pipeline, label: &'static str) {
    let Some(bus) = pipeline.bus() else { return };
    std::thread::spawn(move || {
        while let Some(msg) = bus.timed_pop_filtered(
            gstreamer::ClockTime::NONE,
            &[gstreamer::MessageType::Error, gstreamer::MessageType::Eos],
        ) {
            match msg.view() {
                gstreamer::MessageView::Error(err) => {
                    eprintln!(
                        "freecord-media: {label} pipeline error: {} ({:?})",
                        err.error(),
                        err.debug()
                    );
                    break;
                }
                gstreamer::MessageView::Eos(_) => break,
                _ => {}
            }
        }
    });
}

/// Tears a pipeline down: `set_state(Null)` stops it, and flushing the bus
/// unblocks [`watch`]'s thread (flushing makes `timed_pop_filtered` return
/// `None` immediately instead of blocking forever waiting for an Error/EOS
/// that a plain `Null` transition never posts). Every `Drop` impl that owns
/// a pipeline built via [`watch`] should call this instead of `set_state`
/// directly.
pub fn stop(pipeline: &gstreamer::Pipeline) {
    let _ = pipeline.set_state(gstreamer::State::Null);
    if let Some(bus) = pipeline.bus() {
        bus.set_flushing(true);
    }
}
