//! Delivery sinks for the `zwhisper deliver --listen` consumer.
//!
//! This module owns the two best-effort delivery mechanisms the
//! consumer drives off `Jobs1.JobCompleted`:
//!
//! - [`ClipboardSink`] — a long-lived `arboard::Clipboard` handle. The
//!   handle MUST outlive each individual `set_text` so the Wayland
//!   selection survives (binding amendment C1, copied verbatim from the
//!   tray crate which we deliberately do NOT depend on — `zwhisper-tray`
//!   is excluded from the workspace).
//! - [`notify`] — a one-shot desktop notification via `notify-rust`,
//!   shown from inside `spawn_blocking` so the tokio reactor never
//!   stalls on a synchronous D-Bus round-trip.
//!
//! [`decide_clipboard`] is the pure F3.3 intent guard. It is the only
//! place that decides whether a clipboard output entry results in an
//! actual injection. It is exhaustively unit-tested below because the
//! daemon cannot know the user's wait-intent — we infer it from
//! `submit_mode` plus a size ceiling, and getting that wrong would
//! either clobber the user's clipboard out from under them (detached
//! job finishing minutes later) or silently drop a transcript.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Maximum transcript size we are willing to push straight into the
/// clipboard. Past this we notify-with-action instead: a multi-hundred-KB
/// blob landing in the clipboard unannounced is hostile, and some
/// compositors choke on very large selections. Named per CLAUDE.md
/// "no hardcoded values"; this is the design ceiling, not a tunable.
pub(crate) const CLIPBOARD_MAX_BYTES: u64 = 100_000;

/// Secondary guard documented by RFC-daemon-role F3.3. Intent
/// (`submit_mode`) is the PRIMARY guard — a transcript whose owning job
/// was detached/auto never auto-injects regardless of age. This staleness
/// threshold exists as a defensive backstop for the (currently
/// unreachable) case where a `foreground` job's completion is delivered
/// to us long after the user stopped waiting — e.g. the consumer was
/// restarted and replayed a buffered signal. It is intentionally unused
/// by [`decide_clipboard`] today (intent already covers every live path)
/// and is kept as a named, documented constant so a future replay-aware
/// guard has the threshold pinned in one place.
///
/// `dead_code`: intentionally not referenced by non-test code today — it
/// is a documented, pinned backstop value for a future replay-aware guard
/// (see doc above). Kept per RFC-daemon-role F3.3.
#[allow(dead_code)]
pub(crate) const CLIPBOARD_STALE_THRESHOLD: Duration = Duration::from_secs(10);

/// Outcome of the F3.3 clipboard intent guard.
///
/// `Skip` is part of the contract surface (the consumer's match is
/// exhaustive over it) even though [`decide_clipboard`] never returns it
/// today — every clipboard entry maps to inject-or-notify. It is the
/// explicit "this clipboard entry is a no-op" arm so a future caller can
/// short-circuit without inventing a new value.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardDecision {
    /// User is actively waiting (foreground) and the transcript fits:
    /// inject straight into the clipboard.
    Inject,
    /// Do not inject — instead raise a notification offering a manual
    /// copy. Either the job was detached/auto (the user is not waiting,
    /// so silently overwriting the clipboard would be surprising) or the
    /// transcript is too large for the clipboard.
    NotifyWithAction,
    /// Nothing to do for this clipboard entry.
    Skip,
}

/// Pure F3.3 intent guard. Decides what a `clipboard` output entry means
/// given the job's submit mode and the transcript size.
///
/// Rules (in priority order):
/// 1. `bytes > max_bytes` → [`ClipboardDecision::NotifyWithAction`]
///    (too large) — size wins even for a foreground job, so we never
///    shove a huge blob into the clipboard.
/// 2. `submit_mode == "foreground"` AND `bytes <= max_bytes` →
///    [`ClipboardDecision::Inject`] — the user ran a blocking command and
///    is staring at the terminal; inject immediately.
/// 3. `submit_mode` is `detached` / `auto` (or anything else) →
///    [`ClipboardDecision::NotifyWithAction`] — the user is not actively
///    waiting; offer a copy instead of hijacking the clipboard.
#[must_use]
pub(crate) fn decide_clipboard(submit_mode: &str, bytes: u64, max_bytes: u64) -> ClipboardDecision {
    if bytes > max_bytes {
        // Size guard is checked first so an oversized foreground
        // transcript still degrades to a notification rather than
        // injecting.
        return ClipboardDecision::NotifyWithAction;
    }
    match submit_mode {
        "foreground" => ClipboardDecision::Inject,
        // detached | auto | any future/unknown mode: treat as
        // "user not actively waiting" — the safe, non-surprising default.
        _ => ClipboardDecision::NotifyWithAction,
    }
}

/// Long-lived clipboard handle. Cheap to construct; the underlying
/// `arboard::Clipboard` is lazily opened on first injection and then held
/// for the whole process lifetime (C1). `std::sync::Mutex` is correct
/// here: the lock is only ever held inside `spawn_blocking`, never across
/// an `.await`.
#[derive(Clone)]
pub(crate) struct ClipboardSink {
    clipboard: Arc<Mutex<Option<arboard::Clipboard>>>,
}

impl ClipboardSink {
    pub(crate) fn new() -> Self {
        Self {
            clipboard: Arc::new(Mutex::new(None)),
        }
    }

    /// Inject `text` into the clipboard. Returns a stringified error on
    /// failure so the caller can fall back to a notification. Runs the
    /// synchronous `arboard` work inside `spawn_blocking`; the handle is
    /// stored back into the shared `Option` so it outlives this call and
    /// the Wayland selection survives (C1).
    pub(crate) async fn inject(&self, text: &str) -> Result<(), String> {
        let clipboard = Arc::clone(&self.clipboard);
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || -> Result<(), arboard::Error> {
            let mut guard = clipboard.lock().map_err(|e| arboard::Error::Unknown {
                description: format!("clipboard mutex poisoned: {e}"),
            })?;
            if guard.is_none() {
                *guard = Some(arboard::Clipboard::new()?);
            }
            // We just inserted `Some(_)` when empty, so this branch is
            // always taken; the `if let` avoids an `unwrap` (denied lint).
            if let Some(cb) = guard.as_mut() {
                cb.set_text(text)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| format!("clipboard blocking task panicked: {e}"))?
        .map_err(|e| e.to_string())
    }
}

/// Show a one-shot desktop notification. Best-effort: failures are logged
/// at WARN and swallowed — a missing notification daemon must never crash
/// the consumer. The `notify-rust` call is synchronous, so it runs inside
/// `spawn_blocking` to keep the reactor responsive.
pub(crate) async fn notify(summary: &str, body: &str) {
    let summary = summary.to_owned();
    let body = body.to_owned();
    let join = tokio::task::spawn_blocking(move || {
        notify_rust::Notification::new()
            .appname("zwhisper")
            .summary(&summary)
            .body(&body)
            .icon("zwhisper-idle")
            .timeout(notify_rust::Timeout::Default)
            .show()
            .map(|_| ())
    })
    .await;
    match join {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "deliver: notification failed"),
        Err(e) => tracing::warn!(error = %e, "deliver: notification blocking task panicked"),
    }
}

// ---------------------------------------------------------------------------
// Type-at-cursor sink (RFC-type-at-cursor). Mirrors the clipboard machinery
// above: a pure intent guard (`decide_type`), an injectable command runner
// (`CommandRunner` + `WtypeRunner`), and an async wrapper (`TypeSink`). The
// `notify` helper above is shared by both deliveries.
// ---------------------------------------------------------------------------

/// OD1: 8 KB design ceiling (~4-5k chars ~ 6-8 min speech). Larger
/// transcripts degrade to clipboard. Deliberately smaller than
/// [`CLIPBOARD_MAX_BYTES`]: `wtype` types char-by-char and holds the virtual
/// keyboard for the whole payload, so a huge transcript would lock the
/// keyboard for minutes. Named per CLAUDE.md "no hardcoded values"; this is
/// the design ceiling, not a tunable.
pub(crate) const TYPE_MAX_BYTES: u64 = 8_192;

/// OD3: inter-keystroke delay left at `wtype`'s default (0). Raised only if
/// the manual Sway pass shows dropped characters. Named so the tuning knob
/// lives in one place even though the current value is the library default.
///
/// `dead_code`: intentionally not referenced by non-test code today — it is a
/// documented, pinned tuning value (see doc above). Kept per
/// RFC-type-at-cursor OD3.
#[allow(dead_code)]
pub(crate) const WTYPE_KEYSTROKE_DELAY_MS: u64 = 0;

/// Upper bound on a single `wtype` invocation. A compositor that accepts the
/// connection but never drains keystrokes must not wedge the consumer, so the
/// runner kills the child once this elapses. Picked generously: even a
/// full-ceiling transcript types well within ten seconds at the default rate.
pub(crate) const WTYPE_TIMEOUT: Duration = Duration::from_secs(10);

/// Outcome of the F4 type-at-cursor intent guard.
///
/// Stricter sibling of [`ClipboardDecision`]: there is no `Inject`/`Skip`
/// split because the only two outcomes for a `type_at_cursor` entry are
/// "type it" or "defer to a notification with a manual action".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeDecision {
    /// Foreground + fits: type at the cursor.
    Type,
    /// Too large OR not foreground: skip typing, notify with a manual action
    /// offering `zwhisper output last --to type` / `--to clipboard`.
    NotifyWithAction,
}

/// Pure F4 intent guard. Stricter sibling of [`decide_clipboard`]: typing is
/// far more intrusive than a clipboard write (it injects keystrokes into the
/// focused window), so we only ever type for a foreground job that fits the
/// (smaller) type ceiling.
///
/// Rules (in priority order):
/// 1. `bytes > max_bytes` → [`TypeDecision::NotifyWithAction`] — the size
///    ceiling is checked FIRST, so an oversized foreground transcript still
///    degrades to a notification rather than holding the virtual keyboard for
///    minutes.
/// 2. `submit_mode == "foreground"` AND `bytes <= max_bytes` →
///    [`TypeDecision::Type`] — the user ran a blocking command and is focused
///    on the target window; type at the cursor.
/// 3. `submit_mode` is `detached` / `auto` (or anything else / empty) →
///    [`TypeDecision::NotifyWithAction`] — the user is not actively waiting,
///    so injecting keystrokes into whatever happens to be focused would be
///    hostile.
#[must_use]
pub(crate) fn decide_type(submit_mode: &str, bytes: u64, max_bytes: u64) -> TypeDecision {
    if bytes > max_bytes {
        // Size guard first so an oversized foreground transcript degrades to
        // a notification rather than typing for minutes.
        return TypeDecision::NotifyWithAction;
    }
    match submit_mode {
        "foreground" => TypeDecision::Type,
        // detached | auto | any future/unknown mode | empty: the user is not
        // actively waiting — never inject keystrokes into the focused window.
        _ => TypeDecision::NotifyWithAction,
    }
}

/// Constant argv for the typing command. Factored out and unit-tested so the
/// shape of the invocation (`wtype -`, reading the payload from stdin, NO
/// shell) is pinned in one place and cannot drift.
#[must_use]
pub(crate) fn wtype_argv() -> [&'static str; 2] {
    ["wtype", "-"]
}

/// Executes the typing command with `text` on stdin. Synchronous (it is
/// always driven inside `spawn_blocking`) and injectable so the F4/F5 logic
/// can be unit-tested without a live compositor. Returns `Err(reason)` on
/// spawn failure / non-zero exit / timeout so the caller can run the F6
/// clipboard fallback.
pub(crate) trait CommandRunner: Send + Sync + 'static {
    fn type_text(&self, text: &str) -> Result<(), String>;
}

/// Production runner: spawns `wtype -` ([`wtype_argv`], constant argv, NO
/// shell), writes `text` to the child's stdin, closes it, and waits up to
/// `timeout`. On timeout it kills the child and returns `Err`. Non-zero exit,
/// spawn errors, and the timeout all map to `Err(String)`.
///
/// The timeout is enforced without adding a dependency: the blocking
/// `child.wait()` runs on a helper thread that reports the result back over a
/// channel, and the caller waits with [`std::sync::mpsc::Receiver::recv_timeout`].
/// UTF-8 and newlines pass through verbatim — we never pre-transform per
/// keyboard layout (G3 layout independence).
pub(crate) struct WtypeRunner {
    timeout: Duration,
}

impl WtypeRunner {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl CommandRunner for WtypeRunner {
    fn type_text(&self, text: &str) -> Result<(), String> {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let argv = wtype_argv();
        let mut child = Command::new(argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            // wtype produces no useful stdout; null both so the child never
            // blocks on an undrained pipe and we add no extra reader threads.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn wtype: {e}"))?;

        // Write the payload verbatim, then drop stdin to signal EOF so wtype
        // stops reading and exits. Take() so the pipe closes here, not at the
        // end of the function.
        match child.stdin.take() {
            Some(mut stdin) => {
                stdin
                    .write_all(text.as_bytes())
                    .map_err(|e| format!("failed to write to wtype stdin: {e}"))?;
                // Explicit drop closes the pipe (EOF) before we wait.
                drop(stdin);
            }
            None => return Err("wtype stdin was not captured".to_owned()),
        }

        // Enforce the timeout without a dependency: a helper thread polls
        // `try_wait()` in a short sleep loop until the child exits or the
        // budget is exhausted, then reports back over a channel. `recv_timeout`
        // is the hard ceiling on how long we block. On timeout we kill the
        // child (the helper holds it, so we signal the poll loop to kill) so a
        // compositor that accepts the connection but never drains keystrokes
        // cannot wedge the consumer.
        let timeout = self.timeout;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let _ = tx.send(Ok(status));
                        return;
                    }
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            // Budget exhausted: kill the child so it cannot
                            // hold the virtual keyboard, then report timeout.
                            let _ = child.kill();
                            let _ = child.wait();
                            let _ = tx.send(Err(WtypeWaitError::Timeout));
                            return;
                        }
                        std::thread::sleep(WTYPE_POLL_INTERVAL);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(WtypeWaitError::Wait(e.to_string())));
                        return;
                    }
                }
            }
        });

        // The helper bounds itself by `timeout`; add a small margin so a
        // shutting-down helper can still deliver its message before we treat
        // the channel as dead.
        match rx.recv_timeout(timeout + WTYPE_POLL_INTERVAL) {
            Ok(Ok(status)) => {
                if status.success() {
                    Ok(())
                } else {
                    Err(format!("wtype exited with status {status}"))
                }
            }
            Ok(Err(WtypeWaitError::Timeout)) => {
                Err(format!("wtype timed out after {}s", timeout.as_secs()))
            }
            Ok(Err(WtypeWaitError::Wait(e))) => Err(format!("failed to wait for wtype: {e}")),
            Err(_recv) => Err("wtype wait thread did not report a result".to_owned()),
        }
    }
}

/// Poll cadence for the dependency-free `wtype` timeout loop. Short enough to
/// keep timeout granularity tight, long enough to avoid a busy-wait.
const WTYPE_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Why the `wtype` wait helper finished without a clean exit status.
enum WtypeWaitError {
    /// The timeout budget elapsed and the child was killed.
    Timeout,
    /// `try_wait` itself errored.
    Wait(String),
}

/// Long-lived typing sink. Cheap to clone; holds an `Arc<dyn CommandRunner>`
/// so tests can swap in a fake runner. Mirrors [`ClipboardSink`]: the
/// synchronous runner work is driven inside `spawn_blocking` so the tokio
/// reactor never stalls on the child process.
#[derive(Clone)]
pub(crate) struct TypeSink {
    runner: Arc<dyn CommandRunner>,
}

impl TypeSink {
    /// Production constructor: a [`WtypeRunner`] bounded by [`WTYPE_TIMEOUT`].
    pub(crate) fn new() -> Self {
        Self {
            runner: Arc::new(WtypeRunner::new(WTYPE_TIMEOUT)),
        }
    }

    /// Test seam: inject a fake [`CommandRunner`].
    #[cfg(test)]
    pub(crate) fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// Type `text` at the cursor. Runs the synchronous runner inside
    /// `spawn_blocking` so the reactor stays responsive; a join panic is
    /// mapped to `Err` so the caller can run the clipboard fallback.
    pub(crate) async fn type_text(&self, text: &str) -> Result<(), String> {
        let runner = Arc::clone(&self.runner);
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || runner.type_text(&text))
            .await
            .map_err(|e| format!("wtype blocking task panicked: {e}"))?
    }
}

/// True if a `wtype` binary is found on `$PATH`. Computed once per process and
/// cached in a [`OnceLock`]: the F6 fallback consults this on every typed
/// delivery, and a `$PATH` scan per job is wasteful. Absent ⇒ the caller runs
/// the clipboard/notify fallback.
///
/// Dependency-free: splits `$PATH` on `':'`, joins `wtype`, and accepts the
/// first candidate that exists and is a file.
pub(crate) fn wtype_present() -> bool {
    static PRESENT: OnceLock<bool> = OnceLock::new();
    *PRESENT.get_or_init(|| {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join("wtype");
            candidate.is_file()
        })
    })
}

/// Advisory desktop-environment hint for F6 log/notification enrichment.
///
/// ADVISORY ONLY — it never gates the typing attempt (we still try `wtype`
/// even on GNOME, because the user may be running a wlroots compositor we did
/// not recognise). It returns a human-readable reason string when the env
/// looks like a non-wlroots session so the fallback notification can explain
/// *why* typing was unavailable.
///
/// Case-insensitive. Both `XDG_CURRENT_DESKTOP` and `XDG_SESSION_DESKTOP` can
/// be colon-separated lists (e.g. `ubuntu:GNOME`), so each is split on `':'`
/// and every token is checked.
pub(crate) fn desktop_hint(
    current_desktop: Option<&str>,
    session_desktop: Option<&str>,
) -> Option<String> {
    // Collect every colon-separated token from both env vars, lowercased.
    let tokens = current_desktop
        .into_iter()
        .chain(session_desktop)
        .flat_map(|value| value.split(':'))
        .map(|token| token.trim().to_ascii_lowercase());

    for token in tokens {
        match token.as_str() {
            "gnome" => {
                return Some(
                    "looks like GNOME — wtype needs a wlroots compositor (Sway/Hyprland)"
                        .to_owned(),
                );
            }
            "kde" | "plasma" | "kwin" => {
                return Some(
                    "looks like KDE/KWin — wtype needs a wlroots compositor (Sway/Hyprland)"
                        .to_owned(),
                );
            }
            // sway | hyprland | empty | anything else: no advisory.
            _ => {}
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn foreground_within_limit_injects() {
        assert_eq!(
            decide_clipboard("foreground", 10, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::Inject
        );
    }

    #[test]
    fn foreground_at_limit_injects() {
        // Boundary: exactly max_bytes is still "fits" (`>` not `>=`).
        assert_eq!(
            decide_clipboard("foreground", CLIPBOARD_MAX_BYTES, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::Inject
        );
    }

    #[test]
    fn foreground_over_limit_notifies() {
        assert_eq!(
            decide_clipboard("foreground", CLIPBOARD_MAX_BYTES + 1, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::NotifyWithAction
        );
    }

    #[test]
    fn detached_within_limit_notifies() {
        // Intent guard: user is not waiting, so never auto-inject.
        assert_eq!(
            decide_clipboard("detached", 10, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::NotifyWithAction
        );
    }

    #[test]
    fn auto_within_limit_notifies() {
        assert_eq!(
            decide_clipboard("auto", 10, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::NotifyWithAction
        );
    }

    #[test]
    fn detached_over_limit_notifies() {
        assert_eq!(
            decide_clipboard("detached", CLIPBOARD_MAX_BYTES + 1, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::NotifyWithAction
        );
    }

    #[test]
    fn auto_over_limit_notifies() {
        assert_eq!(
            decide_clipboard("auto", u64::MAX, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::NotifyWithAction
        );
    }

    #[test]
    fn unknown_mode_within_limit_notifies_defensively() {
        // Forward-compat: a submit_mode this build does not know must
        // never auto-inject. Treat it as "not actively waiting".
        assert_eq!(
            decide_clipboard("future-mode", 10, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::NotifyWithAction
        );
    }

    #[test]
    fn empty_mode_within_limit_notifies_defensively() {
        assert_eq!(
            decide_clipboard("", 10, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::NotifyWithAction
        );
    }

    #[test]
    fn size_guard_beats_foreground_intent() {
        // Even foreground must defer to the size ceiling.
        assert_eq!(
            decide_clipboard("foreground", u64::MAX, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::NotifyWithAction
        );
    }

    #[test]
    fn zero_bytes_foreground_injects() {
        assert_eq!(
            decide_clipboard("foreground", 0, CLIPBOARD_MAX_BYTES),
            ClipboardDecision::Inject
        );
    }

    #[test]
    fn stale_threshold_is_ten_seconds() {
        // Pin the documented secondary-guard value.
        assert_eq!(CLIPBOARD_STALE_THRESHOLD, Duration::from_secs(10));
    }

    // -- Type-at-cursor: decide_type (mirrors the decide_clipboard suite) ---

    #[test]
    fn type_foreground_within_limit_types() {
        assert_eq!(
            decide_type("foreground", 10, TYPE_MAX_BYTES),
            TypeDecision::Type
        );
    }

    #[test]
    fn type_foreground_at_limit_types() {
        // Boundary: exactly max_bytes still fits (`>` not `>=`).
        assert_eq!(
            decide_type("foreground", TYPE_MAX_BYTES, TYPE_MAX_BYTES),
            TypeDecision::Type
        );
    }

    #[test]
    fn type_foreground_over_limit_notifies() {
        assert_eq!(
            decide_type("foreground", TYPE_MAX_BYTES + 1, TYPE_MAX_BYTES),
            TypeDecision::NotifyWithAction
        );
    }

    #[test]
    fn type_detached_within_limit_notifies() {
        assert_eq!(
            decide_type("detached", 10, TYPE_MAX_BYTES),
            TypeDecision::NotifyWithAction
        );
    }

    #[test]
    fn type_auto_within_limit_notifies() {
        assert_eq!(
            decide_type("auto", 10, TYPE_MAX_BYTES),
            TypeDecision::NotifyWithAction
        );
    }

    #[test]
    fn type_unknown_mode_within_limit_notifies_defensively() {
        assert_eq!(
            decide_type("future-mode", 10, TYPE_MAX_BYTES),
            TypeDecision::NotifyWithAction
        );
    }

    #[test]
    fn type_empty_mode_within_limit_notifies_defensively() {
        assert_eq!(
            decide_type("", 10, TYPE_MAX_BYTES),
            TypeDecision::NotifyWithAction
        );
    }

    #[test]
    fn type_zero_bytes_foreground_types() {
        assert_eq!(
            decide_type("foreground", 0, TYPE_MAX_BYTES),
            TypeDecision::Type
        );
    }

    #[test]
    fn type_size_guard_beats_foreground_intent() {
        // Even foreground must defer to the (smaller) type ceiling.
        assert_eq!(
            decide_type("foreground", u64::MAX, TYPE_MAX_BYTES),
            TypeDecision::NotifyWithAction
        );
    }

    // -- Type-at-cursor: argv + consts ------------------------------------

    #[test]
    fn wtype_argv_is_dash_stdin() {
        assert_eq!(wtype_argv(), ["wtype", "-"]);
    }

    #[test]
    fn type_consts_are_pinned() {
        assert_eq!(TYPE_MAX_BYTES, 8_192);
        assert_eq!(WTYPE_KEYSTROKE_DELAY_MS, 0);
        assert_eq!(WTYPE_TIMEOUT, Duration::from_secs(10));
    }

    // -- Type-at-cursor: TypeSink with a fake runner ----------------------

    /// Fake runner that records the exact payload it received and returns a
    /// preconfigured result. Mirrors the production runner's contract.
    struct FakeRunner {
        seen: Arc<Mutex<Option<String>>>,
        result: Result<(), String>,
    }

    impl CommandRunner for FakeRunner {
        fn type_text(&self, text: &str) -> Result<(), String> {
            if let Ok(mut guard) = self.seen.lock() {
                *guard = Some(text.to_owned());
            }
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn type_sink_success_passes_exact_payload() {
        let seen = Arc::new(Mutex::new(None));
        let runner = Arc::new(FakeRunner {
            seen: Arc::clone(&seen),
            result: Ok(()),
        });
        let sink = TypeSink::with_runner(runner);

        // Include newlines and non-ASCII to prove byte-for-byte passthrough
        // (G3: no per-layout pre-transform).
        let payload = "line one\nříšř ž\nthird";
        let outcome = sink.type_text(payload).await;

        assert!(outcome.is_ok());
        assert_eq!(seen.lock().unwrap().as_deref(), Some(payload));
    }

    #[tokio::test]
    async fn type_sink_runner_error_propagates() {
        // Simulate a non-zero exit / timeout from the runner.
        let runner = Arc::new(FakeRunner {
            seen: Arc::new(Mutex::new(None)),
            result: Err("wtype timed out after 10s".to_owned()),
        });
        let sink = TypeSink::with_runner(runner);

        let outcome = sink.type_text("anything").await;
        assert_eq!(outcome, Err("wtype timed out after 10s".to_owned()));
    }

    // -- Type-at-cursor: desktop_hint (advisory only) ---------------------

    #[test]
    fn desktop_hint_ubuntu_gnome_reports_gnome() {
        let hint = desktop_hint(Some("ubuntu:GNOME"), None);
        assert_eq!(
            hint.as_deref(),
            Some("looks like GNOME — wtype needs a wlroots compositor (Sway/Hyprland)")
        );
    }

    #[test]
    fn desktop_hint_kde_reports_kwin() {
        let hint = desktop_hint(Some("KDE"), None);
        assert_eq!(
            hint.as_deref(),
            Some("looks like KDE/KWin — wtype needs a wlroots compositor (Sway/Hyprland)")
        );
    }

    #[test]
    fn desktop_hint_sway_is_none() {
        assert_eq!(desktop_hint(Some("sway"), None), None);
    }

    #[test]
    fn desktop_hint_empty_and_none_is_none() {
        assert_eq!(desktop_hint(None, None), None);
        assert_eq!(desktop_hint(Some(""), Some("")), None);
    }

    #[test]
    fn desktop_hint_is_advisory_returns_option() {
        // Type-level proof: the hint never yields a decision, only an
        // Option<String> the caller may attach to a notification. Checking a
        // wlroots session returns None (no advisory, attempt proceeds).
        let advisory: Option<String> = desktop_hint(Some("Hyprland"), Some("hyprland"));
        assert!(advisory.is_none());
    }
}
