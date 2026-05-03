//! M7 — tokio runtime + UI bridge.
//!
//! FLTK widgets are owned exclusively by the main thread. Async I/O
//! (D-Bus, reqwest, portal) lives on a dedicated tokio multi-thread
//! runtime spawned at boot. Cross-thread communication flows through
//! an unbounded mpsc plus `fltk::app::awake_callback`:
//!
//! - Worker tasks call `bridge.tx.send(UiMessage::*)` and then nudge
//!   FLTK via `awake_callback` so the main loop drains the channel
//!   on its next iteration.
//! - The main thread holds `bridge.rt_handle` to spawn additional
//!   tasks without owning the runtime itself.
//!
//! See M7-plan § 2 ("Threading model") and § 2.4
//! ("Window-close mid-download") for the cooperative-cancel
//! contract.

use color_eyre::eyre::WrapErr;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::app::UiMessage;

/// Number of worker threads in the side-thread runtime. The
/// downloader uses one (HTTP + sha256), portal calls use one,
/// leaving headroom for occasional `Profiles1.reload` round-trips
/// without queuing. M7-plan § 2.5 documents the trade-off vs a
/// `current_thread` runtime.
const WORKER_THREADS: usize = 2;

/// Cross-thread plumbing handed to every tab `build` call. Cloning
/// is cheap — `tx` is an unbounded mpsc sender and `rt_handle` is
/// `tokio::runtime::Handle` (Arc internally).
#[derive(Clone, Debug)]
pub(crate) struct UiBridge {
    /// Worker → main thread message channel. Unbounded so a slow
    /// repaint never blocks an HTTP chunk handler.
    #[allow(dead_code)] // Used by Group B/C/D tab implementations.
    pub(crate) tx: UnboundedSender<UiMessage>,
    /// Tokio runtime handle. Tabs use `rt_handle.spawn(future)` to
    /// kick off background work without owning the runtime.
    #[allow(dead_code)] // Used by Group B/C/D tab implementations.
    pub(crate) rt_handle: Handle,
    /// Cooperative cancel token. The FLTK quit handler calls
    /// `cancel_token.cancel()` so in-flight downloads can wind
    /// down before the runtime is dropped.
    pub(crate) cancel_token: CancellationToken,
}

/// Spawn the side-thread runtime and return everything `app::App`
/// needs to wire the UI to it. The returned `Runtime` is owned by
/// `main` so `Fl::run()` can outlive every spawned task; dropping
/// the runtime triggers `shutdown_background()` implicitly.
pub(crate) fn spawn_runtime() -> color_eyre::Result<(
    UiBridge,
    Runtime,
    UnboundedReceiver<UiMessage>,
    CancellationToken,
)> {
    let runtime = Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .thread_name("zwhisper-settings-rt")
        .build()
        .wrap_err("failed to build tokio runtime for zwhisper-settings")?;

    let (tx, rx) = mpsc::unbounded_channel::<UiMessage>();
    let cancel_token = CancellationToken::new();

    let bridge = UiBridge {
        tx,
        rt_handle: runtime.handle().clone(),
        cancel_token: cancel_token.clone(),
    };

    Ok((bridge, runtime, rx, cancel_token))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn spawn_runtime_returns_handle_and_token() {
        // Smoke: the constructor must produce a working Handle and
        // a fresh (uncancelled) token. We exercise the handle by
        // running a trivial future on it — if the runtime is dead
        // this hangs or panics.
        let (bridge, rt, _rx, token) = spawn_runtime().expect("runtime builds");
        assert!(
            !token.is_cancelled(),
            "fresh token must not be pre-cancelled"
        );
        let result = bridge.rt_handle.block_on(async { 42_u32 });
        assert_eq!(result, 42);
        // Cancel and confirm both clones see it.
        bridge.cancel_token.cancel();
        assert!(token.is_cancelled());
        // Drop the runtime explicitly so the test does not depend
        // on Drop ordering.
        drop(rt);
    }

    #[test]
    fn bridge_is_clone() {
        let (bridge, rt, _rx, _token) = spawn_runtime().expect("runtime builds");
        let cloned = bridge.clone();
        // Clones share the same channel — sending via either is
        // observable on the receiver. We do not have a UiMessage
        // constructor here that does not depend on a tab, so just
        // verify the cheap properties.
        assert!(!cloned.cancel_token.is_cancelled());
        drop(rt);
    }
}
