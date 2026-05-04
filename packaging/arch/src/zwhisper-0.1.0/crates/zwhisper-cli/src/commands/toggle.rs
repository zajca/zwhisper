//! `zwhisper toggle` — universal start/stop entry point (M6 `DoD` #14).
//!
//! Calls the shared [`zwhisper_hotkey::toggle::toggle_once`] helper
//! against a real `Recorder1Proxy` + `Profiles1Proxy`, mapping the
//! outcome to the M3 exit-code table (0/1/2/3). The same logic is
//! used by the tray's hotkey listener — keeping CLI and tray on a
//! single decision path is the whole point of the lib crate.
//!
//! ## Daemon-down fallback (`DoD` #14)
//!
//! When the daemon is not on the bus, we still want the user to
//! see *something* on screen so a WM-bound `zwhisper toggle` does
//! not fail silently. We:
//!
//! 1. Spawn a `notify-rust` show in its own task wrapped in a
//!    500 ms `tokio::time::timeout` — the session bus may itself
//!    be the reason the daemon is down, so we cap the call so the
//!    user-facing exit isn't held up by a broken bus.
//! 2. NEVER mask the underlying daemon-down error: notification
//!    failure is logged via `tracing::warn!` only.
//! 3. Always print the stderr `toggle: FAIL (daemon not running)`
//!    line and exit 2 regardless of notification outcome.
//!
//! ## Output format (mirrors `backend health`)
//!
//! - `toggle: STARTED (session=<sid>, profile=<name>)` — exit 0
//! - `toggle: STOPPING (session=<sid>)`                — exit 0
//! - `toggle: NOOP (reason=<reason>)`                  — exit 0
//! - `toggle: FAIL (daemon not running)`               — exit 2
//! - `toggle: FAIL (no active profile; …)`             — exit 2
//! - `toggle: FAIL (rpc: <msg>)`                       — exit 3

use std::path::PathBuf;
use std::time::Duration;

use tracing::{debug, info, warn};

use zwhisper_hotkey::config::HotkeyConfig;
use zwhisper_hotkey::toggle::{
    Debouncer, LiveRecorderClient, NoOpReason, ToggleError, ToggleOutcome, toggle_once,
};
use zwhisper_ipc::{Profiles1Proxy, Recorder1Proxy};

use super::{EXIT_IPC_FAILURE, EXIT_OK, EXIT_PROTOCOL_ERROR, build_runtime};

/// How long the desktop notification call gets before we give up.
/// Bus may itself be broken (that is, after all, why the daemon is
/// unreachable); never let `notify-send` hang the CLI exit.
const NOTIFY_TIMEOUT: Duration = Duration::from_millis(500);

/// Body shown on the daemon-down notification (`DoD` #14). `notify-rust`
/// renders this verbatim — keep the actionable hint in the body.
const DAEMON_DOWN_NOTIFY_BODY: &str =
    "zwhisper: daemon not running. Run `systemctl --user start zwhisperd`.";

/// One-shot CLI entry point. Builds a current-thread runtime and
/// returns the M3-flavoured exit code via `process::exit`. Error
/// propagation through `color_eyre::Result` would collapse the
/// 0/1/2/3 spread into a single Err shape, so we exit explicitly
/// (see `commands::status::run` for the same pattern).
pub(crate) fn run() -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    let (code, stdout, stderr) = rt.block_on(run_async());
    if !stdout.is_empty() {
        println!("{stdout}");
    }
    if !stderr.is_empty() {
        eprintln!("{stderr}");
    }
    if code == EXIT_OK {
        Ok(())
    } else {
        std::process::exit(code);
    }
}

/// Driver — kept tiny so `format_outcome` carries the test-able
/// classification logic.
async fn run_async() -> (i32, String, String) {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            // Bus connect failure → treat as daemon-down per
            // [`ToggleError::from_zbus`] (which classifies
            // `Address` / `InputOutput` the same way). We do not
            // have a `ToggleError` here yet, so emulate the path
            // directly by firing the notification + exiting 2.
            debug!(error = %err, "session bus unreachable; treating as daemon-down");
            fire_daemon_down_notification().await;
            let (code, stdout, stderr) = format_outcome(Err(ToggleError::DaemonDown));
            return (code, stdout, stderr);
        }
    };

    let recorder = match Recorder1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            // The proxy build itself usually only fails on bus
            // teardown; classify as IPC failure and surface the
            // raw error.
            return (
                EXIT_IPC_FAILURE,
                String::new(),
                format!("toggle: FAIL (rpc: failed to build Recorder1 proxy: {err})"),
            );
        }
    };

    // M8 pre-flight handshake. Version mismatch aborts the toggle
    // before any state-mutating call so we never start a session
    // that the daemon will fail to honour at signal time.
    match super::verify_protocol(&recorder).await {
        super::HandshakeOutcome::Match | super::HandshakeOutcome::DaemonDown => {}
        super::HandshakeOutcome::Mismatch(err) => {
            let code = super::report_protocol_mismatch(&err);
            return (code, String::new(), String::new());
        }
    }

    let profiles = match Profiles1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            return (
                EXIT_IPC_FAILURE,
                String::new(),
                format!("toggle: FAIL (rpc: failed to build Profiles1 proxy: {err})"),
            );
        }
    };

    let cfg = load_hotkey_config();
    let mut debouncer = Debouncer::new(&cfg);
    let client = LiveRecorderClient::new(recorder, profiles);

    let result = toggle_once(&client, &mut debouncer).await;

    if matches!(result, Err(ToggleError::DaemonDown)) {
        fire_daemon_down_notification().await;
    }

    let (code, stdout, stderr) = format_outcome(result);
    info!(target: "zwhisper_cli::toggle", exit_code = code, "toggle complete");
    (code, stdout, stderr)
}

/// Pure mapping of `toggle_once`'s result onto the (`exit_code`,
/// `stdout`, `stderr`) triple. Lives outside `run_async` so unit tests
/// can drive the truth table without standing up tokio + zbus.
fn format_outcome(result: Result<ToggleOutcome, ToggleError>) -> (i32, String, String) {
    match result {
        Ok(ToggleOutcome::Started {
            session_id,
            profile,
        }) => (
            EXIT_OK,
            format!("toggle: STARTED (session={session_id}, profile={profile})"),
            String::new(),
        ),
        Ok(ToggleOutcome::Stopping { session_id }) => (
            EXIT_OK,
            format!("toggle: STOPPING (session={session_id})"),
            String::new(),
        ),
        Ok(ToggleOutcome::NoOp { reason }) => (
            EXIT_OK,
            format!("toggle: NOOP (reason={})", noop_reason_label(reason)),
            String::new(),
        ),
        Err(ToggleError::Debounced { .. }) => (
            EXIT_OK,
            String::new(),
            "toggle: NOOP (debounced)".to_owned(),
        ),
        Err(ToggleError::CoolingDown { .. }) => (
            EXIT_OK,
            String::new(),
            "toggle: NOOP (cooldown active)".to_owned(),
        ),
        Err(ToggleError::DaemonDown) => (
            EXIT_PROTOCOL_ERROR,
            String::new(),
            "toggle: FAIL (daemon not running)".to_owned(),
        ),
        Err(ToggleError::NoActiveProfile) => (
            EXIT_PROTOCOL_ERROR,
            String::new(),
            "toggle: FAIL (no active profile; run zwhisper profile set <name>)".to_owned(),
        ),
        Err(ToggleError::AlreadyActive) => {
            // Defensive: `toggle_once` should fold this into
            // `NoOp { AlreadyActive }` before it reaches us, but
            // the enum is `non_exhaustive`-style so the match
            // must cover every variant. If it does leak through,
            // treat it as the same benign outcome and keep exit
            // code 0 so callers never see a hard failure for the
            // concurrent-toggle race.
            (
                EXIT_OK,
                format!(
                    "toggle: NOOP (reason={})",
                    noop_reason_label(NoOpReason::AlreadyActive)
                ),
                String::new(),
            )
        }
        Err(ToggleError::Rpc(msg)) => (
            EXIT_IPC_FAILURE,
            String::new(),
            format!("toggle: FAIL (rpc: {msg})"),
        ),
    }
}

/// Friendly tag for the `NoOp` reason.
fn noop_reason_label(reason: NoOpReason) -> &'static str {
    match reason {
        NoOpReason::AlreadyDraining => "AlreadyDraining",
        NoOpReason::AlreadyActive => "AlreadyActive",
        NoOpReason::Unknown => "Unknown",
    }
}

/// Resolve `~/.config/zwhisper/hotkey.toml` and load it. Missing
/// or corrupt files fall back to defaults — see
/// [`HotkeyConfig::from_path`] for the policy. The CLI must NOT
/// die just because the optional config file has a typo.
fn load_hotkey_config() -> HotkeyConfig {
    if let Some(path) = hotkey_config_path() {
        HotkeyConfig::from_path(&path)
    } else {
        warn!("$XDG_CONFIG_HOME unresolved; using hotkey defaults");
        HotkeyConfig::default()
    }
}

/// `${XDG_CONFIG_HOME:-$HOME/.config}/zwhisper/hotkey.toml`. None
/// only if neither env var nor `$HOME` is resolvable, which would
/// be an exotic environment.
fn hotkey_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("zwhisper").join("hotkey.toml"))
}

/// Best-effort desktop notification — logs failures, never
/// propagates them. Wrapped in a 500 ms timeout because the
/// session bus might be the reason we're in this code path; we
/// don't want a broken bus to hang the CLI exit. The blocking
/// `notify-rust` call lives in a `spawn_blocking` task so it
/// cannot stall the runtime even when the timeout fires.
async fn fire_daemon_down_notification() {
    let join = tokio::task::spawn_blocking(|| {
        notify_rust::Notification::new()
            .appname("zwhisper")
            .summary("Cannot toggle recording")
            .body(DAEMON_DOWN_NOTIFY_BODY)
            .icon("zwhisper")
            .timeout(notify_rust::Timeout::Default)
            .urgency(notify_rust::Urgency::Critical)
            .show()
            .map(|_| ())
    });
    match tokio::time::timeout(NOTIFY_TIMEOUT, join).await {
        Ok(Ok(Ok(()))) => debug!("daemon-down notification delivered"),
        Ok(Ok(Err(e))) => warn!(error = %e, "daemon-down notification failed"),
        Ok(Err(e)) => warn!(error = %e, "daemon-down notification task panicked"),
        Err(_) => warn!(
            timeout_ms = u64::try_from(NOTIFY_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            "daemon-down notification timed out (bus likely broken)"
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn mapping_started_returns_exit_0() {
        let (code, stdout, stderr) = format_outcome(Ok(ToggleOutcome::Started {
            session_id: "abc-123".into(),
            profile: "default".into(),
        }));
        assert_eq!(code, EXIT_OK);
        assert_eq!(stdout, "toggle: STARTED (session=abc-123, profile=default)");
        assert!(stderr.is_empty(), "no stderr on success: {stderr}");
    }

    #[test]
    fn mapping_stopping_returns_exit_0() {
        let (code, stdout, stderr) = format_outcome(Ok(ToggleOutcome::Stopping {
            session_id: "sid-xyz".into(),
        }));
        assert_eq!(code, EXIT_OK);
        assert_eq!(stdout, "toggle: STOPPING (session=sid-xyz)");
        assert!(stderr.is_empty());
    }

    #[test]
    fn mapping_noop_already_draining_returns_exit_0() {
        let (code, stdout, _) = format_outcome(Ok(ToggleOutcome::NoOp {
            reason: NoOpReason::AlreadyDraining,
        }));
        assert_eq!(code, EXIT_OK);
        assert_eq!(stdout, "toggle: NOOP (reason=AlreadyDraining)");
    }

    #[test]
    fn mapping_debounced_returns_exit_0_with_stderr() {
        let (code, stdout, stderr) =
            format_outcome(Err(ToggleError::Debounced { debounce_ms: 250 }));
        // Debounced is correct behavior, not an error → exit 0.
        // The note goes to stderr so scripts can still scrape stdout
        // for accepted toggles.
        assert_eq!(code, EXIT_OK);
        assert!(stdout.is_empty());
        assert_eq!(stderr, "toggle: NOOP (debounced)");
    }

    #[test]
    fn mapping_cooling_down_returns_exit_0_with_stderr() {
        let (code, stdout, stderr) =
            format_outcome(Err(ToggleError::CoolingDown { cooldown_ms: 1500 }));
        assert_eq!(code, EXIT_OK);
        assert!(stdout.is_empty());
        assert_eq!(stderr, "toggle: NOOP (cooldown active)");
    }

    #[test]
    fn mapping_daemon_down_returns_exit_2() {
        let (code, stdout, stderr) = format_outcome(Err(ToggleError::DaemonDown));
        assert_eq!(code, EXIT_PROTOCOL_ERROR);
        assert!(stdout.is_empty());
        assert_eq!(stderr, "toggle: FAIL (daemon not running)");
    }

    #[test]
    fn mapping_no_active_profile_returns_exit_2() {
        let (code, stdout, stderr) = format_outcome(Err(ToggleError::NoActiveProfile));
        assert_eq!(code, EXIT_PROTOCOL_ERROR);
        assert!(stdout.is_empty());
        assert!(
            stderr.contains("no active profile"),
            "expected no-active-profile hint, got: {stderr}",
        );
        assert!(
            stderr.contains("zwhisper profile set"),
            "expected actionable command, got: {stderr}",
        );
    }

    #[test]
    fn mapping_rpc_returns_exit_3() {
        let (code, stdout, stderr) = format_outcome(Err(ToggleError::Rpc("session-in-use".into())));
        assert_eq!(code, EXIT_IPC_FAILURE);
        assert!(stdout.is_empty());
        assert_eq!(stderr, "toggle: FAIL (rpc: session-in-use)");
    }

    #[test]
    fn noop_reason_label_covers_all_variants() {
        assert_eq!(
            noop_reason_label(NoOpReason::AlreadyDraining),
            "AlreadyDraining"
        );
        assert_eq!(
            noop_reason_label(NoOpReason::AlreadyActive),
            "AlreadyActive"
        );
        assert_eq!(noop_reason_label(NoOpReason::Unknown), "Unknown");
    }

    #[test]
    fn mapping_noop_already_active_returns_exit_0() {
        // Concurrent-toggle race: another process won the
        // `StartRecording`. Daemon answered with the typed
        // `SessionInUse` error; the toggle decision folded it
        // into `NoOp(AlreadyActive)`. The CLI must exit 0
        // (benign) instead of exit 3 (rpc failure).
        let (code, stdout, _) = format_outcome(Ok(ToggleOutcome::NoOp {
            reason: NoOpReason::AlreadyActive,
        }));
        assert_eq!(code, EXIT_OK);
        assert_eq!(stdout, "toggle: NOOP (reason=AlreadyActive)");
    }

    #[test]
    fn mapping_already_active_error_falls_back_to_exit_0_noop() {
        // Defensive: even if the typed error somehow leaks past
        // `toggle_once`, the CLI must keep the user-facing exit
        // code at 0.
        let (code, stdout, stderr) = format_outcome(Err(ToggleError::AlreadyActive));
        assert_eq!(
            code, EXIT_OK,
            "AlreadyActive must never surface as a failure"
        );
        assert_eq!(stdout, "toggle: NOOP (reason=AlreadyActive)");
        assert!(stderr.is_empty());
    }
}
