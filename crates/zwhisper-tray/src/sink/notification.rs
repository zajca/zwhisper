//! Desktop notification sink — non-blocking show via `notify-rust`.
//!
//! ## `DoD` #23 — non-blocking notifications
//!
//! We do NOT use `Notification::wait_for_action`: it spawns a
//! blocking thread per notification that lives until the user clicks
//! or the notification expires, which on a busy session leaks
//! threads. Instead we call `show_async()` (or `show()` inside
//! `spawn_blocking`) and drop the returned `NotificationHandle`
//! immediately.
//!
//! For M4 we deliberately do NOT register a per-notification
//! `ActionInvoked` listener — the body always contains the absolute
//! transcript path so the user can copy-paste it manually if their
//! desktop's notification center stripped the action button. A
//! global `ActionInvoked` listener that calls `xdg-open` is left for
//! M5+ once we wire `OpenLastTranscript` through the menu (cf.
//! `cmd::run_dispatcher`).

use std::path::PathBuf;

use async_trait::async_trait;

use crate::sink::{Sink, SinkContext, SinkError};

/// Desktop notification sink. Stable across notifications so the
/// notification daemon can group by `appname`.
#[derive(Debug, Clone)]
pub struct NotificationSink {
    /// Application name used in the `org.freedesktop.Notifications`
    /// API.
    app_name: String,
}

impl NotificationSink {
    pub fn new() -> Self {
        Self {
            app_name: "zwhisper".to_owned(),
        }
    }
}

impl Default for NotificationSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Sink for NotificationSink {
    fn id(&self) -> &'static str {
        "notification"
    }

    async fn deliver(&self, ctx: &SinkContext<'_>) -> Result<(), SinkError> {
        // Body composition: cover the four cases (clipboard ok,
        // clipboard failed, clipboard too large, transcript missing).
        let body = build_body(ctx);
        let app = self.app_name.clone();
        // `path` is captured for logging only; once we add the
        // global ActionInvoked listener it becomes the
        // open-target.
        let path: PathBuf = ctx.transcript_path.to_path_buf();

        tokio::task::spawn_blocking(move || -> Result<(), notify_rust::error::Error> {
            notify_rust::Notification::new()
                .appname(&app)
                .summary("Transcript ready")
                .body(&body)
                .icon("zwhisper-idle")
                .timeout(notify_rust::Timeout::Default)
                .show()?;
            // Do NOT call wait_for_action / wait_for_close — `DoD` #23.
            // The handle drops here; on KDE/GNOME this is fine.
            let _ = path;
            Ok(())
        })
        .await
        .map_err(|e| SinkError::Notification(format!("blocking task panicked: {e}")))?
        .map_err(|e| SinkError::Notification(e.to_string()))?;
        Ok(())
    }
}

/// Pure helper used by both the sink and the dispatcher's tests.
///
/// Branches mirror the four `SinkRunPlan` variants in
/// `super::dispatch`.
pub fn build_body(ctx: &SinkContext<'_>) -> String {
    if ctx.clipboard_skipped_too_large {
        format!(
            "Transcript too large for clipboard ({} bytes). Open file: {}",
            ctx.bytes,
            ctx.transcript_path.display()
        )
    } else if ctx.clipboard_failed {
        format!(
            "Clipboard unavailable; transcript saved at: {}",
            ctx.transcript_path.display()
        )
    } else {
        format!(
            "Transcript copied to clipboard ({} bytes). File: {}",
            ctx.bytes,
            ctx.transcript_path.display()
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
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
    fn id_is_stable() {
        assert_eq!(NotificationSink::new().id(), "notification");
    }

    #[test]
    fn body_for_run_both_mentions_clipboard_success() {
        let path = Path::new("/tmp/t.txt");
        let body = build_body(&ctx(path, "hi", 12, false, false));
        assert!(body.contains("copied to clipboard"));
        assert!(body.contains("/tmp/t.txt"));
        assert!(body.contains("12"));
    }

    #[test]
    fn body_for_too_large_mentions_too_large() {
        let path = Path::new("/tmp/big.txt");
        let body = build_body(&ctx(path, "ignored", 999_999, false, true));
        assert!(body.contains("too large"));
        assert!(body.contains("/tmp/big.txt"));
    }

    #[test]
    fn body_for_clipboard_failed_mentions_unavailable() {
        let path = Path::new("/tmp/oops.txt");
        let body = build_body(&ctx(path, "x", 5, true, false));
        assert!(body.contains("Clipboard unavailable"));
        assert!(body.contains("/tmp/oops.txt"));
    }
}
