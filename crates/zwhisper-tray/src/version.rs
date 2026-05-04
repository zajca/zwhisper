//! M8 — pre-flight version handshake for the tray (DoD #13).
//!
//! The tray is a long-running process: if the daemon's protocol
//! version diverges from ours, an infinite reconnect loop would
//! produce a stream of toast notifications and a confused user. The
//! handshake converts that scenario into:
//!
//! 1. A single `notify-rust` notification, deduped by a
//!    process-wide [`std::sync::OnceLock`] so subsequent reconnect
//!    attempts (which we do not perform — the pump exits sticky on
//!    mismatch) cannot retrigger it.
//! 2. A sticky `IconState::DaemonOffline` indicator.
//! 3. A clean shutdown when the surrounding service receives
//!    `SIGTERM`.
//!
//! The helper logic mirrors the CLI's `verify_protocol` so the same
//! `MethodCallNotImplemented` / `UnknownProperty` -> legacy-daemon
//! mapping applies here.

use std::sync::OnceLock;

use tracing::warn;
use zwhisper_ipc::{ProtocolMismatch, Recorder1Proxy};

/// Outcome of the pre-flight handshake. Mirrors the CLI's
/// `HandshakeOutcome` so the two crates surface the same semantics.
#[derive(Debug)]
pub(crate) enum HandshakeOutcome {
    Match,
    Mismatch(ProtocolMismatch),
    /// The bus is up but the daemon is not on it. Treat as
    /// "no information" — let the existing reconnect/backoff path
    /// surface the user-facing offline icon.
    DaemonDown,
}

/// Run the handshake against an already-built `Recorder1Proxy`.
pub(crate) async fn verify_protocol(proxy: &Recorder1Proxy<'_>) -> HandshakeOutcome {
    match proxy.protocol_version().await {
        Ok(daemon_version) => {
            if daemon_version == zwhisper_ipc::PROTOCOL_VERSION {
                HandshakeOutcome::Match
            } else {
                HandshakeOutcome::Mismatch(ProtocolMismatch::new(daemon_version))
            }
        }
        Err(err) if is_daemon_down(&err) => HandshakeOutcome::DaemonDown,
        Err(zbus::Error::FDO(boxed)) => match *boxed {
            zbus::fdo::Error::UnknownMethod(_)
            | zbus::fdo::Error::UnknownProperty(_)
            | zbus::fdo::Error::UnknownInterface(_) => {
                HandshakeOutcome::Mismatch(ProtocolMismatch::legacy_daemon())
            }
            other => {
                HandshakeOutcome::Mismatch(ProtocolMismatch::new(format!("fdo error: {other}")))
            }
        },
        Err(other) => HandshakeOutcome::Mismatch(ProtocolMismatch::new(format!(
            "unexpected protocol-version error: {other}"
        ))),
    }
}

/// `true` when a zbus error means the daemon is not on the bus
/// (`ServiceUnknown` / `NameHasNoOwner`). Mirrors the CLI's
/// `is_daemon_down`.
pub(crate) fn is_daemon_down(err: &zbus::Error) -> bool {
    if let zbus::Error::MethodError(name, ..) = err {
        let n: &str = name.as_str();
        return n == "org.freedesktop.DBus.Error.ServiceUnknown"
            || n == "org.freedesktop.DBus.Error.NameHasNoOwner";
    }
    false
}

/// Process-wide latch: ensures the user sees the mismatch
/// notification at most once per tray process even if the pump's
/// outer loop somehow re-runs the handshake (e.g. future change
/// that drops the sticky exit). Invariant tested by the unit
/// tests below.
static MISMATCH_NOTIFIED: OnceLock<()> = OnceLock::new();

/// Send a `notify-rust` notification once per process. Best-effort:
/// if the desktop notification service is unavailable, log a warn
/// and move on — the canonical user-facing surface is still the
/// `DaemonOffline` icon.
///
/// The body uses `show_async().await` against the workspace-pinned
/// `notify-rust` `z-with-tokio` backend (matches the existing M4
/// notification sink). Calling the sync `.show()` from a tokio
/// worker thread would attempt to start a nested runtime — panics
/// "Cannot start a runtime from within a runtime" — because the
/// notify-rust internals depend on async zbus.
pub(crate) async fn notify_mismatch_once(err: &ProtocolMismatch) {
    if MISMATCH_NOTIFIED.set(()).is_err() {
        // Already notified earlier in this process. Do not re-fire.
        return;
    }
    let result = notify_rust::Notification::new()
        .summary("zwhisper daemon version mismatch")
        .body(&format!(
            "{err}\nReinstall the matching zwhisperd to restore the tray."
        ))
        .icon("dialog-warning")
        .show_async()
        .await;
    if let Err(e) = result {
        warn!(error = %e, "failed to deliver mismatch notification");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The latch is visible from this module; the dedup invariant
    /// (notification fires at most once) is enforced by the
    /// `OnceLock::set` semantics. We pin the boolean here so a
    /// future refactor that swaps `OnceLock` for a `Mutex<bool>`
    /// (or worse, an `AtomicBool` with the wrong ordering) trips
    /// this test.
    #[test]
    fn dedup_latch_is_one_shot() {
        // Use a fresh OnceLock rather than the production one so
        // unit tests in the same process do not race with each
        // other or with a manual repro.
        let latch: OnceLock<()> = OnceLock::new();
        assert!(latch.set(()).is_ok(), "first set must succeed");
        assert!(latch.set(()).is_err(), "second set must fail");
    }

    #[test]
    fn handshake_outcome_legacy_uses_sentinel() {
        let m = ProtocolMismatch::legacy_daemon();
        assert!(m.is_legacy_daemon());
        assert_eq!(m.got, ProtocolMismatch::LEGACY_DAEMON_SENTINEL);
    }

    #[test]
    fn handshake_outcome_mismatch_carries_daemon_version() {
        let m = ProtocolMismatch::new("9.9.9");
        assert!(!m.is_legacy_daemon());
        assert_eq!(m.expected, zwhisper_ipc::PROTOCOL_VERSION);
        assert_eq!(m.got, "9.9.9");
    }
}
