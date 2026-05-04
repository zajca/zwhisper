//! Desktop notification sink — non-blocking show + async action listener.
//!
//! ## `DoD` #5 + `DoD` #23
//!
//! `DoD` #5 mandates a clickable "Open in editor" action mapping to
//! `xdg-open <transcript_path>`. `DoD` #23 mandates that this MUST
//! NOT use `Notification::wait_for_action` (which spawns a blocking
//! thread per notification and accumulates them on a busy session,
//! quickly exhausting tokio's `spawn_blocking` pool).
//!
//! The implementation that satisfies both invariants:
//!
//! 1. Build the notification with `.action("open-in-editor", "Open
//!    in editor")` and `.show_async().await` against the zbus tokio
//!    backend (workspace dep `notify-rust = { features =
//!    ["z-with-tokio"] }`). `show_async` returns a
//!    `NotificationHandle` whose `wait_for_action_async` is a
//!    regular tokio future — no blocking thread.
//! 2. Spawn one tokio task per notification that awaits
//!    `wait_for_action_async`. The task captures the absolute
//!    transcript path; on `Custom("open-in-editor")` it spawns
//!    `xdg-open <path>` (detached). On `Closed(_)` (timeout / user
//!    dismiss) the task ends and is dropped. Tokio tasks are cheap
//!    (~few KB each) — no pool exhaustion even on a busy session.
//!
//! The body still carries the absolute transcript path so users on
//! desktops that strip action buttons (older XFCE, some embedded
//! shells) can copy-paste manually.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use notify_rust::ActionResponse;
use tokio::process::Command;

use crate::sink::{Sink, SinkContext, SinkError};

/// Action key sent over D-Bus when the user clicks the action button
/// on a transcript notification. Stable so the listener task can
/// match it by string equality without keeping a separate id map.
const TRANSCRIPT_ACTION_KEY: &str = "open-in-editor";

/// Action label rendered by the notification daemon next to the
/// notification body.
const TRANSCRIPT_ACTION_LABEL: &str = "Open in editor";

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
        let body = build_body(ctx);
        let app = self.app_name.clone();
        let path: PathBuf = ctx.transcript_path.to_path_buf();

        let handle = notify_rust::Notification::new()
            .appname(&app)
            .summary("Transcript ready")
            .body(&body)
            .icon("zwhisper-idle")
            .action(TRANSCRIPT_ACTION_KEY, TRANSCRIPT_ACTION_LABEL)
            .timeout(notify_rust::Timeout::Default)
            .show_async()
            .await
            .map_err(|e| SinkError::Notification(e.to_string()))?;

        // Detached listener task — bounded by either user action or
        // notification expiry. Tokio tasks are cheap, so accumulating
        // a few while several notifications are open at once is
        // fine; `wait_for_action_async` returns the moment the
        // server emits `ActionInvoked` or `NotificationClosed`.
        // We do NOT `await` this task — that would defeat the
        // non-blocking property we are trying to preserve.
        tokio::spawn(listen_for_action(handle, path));
        Ok(())
    }
}

async fn listen_for_action(handle: notify_rust::NotificationHandle, transcript_path: PathBuf) {
    handle
        .wait_for_action_async(|action| match action {
            ActionResponse::Custom(key) if *key == TRANSCRIPT_ACTION_KEY => {
                spawn_xdg_open(transcript_path.clone());
            }
            ActionResponse::Custom(other) => {
                tracing::debug!(action = %other, "notification action ignored");
            }
            ActionResponse::Closed(_) => {
                // Notification expired or user dismissed it. No
                // action to take; just let the task drop.
            }
        })
        .await;
}

fn spawn_xdg_open(path: PathBuf) {
    tokio::spawn(async move {
        let display_path = path.display().to_string();
        let mut cmd = Command::new("xdg-open");
        cmd.arg(&path).stdout(Stdio::null()).stderr(Stdio::null());
        match cmd.spawn() {
            Ok(mut child) => match child.wait().await {
                Ok(status) if status.success() => {
                    tracing::info!(path = %display_path, "transcript opened via xdg-open");
                }
                Ok(status) => {
                    tracing::warn!(
                        path = %display_path,
                        code = ?status.code(),
                        "xdg-open exited non-zero",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        path = %display_path,
                        error = %e,
                        "could not await xdg-open child",
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    path = %display_path,
                    error = %e,
                    "could not spawn xdg-open (is it installed?)",
                );
            }
        }
    });
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
