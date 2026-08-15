//! `freecord-signal`/`freecord-net`/`freecord-media` are all written against
//! tokio (`tokio::spawn`, `tokio::net`, ...), while GPUI drives its own
//! executor that knows nothing about tokio. Rather than push tokio into
//! GPUI's event loop, this keeps one tokio runtime alive on its own threads
//! for the lifetime of the app, and callers bridge results back to GPUI via
//! plain channels — `tokio::sync::mpsc::Receiver::recv` is just a future,
//! pollable from any executor, so `cx.spawn(async move { rx.recv().await })`
//! works without GPUI needing to know tokio exists.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Runtime;
use tokio::sync::oneshot;

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        // `Runtime::new()` defaults to one worker thread per CPU core, which
        // is wasted here: the actual work running on this runtime is I/O-bound
        // glue (signaling, RTP forwarding, a handful of connections) — the
        // CPU-heavy work (encode/decode) already runs on its own dedicated
        // threads via GStreamer (see freecord-media::pipeline), not as tokio
        // tasks. A idle 16-core machine would otherwise reserve 16 OS threads
        // for this runtime alone. 4 is comfortably more than this app's
        // concurrency needs without paying for cores it won't use.
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("failed to start tokio runtime")
    })
}

/// Runs `future` on the shared tokio runtime and returns a receiver for its
/// result. `oneshot::Receiver::await` needs no reactor of its own (just a
/// waker), so the GPUI side can await it directly from `cx.spawn` without
/// GPUI's executor needing to know tokio is involved at all.
pub fn spawn_and_send<F, T>(future: F) -> oneshot::Receiver<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    runtime().spawn(async move {
        let _ = tx.send(future.await);
    });
    rx
}
