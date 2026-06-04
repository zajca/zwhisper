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

use std::sync::{Arc, Mutex};
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
}
