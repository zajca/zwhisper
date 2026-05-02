//! Clipboard sink — wraps a long-lived `arboard::Clipboard` handle.
//!
//! ## Binding amendment C1 (M4-plan)
//!
//! `arboard::Clipboard` MUST be created once and held for the tray's
//! lifetime. Otherwise the Wayland selection dies the instant the
//! object drops, and any subsequent paste yields empty content. We
//! lazy-init on the first successful delivery and keep the handle in
//! `Arc<Mutex<Option<...>>>` so it can outlive each `spawn_blocking`
//! call.
//!
//! ## Threading
//!
//! `arboard` is synchronous; we run all clipboard interactions inside
//! `tokio::task::spawn_blocking` so the tokio worker thread is never
//! blocked by an X11 / Wayland round-trip.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::sink::{Sink, SinkContext, SinkError};

/// Long-lived clipboard sink. Cheap to construct; lazily opens the
/// underlying clipboard on first successful delivery.
///
/// `arboard::Clipboard` does not implement `Debug`, so we provide a
/// hand-rolled `Debug` impl (below) that just prints the type name.
#[derive(Clone)]
pub struct ClipboardSink {
    /// Held alive for the entire tray process lifetime. Lazy-init
    /// to avoid failing the dispatcher startup when no compositor
    /// clipboard is available (e.g. headless CI). Using `std::sync`
    /// here on purpose: the lock is only held inside
    /// `spawn_blocking`, never across `.await`.
    clipboard: Arc<Mutex<Option<arboard::Clipboard>>>,
}

impl ClipboardSink {
    pub fn new() -> Self {
        Self {
            clipboard: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for ClipboardSink {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ClipboardSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // We deliberately do NOT print the inner clipboard handle —
        // it has no Debug impl and even if it did, its contents are
        // user data we don't want to leak through tracing.
        f.debug_struct("ClipboardSink")
            .field(
                "initialised",
                &self.clipboard.lock().is_ok_and(|g| g.is_some()),
            )
            .finish()
    }
}

#[async_trait]
impl Sink for ClipboardSink {
    fn id(&self) -> &'static str {
        "clipboard"
    }

    async fn deliver(&self, ctx: &SinkContext<'_>) -> Result<(), SinkError> {
        if ctx.clipboard_skipped_too_large {
            // Caller already decided to skip; the trait was still
            // invoked to keep the dispatcher's iteration uniform.
            // This branch is informational.
            return Ok(());
        }
        let clipboard = Arc::clone(&self.clipboard);
        let text = ctx.transcript_text.to_owned();
        tokio::task::spawn_blocking(move || -> Result<(), arboard::Error> {
            let mut guard = clipboard.lock().map_err(|e| arboard::Error::Unknown {
                description: format!("clipboard mutex poisoned: {e}"),
            })?;
            if guard.is_none() {
                *guard = Some(arboard::Clipboard::new()?);
            }
            // The `if let` keeps clippy happy compared to
            // `unwrap()`; we just inserted `Some(_)` above when
            // empty so the branch is always taken.
            if let Some(cb) = guard.as_mut() {
                cb.set_text(text)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| SinkError::Clipboard(format!("blocking task panicked: {e}")))?
        .map_err(|e| SinkError::Clipboard(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    // NOTE: The C1 invariant ("Clipboard handle stays alive for the
    // tray's whole life so paste-after-5-seconds still yields the
    // text") cannot be tested without a real Wayland compositor.
    // Phase P7 will document the manual paste-after-5s verification
    // step in `docs/M4-verification.md`.
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn clipboard_sink_skipped_too_large_returns_ok() {
        let sink = ClipboardSink::new();
        let path = Path::new("/tmp/zwhisper-fake.txt");
        let ctx = SinkContext {
            session_id: "sess-1",
            transcript_path: path,
            transcript_text: "ignored",
            bytes: 1_000_000,
            backend: "whisper-cli",
            clipboard_failed: false,
            clipboard_skipped_too_large: true,
        };
        // Must NOT touch the real clipboard — should be a no-op
        // even on a host without a compositor.
        sink.deliver(&ctx).await.unwrap();
    }

    #[test]
    fn id_is_stable() {
        assert_eq!(ClipboardSink::new().id(), "clipboard");
    }
}
