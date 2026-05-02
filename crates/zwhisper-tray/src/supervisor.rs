//! Task D — ksni service liveness watch (Concurrency-3 contract).
//!
//! Per ksni 0.3.4 (`ksni::Handle::update`): the call returns `None`
//! when the underlying tray service has been shut down or panicked.
//! That return value is the only liveness signal we get — there's no
//! `JoinHandle` exposed by ksni's spawned event loop, and the
//! workspace forbids `unsafe` so we can't enable tokio's
//! `unhandled_panic` config (it requires `tokio_unstable`).
//!
//! The supervisor implements the C3 contract from M4-plan:
//!
//! 1. Whenever the watch channel publishes a new [`TrayState`],
//!    propagate it to ksni via `handle.update(|tray| tray.set_state(...))`.
//! 2. If `update` returns `None` we know the service is dead. Log
//!    and exit(1) so systemd's `Restart=on-failure` recovers.
//! 3. On a deliberate shutdown signal (Quit menu item or Ctrl-C in
//!    `main.rs`), we shut ksni down cleanly and return `Ok(())` —
//!    `main.rs` then exits 0.

use color_eyre::eyre::Result;
use tokio::sync::watch;
use tracing::{error, info};

use crate::state::TrayState;
use crate::tray::ZwhisperTray;

/// Outcome of a single `Handle::update` call, factored out so the
/// classification logic is testable without spinning up a real tray.
#[derive(Debug, PartialEq, Eq)]
pub enum SupervisorAction {
    /// `update` returned `Some` — the service accepted the snapshot.
    Continue,
    /// `update` returned `None` — the service has been shut down.
    /// The supervisor must exit(1) so systemd restarts us.
    ExitOne,
}

/// Pure helper used by [`run_supervisor`]. Mapping is trivial today
/// but the indirection lets unit tests verify the contract without
/// instantiating ksni.
///
/// We take `Option<&T>` rather than `Option<T>` so that callers can
/// pass a borrow of a non-`Copy` payload without a move; clippy's
/// `needless_pass_by_value` flags the by-value form because the
/// payload is never consumed.
#[must_use]
pub fn classify_handle_outcome<T>(update_result: Option<&T>) -> SupervisorAction {
    if update_result.is_some() {
        SupervisorAction::Continue
    } else {
        SupervisorAction::ExitOne
    }
}

/// Run the supervisor loop until shutdown is signalled or the ksni
/// service dies.
///
/// On unexpected death (`Handle::update` returns `None`) the function
/// calls `std::process::exit(1)`. On clean shutdown it returns
/// `Ok(())`. The exit-on-death is intentional: if ksni panicked,
/// continuing in a half-broken state would leave the user with a
/// stale icon and no way to interact with it. Systemd takes us back
/// up.
pub async fn run_supervisor(
    handle: ksni::Handle<ZwhisperTray>,
    mut state_rx: watch::Receiver<TrayState>,
    mut shutdown_rx: watch::Receiver<()>,
) -> Result<()> {
    // First push: propagate the initial state so the icon doesn't
    // sit in `TrayState::default()` until the first signal arrives.
    let initial = state_rx.borrow_and_update().clone();
    let result = handle.update(move |t| t.set_state(initial)).await;
    if matches!(
        classify_handle_outcome(result.as_ref()),
        SupervisorAction::ExitOne
    ) {
        error!("ksni handle dropped at startup; exit(1) per C3");
        std::process::exit(1);
    }

    loop {
        tokio::select! {
            biased;

            res = shutdown_rx.changed() => {
                info!("supervisor shutdown requested");
                let _shutdown = handle.shutdown();
                // `shutdown()` returns a future that resolves when
                // the service has fully exited; awaiting it would
                // block our caller's clean exit if ksni is wedged.
                // We accept "best-effort" semantics here: the main
                // task will join us regardless.
                if res.is_err() {
                    // Sender dropped without ever sending — treat
                    // as shutdown anyway.
                }
                return Ok(());
            }

            res = state_rx.changed() => {
                if res.is_err() {
                    // The pump dropped its sender. That happens
                    // only on shutdown; treat as clean exit.
                    info!("state channel closed; supervisor exiting");
                    return Ok(());
                }
                let snapshot = state_rx.borrow_and_update().clone();
                let result = handle.update(move |t| t.set_state(snapshot)).await;
                if matches!(
                    classify_handle_outcome(result.as_ref()),
                    SupervisorAction::ExitOne,
                ) {
                    error!("ksni service died; exit(1) per C3");
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn classify_handle_outcome_some_continues() {
        let v = ();
        assert_eq!(
            classify_handle_outcome(Some(&v)),
            SupervisorAction::Continue,
        );
    }

    #[test]
    fn classify_handle_outcome_none_exits_one() {
        assert_eq!(
            classify_handle_outcome::<()>(None),
            SupervisorAction::ExitOne,
        );
    }

    #[test]
    fn classify_handle_outcome_carries_through_payload_type() {
        // The classifier discards the payload — exercise that with
        // a non-unit type to make sure we don't accidentally rely on
        // `()`.
        let n: i32 = 42;
        let outcome: SupervisorAction = classify_handle_outcome(Some(&n));
        assert_eq!(outcome, SupervisorAction::Continue);
    }
}
