//! Sink dispatcher (Task C in the M4 threading model).
//!
//! Receives a [`TranscriptJob`] from the pump on `TranscriptComplete`,
//! reads the transcript file from disk, applies the size guard
//! (`DoD` #19), and runs the clipboard + notification sinks in order.
//! Sink failures are logged but never abort the iteration — the two
//! sinks are independent (see M4-plan § "Sink invocation order and
//! atomicity").

use std::path::PathBuf;

use color_eyre::eyre::Result;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::sink::clipboard::ClipboardSink;
use crate::sink::notification::NotificationSink;
use crate::sink::{Sink, SinkContext};

/// Job sent by the pump on `TranscriptComplete`. Owns its strings
/// because the signal arguments borrow from a transient zbus
/// message.
#[derive(Debug, Clone)]
pub struct TranscriptJob {
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub bytes: u64,
    pub backend: String,
}

// `DEFAULT_CLIPBOARD_MAX_BYTES` lives in `crate::config` (per CLAUDE.md
// "all configuration in a dedicated module"). Re-exported here so
// existing callers keep their import paths.
pub use crate::config::DEFAULT_CLIPBOARD_MAX_BYTES;

/// Decision tree applied to a single `TranscriptJob` once we know
/// the file size and whether the file is readable.
///
/// Pure-function decision so tests don't need a real file system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkRunPlan {
    /// Clipboard + notify.
    RunBoth,
    /// Notify-only with "too large" body (`DoD` #19).
    SkipClipboardTooLarge,
    /// Notify-only with "deleted" body (M6 hardening adjacent).
    SkipClipboardMissing,
    /// Notify-only with generic read-error body.
    SkipClipboardReadError,
}

/// Classify the run plan from the byte count and an optional
/// `read_error.kind()`.
pub fn classify_run(
    bytes: u64,
    max_bytes: u64,
    read_error: Option<&std::io::ErrorKind>,
) -> SinkRunPlan {
    match read_error {
        Some(std::io::ErrorKind::NotFound) => SinkRunPlan::SkipClipboardMissing,
        Some(_) => SinkRunPlan::SkipClipboardReadError,
        None if bytes > max_bytes => SinkRunPlan::SkipClipboardTooLarge,
        None => SinkRunPlan::RunBoth,
    }
}

/// Build the notification body for branches where the file could
/// not be read. Kept separate from `notification::build_body`
/// because the latter assumes a successful read.
pub fn build_unreadable_body(plan: SinkRunPlan, path: &std::path::Path) -> String {
    match plan {
        SinkRunPlan::SkipClipboardMissing => format!(
            "Transcript file was deleted before it could be copied to clipboard. File: {}",
            path.display()
        ),
        SinkRunPlan::SkipClipboardReadError => {
            format!("Could not read transcript file: {}", path.display())
        }
        // The other variants imply a successful read; callers
        // should not hit this branch.
        SinkRunPlan::RunBoth | SinkRunPlan::SkipClipboardTooLarge => {
            format!("Transcript ready: {}", path.display())
        }
    }
}

/// Run the dispatcher loop until `shutdown_rx` fires.
///
/// Sinks are passed in by value so the dispatcher owns them for the
/// process's whole lifetime — that's the C1 invariant for the
/// clipboard handle.
pub async fn run_dispatcher(
    mut job_rx: mpsc::Receiver<TranscriptJob>,
    clipboard_sink: ClipboardSink,
    notification_sink: NotificationSink,
    clipboard_max_bytes: u64,
    mut shutdown_rx: watch::Receiver<()>,
) -> Result<()> {
    info!(max_bytes = clipboard_max_bytes, "sink dispatcher started");
    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                info!("sink dispatcher shutting down");
                return Ok(());
            }
            maybe_job = job_rx.recv() => {
                let Some(job) = maybe_job else {
                    info!("sink dispatcher: job channel closed");
                    return Ok(());
                };
                handle_job(
                    job,
                    &clipboard_sink,
                    &notification_sink,
                    clipboard_max_bytes,
                )
                .await;
            }
        }
    }
}

async fn handle_job(
    job: TranscriptJob,
    clipboard_sink: &ClipboardSink,
    notification_sink: &NotificationSink,
    clipboard_max_bytes: u64,
) {
    // 1. Read transcript text from disk.
    let read_result = tokio::fs::read_to_string(&job.transcript_path).await;
    let read_error_kind = read_result.as_ref().err().map(std::io::Error::kind);
    let plan = classify_run(job.bytes, clipboard_max_bytes, read_error_kind.as_ref());
    if let Err(ref e) = read_result {
        warn!(
            error = %e,
            path = %job.transcript_path.display(),
            session_id = %job.session_id,
            "transcript read failed; clipboard skipped",
        );
    }
    let transcript_text = read_result.unwrap_or_default();

    // 2. Run the clipboard sink, when applicable, and track its
    //    outcome so the notification body can reflect it.
    let mut clipboard_failed = false;
    let mut clipboard_skipped_too_large = false;

    match plan {
        SinkRunPlan::RunBoth => {
            let ctx = SinkContext {
                session_id: &job.session_id,
                transcript_path: &job.transcript_path,
                transcript_text: &transcript_text,
                bytes: job.bytes,
                backend: &job.backend,
                clipboard_failed: false,
                clipboard_skipped_too_large: false,
            };
            if let Err(e) = clipboard_sink.deliver(&ctx).await {
                warn!(error = %e, sink = clipboard_sink.id(), "clipboard sink failed");
                clipboard_failed = true;
            } else {
                info!(
                    sink = clipboard_sink.id(),
                    bytes = job.bytes,
                    "clipboard sink delivered",
                );
            }
        }
        SinkRunPlan::SkipClipboardTooLarge => {
            clipboard_skipped_too_large = true;
            info!(
                bytes = job.bytes,
                max_bytes = clipboard_max_bytes,
                "transcript exceeds clipboard size guard; skipping clipboard sink",
            );
        }
        SinkRunPlan::SkipClipboardMissing | SinkRunPlan::SkipClipboardReadError => {
            // Clipboard cannot run; the override notification path
            // below builds a more specific body.
            clipboard_failed = true;
        }
    }

    // 3. Notification.
    match plan {
        SinkRunPlan::SkipClipboardMissing | SinkRunPlan::SkipClipboardReadError => {
            // Override body so the user sees "deleted" / "could not
            // read" instead of the generic "clipboard unavailable"
            // text. Bypass the sink's own body composer rather than
            // teach `SinkContext` a fourth state.
            let body = build_unreadable_body(plan, &job.transcript_path);
            deliver_notification_with_body(&body).await;
            info!(
                sink = notification_sink.id(),
                plan = ?plan,
                "notification sink delivered (unreadable override)",
            );
        }
        SinkRunPlan::RunBoth | SinkRunPlan::SkipClipboardTooLarge => {
            let ctx = SinkContext {
                session_id: &job.session_id,
                transcript_path: &job.transcript_path,
                transcript_text: &transcript_text,
                bytes: job.bytes,
                backend: &job.backend,
                clipboard_failed,
                clipboard_skipped_too_large,
            };
            if let Err(e) = notification_sink.deliver(&ctx).await {
                warn!(error = %e, sink = notification_sink.id(), "notification sink failed");
            } else {
                info!(
                    sink = notification_sink.id(),
                    plan = ?plan,
                    "notification sink delivered",
                );
            }
        }
    }
}

/// Show a notification with a hand-composed body. Used for the
/// unreadable-file branches where the sink's own composer would
/// emit misleading text.
async fn deliver_notification_with_body(body: &str) {
    let body = body.to_owned();
    let app = "zwhisper".to_owned();
    let join = tokio::task::spawn_blocking(move || {
        notify_rust::Notification::new()
            .appname(&app)
            .summary("Transcript ready")
            .body(&body)
            .icon("zwhisper-idle")
            .timeout(notify_rust::Timeout::Default)
            .show()
            .map(|_| ())
    })
    .await;
    match join {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(error = %e, "notification (unreadable override) failed"),
        Err(e) => warn!(error = %e, "notification blocking task panicked"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sink::notification::build_body;
    use std::io::ErrorKind;
    use std::path::Path;

    fn ctx<'a>(
        path: &'a Path,
        text: &'a str,
        bytes: u64,
        clipboard_failed: bool,
        clipboard_skipped_too_large: bool,
    ) -> SinkContext<'a> {
        SinkContext {
            session_id: "sess-x",
            transcript_path: path,
            transcript_text: text,
            bytes,
            backend: "whisper-cli",
            clipboard_failed,
            clipboard_skipped_too_large,
        }
    }

    #[test]
    fn classify_run_under_limit_runs_both() {
        let plan = classify_run(100, 1024, None);
        assert_eq!(plan, SinkRunPlan::RunBoth);
    }

    #[test]
    fn classify_run_at_limit_runs_both() {
        let plan = classify_run(1024, 1024, None);
        assert_eq!(plan, SinkRunPlan::RunBoth);
    }

    #[test]
    fn classify_run_over_limit_skips_clipboard() {
        let plan = classify_run(1025, 1024, None);
        assert_eq!(plan, SinkRunPlan::SkipClipboardTooLarge);
    }

    #[test]
    fn classify_run_missing_file_skips_clipboard() {
        let plan = classify_run(0, 1024, Some(&ErrorKind::NotFound));
        assert_eq!(plan, SinkRunPlan::SkipClipboardMissing);
    }

    #[test]
    fn classify_run_other_io_error_skips_clipboard() {
        let plan = classify_run(0, 1024, Some(&ErrorKind::PermissionDenied));
        assert_eq!(plan, SinkRunPlan::SkipClipboardReadError);
    }

    #[test]
    fn classify_run_io_error_takes_priority_over_size() {
        let plan = classify_run(u64::MAX, 1024, Some(&ErrorKind::PermissionDenied));
        assert_eq!(plan, SinkRunPlan::SkipClipboardReadError);
    }

    #[test]
    fn body_for_run_both_mentions_clipboard_success() {
        let path = Path::new("/tmp/t.txt");
        let body = build_body(&ctx(path, "hi", 12, false, false));
        assert!(body.contains("copied to clipboard"));
        assert!(body.contains("/tmp/t.txt"));
    }

    #[test]
    fn body_for_too_large_mentions_too_large() {
        let path = Path::new("/tmp/big.txt");
        let body = build_body(&ctx(path, "ignored", 999_999, false, true));
        assert!(body.contains("too large"));
    }

    #[test]
    fn body_for_clipboard_failed_mentions_unavailable() {
        let path = Path::new("/tmp/oops.txt");
        let body = build_body(&ctx(path, "x", 5, true, false));
        assert!(body.contains("Clipboard unavailable"));
    }

    #[test]
    fn body_for_missing_mentions_deleted() {
        let path = Path::new("/tmp/gone.txt");
        let body = build_unreadable_body(SinkRunPlan::SkipClipboardMissing, path);
        assert!(body.contains("deleted"));
        assert!(body.contains("/tmp/gone.txt"));
    }

    #[test]
    fn body_for_read_error_mentions_could_not_read() {
        let path = Path::new("/tmp/locked.txt");
        let body = build_unreadable_body(SinkRunPlan::SkipClipboardReadError, path);
        assert!(body.contains("Could not read"));
    }
}
