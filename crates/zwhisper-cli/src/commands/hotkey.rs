//! `zwhisper hotkey {status,bind,unbind,probe}` — manage and probe
//! the system-wide `GlobalShortcuts` portal binding (M6 `DoD` #10–#13).
//!
//! Bypasses the daemon entirely. Each subcommand mirrors the M5
//! `backend health` style: single-line stdout summary with a
//! machine-parsable verdict prefix and an exit code that carries
//! the actionable signal (0 OK, 2 unavailable / cancel / no portal).
//!
//! ## Output truth table
//!
//! ```text
//! status  → BOUND       (chord, portal=<backend>, shortcut=<id>) → 0
//!         → NOT_BOUND   (portal=<backend> available)             → 0
//!         → UNAVAILABLE (no GlobalShortcuts portal …)            → 2
//! bind    → BOUND       (<chord>)                                → 0
//!         → bind cancelled by user                               → 2
//!         → bind timed out after Ns                              → 2
//!         → UNAVAILABLE (no GlobalShortcuts portal …)            → 2
//! unbind  → unbound                                              → 0
//! probe   → portal=<backend> GlobalShortcuts=<true|false>
//!           version=<n|none> reason=<reason>                     → 0|2
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use zwhisper_hotkey::config::HotkeyConfig;
use zwhisper_hotkey::portal::{
    AshpdAdapter, BindRequest, BoundShortcut, HotkeySession, PortalError, SHORTCUT_DESCRIPTION,
    SHORTCUT_ID,
};
use zwhisper_hotkey::probe::{self, BackendDetected, ProbeReport};

use super::{EXIT_OK, EXIT_PROTOCOL_ERROR, build_runtime};
use crate::cli::HotkeyCmd;

/// App-id passed to the portal `CreateSession` call. Reuses the
/// tray's well-known bus name so a CLI-bound shortcut survives a
/// later tray takeover (architecture proposal § 5).
const APP_ID: &str = "cz.zajca.Zwhisper1.Tray";

/// One-shot CLI entry point. Dispatches to per-subcommand `run_*`
/// helpers and surfaces the (`exit_code`, `stdout`, `stderr`) triple.
pub(crate) fn run(cmd: &HotkeyCmd) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    let (code, stdout, stderr) = rt.block_on(run_async(cmd));
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

async fn run_async(cmd: &HotkeyCmd) -> (i32, String, String) {
    match cmd {
        HotkeyCmd::Status => run_status().await,
        HotkeyCmd::Bind => run_bind().await,
        HotkeyCmd::Unbind => run_unbind().await,
        HotkeyCmd::Probe => run_probe().await,
    }
}

// =====================================================================
// status
// =====================================================================

async fn run_status() -> (i32, String, String) {
    let report = probe::probe().await;
    let list_result = if report.global_shortcuts_available {
        match open_session().await {
            Ok(session) => {
                let listed = session.list_shortcuts().await;
                // Best-effort close — session drop already aborts
                // the listener task, but we drop explicitly so the
                // portal sees a clean exit.
                let _ = session.close().await;
                Some(listed)
            }
            Err(e) => Some(Err(e)),
        }
    } else {
        None
    };
    let triple = format_status_report(&report, list_result.as_ref());
    info!(target: "zwhisper_cli::hotkey", verdict = %triple.0, "status complete");
    (triple.0, triple.1, triple.2)
}

/// Pure output formatter for `hotkey status`. Lives outside
/// `run_status` so unit tests can drive the truth table without
/// touching the live portal.
///
/// Inputs:
/// - `report`         — outcome of `probe::probe()`.
/// - `list_result`    — `None` if we did not even try to call
///   `list_shortcuts` (because the portal is unavailable);
///   `Some(Ok(vec))` when the call returned a list (which may be
///   empty); `Some(Err(_))` when the call failed.
///
/// Returns `(exit_code, stdout, stderr)`. We render a single line
/// per call — verdict on stdout for OK paths, on stderr for FAIL.
#[allow(clippy::type_complexity)]
fn format_status_report(
    report: &ProbeReport,
    list_result: Option<&Result<Vec<BoundShortcut>, PortalError>>,
) -> (i32, String, String) {
    if !report.global_shortcuts_available {
        // Portal-less Wayland sessions still support compositor
        // binds, so point directly at the universal toggle command.
        let stderr = format!(
            "hotkey: UNAVAILABLE (no GlobalShortcuts portal — {} detected; \
             use `zwhisper toggle` in a Wayland compositor bind)",
            backend_label_for_unavailable(&report.backend),
        );
        return (EXIT_PROTOCOL_ERROR, String::new(), stderr);
    }

    let backend = backend_label(&report.backend);
    match list_result {
        Some(Ok(shortcuts)) if !shortcuts.is_empty() => {
            // First binding wins for the summary line. Users who
            // bind multiple chords (not currently a supported
            // workflow) will only see the first; that's fine for
            // M6 — we only ever bind `SHORTCUT_ID` ourselves.
            let bound = &shortcuts[0];
            // `bound.id` is the shortcut id we registered with
            // the portal (e.g. "toggle-recording"), NOT the
            // portal session handle. Earlier versions mislabeled
            // it `session=…` — this format is the corrected
            // contract.
            let stdout = format!(
                "hotkey: BOUND ({}, portal={}, shortcut={})",
                bound.trigger_description, backend, bound.id,
            );
            (EXIT_OK, stdout, String::new())
        }
        Some(Ok(_empty)) => {
            let stdout = format!("hotkey: NOT_BOUND (portal={backend} available)");
            (EXIT_OK, stdout, String::new())
        }
        Some(Err(err)) => {
            // The portal exists but listing failed — surface as
            // UNAVAILABLE so the user gets a concrete next step.
            let stderr = format!("hotkey: UNAVAILABLE (list_shortcuts failed: {err})");
            (EXIT_PROTOCOL_ERROR, String::new(), stderr)
        }
        None => {
            // Should not happen — `report.global_shortcuts_available`
            // was true so we tried to open the session. Treat as
            // an internal error and exit 2.
            let stderr = format!("hotkey: UNAVAILABLE (portal={backend} probe inconsistent)");
            (EXIT_PROTOCOL_ERROR, String::new(), stderr)
        }
    }
}

// =====================================================================
// bind
// =====================================================================

async fn run_bind() -> (i32, String, String) {
    let cfg = load_hotkey_config();
    let session = match open_session().await {
        Ok(s) => s,
        Err(PortalError::Unavailable) => {
            return (
                EXIT_PROTOCOL_ERROR,
                String::new(),
                unavailable_line_no_backend(),
            );
        }
        Err(err) => {
            return (
                EXIT_PROTOCOL_ERROR,
                String::new(),
                format!("hotkey: UNAVAILABLE (session create failed: {err})"),
            );
        }
    };

    let req = BindRequest {
        id: SHORTCUT_ID.to_owned(),
        description: SHORTCUT_DESCRIPTION.to_owned(),
        preferred_trigger: None,
    };
    let timeout_secs = cfg.bind_timeout_secs;
    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), session.bind(&req)).await;

    let triple = match result {
        Err(_elapsed) => (
            EXIT_PROTOCOL_ERROR,
            String::new(),
            format!("hotkey: bind timed out after {timeout_secs}s"),
        ),
        Ok(Ok(shortcuts)) if !shortcuts.is_empty() => {
            let bound = &shortcuts[0];
            (
                EXIT_OK,
                format!("hotkey: BOUND ({})", bound.trigger_description),
                String::new(),
            )
        }
        Ok(Ok(_empty)) => (
            EXIT_PROTOCOL_ERROR,
            String::new(),
            "hotkey: bind returned no shortcuts (portal accepted but did not record a chord)"
                .to_owned(),
        ),
        Ok(Err(PortalError::BindCancelled)) => (
            EXIT_PROTOCOL_ERROR,
            String::new(),
            "hotkey: bind cancelled by user".to_owned(),
        ),
        Ok(Err(PortalError::Unavailable)) => (
            EXIT_PROTOCOL_ERROR,
            String::new(),
            unavailable_line_no_backend(),
        ),
        Ok(Err(err)) => (
            EXIT_PROTOCOL_ERROR,
            String::new(),
            format!("hotkey: bind failed ({err})"),
        ),
    };

    let _ = session.close().await;
    info!(target: "zwhisper_cli::hotkey", "bind complete");
    triple
}

// =====================================================================
// unbind — `DoD` #13 (idempotent)
// =====================================================================

async fn run_unbind() -> (i32, String, String) {
    match open_session().await {
        Ok(session) => {
            // `HotkeySession::unbind` is already idempotent
            // (returns Ok even on adapter failure with a warn
            // log). We mirror that here — exit 0 always.
            if let Err(e) = session.unbind().await {
                warn!(error = %e, "ignoring unbind error (idempotent)");
            }
            let _ = session.close().await;
        }
        Err(err) => {
            // No session means there's nothing to unbind. Per
            // `DoD` #13 this is still an exit-0 success. Log so the
            // operator can spot the case in the journal.
            debug!(error = %err, "no session for unbind; treating as already-unbound");
        }
    }
    (EXIT_OK, "hotkey: unbound".to_owned(), String::new())
}

// =====================================================================
// probe — diagnostic
// =====================================================================

async fn run_probe() -> (i32, String, String) {
    let report = probe::probe().await;
    format_probe_report(&report)
}

/// Pure output formatter for `hotkey probe`. Mirrors the
/// `backend health` line shape so scripts can grep for the same
/// `key=value` tokens regardless of subcommand.
fn format_probe_report(report: &ProbeReport) -> (i32, String, String) {
    let backend = backend_label(&report.backend);
    let available = report.global_shortcuts_available;
    let version = report
        .portal_version
        .map_or_else(|| "none".to_owned(), |v| v.to_string());
    let stdout = format!(
        "hotkey: portal={backend} GlobalShortcuts={available} version={version} reason={}",
        report.reason,
    );
    if available {
        (EXIT_OK, stdout, String::new())
    } else {
        (EXIT_PROTOCOL_ERROR, stdout, String::new())
    }
}

// =====================================================================
// helpers
// =====================================================================

/// Friendly tag for the `backend=…` field in stdout/stderr.
fn backend_label(backend: &BackendDetected) -> String {
    match backend {
        BackendDetected::Kde => "kde".to_owned(),
        BackendDetected::Gnome => "gnome".to_owned(),
        BackendDetected::Wlr => "wlr".to_owned(),
        BackendDetected::Other(s) => s.clone(),
        BackendDetected::None => "none".to_owned(),
    }
}

/// Hint string for the `UNAVAILABLE` line when probing detected
/// no portal. We point users at `zwhisper toggle`, which stays
/// usable from compositor keybinds without a portal.
fn backend_label_for_unavailable(backend: &BackendDetected) -> String {
    match backend {
        BackendDetected::None => "Wayland compositor bind".to_owned(),
        other => format!("portal={}", backend_label(other)),
    }
}

/// Same line we print when `bind`/`status` fail with `Unavailable`
/// without having a `ProbeReport` in scope.
fn unavailable_line_no_backend() -> String {
    "hotkey: UNAVAILABLE (no GlobalShortcuts portal — \
     use `zwhisper toggle` in a Wayland compositor bind)"
        .to_owned()
}

/// Build a `HotkeySession` against the live `AshpdAdapter`. The
/// adapter has to be wrapped in `Arc` because `HotkeySession`
/// stores it inside the session and may keep it alive past the
/// caller's stack frame (event listener task).
async fn open_session() -> Result<HotkeySession<AshpdAdapter>, PortalError> {
    let adapter = Arc::new(AshpdAdapter::new());
    HotkeySession::create(adapter, APP_ID).await
}

/// Resolve `~/.config/zwhisper/hotkey.toml` and load it. Same
/// fallback policy as `commands::toggle::load_hotkey_config` —
/// missing or corrupt file → defaults.
fn load_hotkey_config() -> HotkeyConfig {
    if let Some(path) = hotkey_config_path() {
        HotkeyConfig::from_path(&path)
    } else {
        warn!("$XDG_CONFIG_HOME unresolved; using hotkey defaults");
        HotkeyConfig::default()
    }
}

fn hotkey_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("zwhisper").join("hotkey.toml"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn report_no_portal() -> ProbeReport {
        ProbeReport {
            backend: BackendDetected::None,
            global_shortcuts_available: false,
            portal_version: None,
            reason:
                "no GlobalShortcuts portal — use a Wayland compositor bind for `zwhisper toggle`"
                    .to_owned(),
        }
    }

    fn report_kde_v2() -> ProbeReport {
        ProbeReport {
            backend: BackendDetected::Kde,
            global_shortcuts_available: true,
            portal_version: Some(2),
            reason: "portal-kde GlobalShortcuts v2".to_owned(),
        }
    }

    fn fixture_bound() -> BoundShortcut {
        BoundShortcut {
            id: "toggle-recording".to_owned(),
            trigger_description: "Ctrl+Alt+R".to_owned(),
            description: "Toggle zwhisper recording".to_owned(),
        }
    }

    // ---- format_status_report truth table -------------------------

    #[test]
    fn status_no_portal_returns_exit_2_unavailable() {
        let report = report_no_portal();
        let (code, stdout, stderr) = format_status_report(&report, None);
        assert_eq!(code, EXIT_PROTOCOL_ERROR);
        assert!(stdout.is_empty());
        assert!(stderr.contains("UNAVAILABLE"), "got: {stderr}");
        assert!(stderr.contains("Wayland compositor bind"), "got: {stderr}");
        assert!(!stderr.contains("i3/X11"), "got: {stderr}");
        assert!(stderr.contains("zwhisper toggle"), "got: {stderr}");
    }

    #[test]
    fn status_bound_emits_first_chord_with_shortcut_id() {
        let report = report_kde_v2();
        let bindings = vec![fixture_bound()];
        let (code, stdout, _) = format_status_report(&report, Some(&Ok(bindings)));
        assert_eq!(code, EXIT_OK);
        assert!(
            stdout.starts_with("hotkey: BOUND (Ctrl+Alt+R, portal=kde, shortcut=toggle-recording)"),
            "got: {stdout}",
        );
        // Belt + braces: the legacy `session=` field MUST NOT
        // be re-introduced — `bound.id` is a shortcut id, not a
        // portal session handle.
        assert!(
            !stdout.contains("session="),
            "legacy session= label must not reappear: {stdout}",
        );
    }

    #[test]
    fn status_not_bound_when_list_is_empty() {
        let report = report_kde_v2();
        let (code, stdout, _) = format_status_report(&report, Some(&Ok(vec![])));
        assert_eq!(code, EXIT_OK);
        assert_eq!(stdout, "hotkey: NOT_BOUND (portal=kde available)");
    }

    #[test]
    fn status_list_error_falls_through_to_unavailable() {
        let report = report_kde_v2();
        let err: Result<Vec<BoundShortcut>, PortalError> =
            Err(PortalError::Ashpd("transport gone".into()));
        let (code, stdout, stderr) = format_status_report(&report, Some(&err));
        assert_eq!(code, EXIT_PROTOCOL_ERROR);
        assert!(stdout.is_empty());
        assert!(stderr.contains("list_shortcuts failed"), "got: {stderr}");
    }

    // ---- format_probe_report truth table --------------------------

    #[test]
    fn probe_available_kde_returns_exit_0_with_version() {
        let report = report_kde_v2();
        let (code, stdout, _) = format_probe_report(&report);
        assert_eq!(code, EXIT_OK);
        assert!(stdout.contains("portal=kde"), "got: {stdout}");
        assert!(stdout.contains("GlobalShortcuts=true"), "got: {stdout}",);
        assert!(stdout.contains("version=2"), "got: {stdout}");
    }

    #[test]
    fn probe_no_portal_returns_exit_2_with_version_none() {
        let report = report_no_portal();
        let (code, stdout, _) = format_probe_report(&report);
        assert_eq!(code, EXIT_PROTOCOL_ERROR);
        assert!(stdout.contains("portal=none"), "got: {stdout}");
        assert!(stdout.contains("GlobalShortcuts=false"), "got: {stdout}",);
        assert!(stdout.contains("version=none"), "got: {stdout}");
        assert!(stdout.contains("Wayland compositor bind"), "got: {stdout}");
        assert!(!stdout.contains("i3/X11"), "got: {stdout}");
    }

    #[test]
    fn probe_other_backend_renders_raw_label() {
        let report = ProbeReport {
            backend: BackendDetected::Other("xdg-desktop-portal-foo".into()),
            global_shortcuts_available: true,
            portal_version: Some(1),
            reason: "portal=xdg-desktop-portal-foo GlobalShortcuts v1".to_owned(),
        };
        let (code, stdout, _) = format_probe_report(&report);
        assert_eq!(code, EXIT_OK);
        assert!(
            stdout.contains("portal=xdg-desktop-portal-foo"),
            "got: {stdout}",
        );
    }

    #[test]
    fn backend_label_covers_every_variant() {
        assert_eq!(backend_label(&BackendDetected::Kde), "kde");
        assert_eq!(backend_label(&BackendDetected::Gnome), "gnome");
        assert_eq!(backend_label(&BackendDetected::Wlr), "wlr");
        assert_eq!(backend_label(&BackendDetected::Other("foo".into())), "foo",);
        assert_eq!(backend_label(&BackendDetected::None), "none");
    }
}
