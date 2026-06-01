//! Session-bound sinks: clipboard + notifications.
//!
//! Per IDEA.md § 5, these sinks live in the tray process (not the
//! daemon) because they need an active graphical session
//! (`WAYLAND_DISPLAY`, notification bus). Sinks fire ONLY
//! on `TranscriptComplete` (`DoD` item 8). `RecordingComplete` is
//! informational only — it bumps state, it does NOT trigger sinks.

use async_trait::async_trait;
use std::path::Path;

pub mod clipboard;
pub mod dispatch;
pub mod notification;

/// A session-bound delivery target for a finished transcript.
///
/// Implementations are expected to be cheap to construct, hold any
/// long-lived OS handles internally (e.g. `arboard::Clipboard` per
/// binding amendment C1), and to be safe to call concurrently with
/// other sinks. Failure in one sink MUST NOT abort the others —
/// the dispatcher applies that policy.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Stable identifier used in tracing / metrics. Keep this short
    /// and lower-case (`"clipboard"`, `"notification"`).
    fn id(&self) -> &'static str;

    /// Deliver the transcript described by `ctx`. Returns `Ok(())`
    /// on success or a [`SinkError`] on failure. The dispatcher
    /// records errors but continues with the remaining sinks.
    async fn deliver(&self, ctx: &SinkContext<'_>) -> Result<(), SinkError>;
}

/// Context payload passed to every sink. Borrows the dispatcher's
/// owned data so we don't allocate per-sink copies.
#[derive(Debug)]
pub struct SinkContext<'a> {
    pub session_id: &'a str,
    pub transcript_path: &'a Path,
    pub transcript_text: &'a str,
    pub bytes: u64,
    pub backend: &'a str,
    /// True when the clipboard sink ran AHEAD of this sink and
    /// returned an error. The notification sink reads this to mutate
    /// its body ("Clipboard unavailable, transcript at `<path>`").
    pub clipboard_failed: bool,
    /// True when the dispatcher decided the transcript is too big
    /// for the clipboard (`DoD` #19); the notification sink also reads
    /// this so the body reflects "Transcript too large" instead of
    /// "Transcript ready".
    pub clipboard_skipped_too_large: bool,
}

/// Error returned from a single sink delivery attempt.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("clipboard error: {0}")]
    Clipboard(String),
    #[error("notification error: {0}")]
    Notification(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
