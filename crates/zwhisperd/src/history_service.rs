//! Server-side `cz.zajca.Zwhisper1.History1` interface (RFC-daemon-role
//! Feature 2).
//!
//! Read + housekeeping surface ships in Phase 2. `Retry` is registered
//! but returns the typed `RpcError::RetryUnavailable` until Phase 4
//! wires it to the model registry (F2.4) — we deliberately do NOT build
//! a second model-resolution path here.

use tracing::info;
use zwhisper_ipc::{HistorySession, RpcError};

use crate::history::HistoryHandle;

/// Hard cap on a single `ListSessions` reply, independent of the
/// caller's `limit`. The index itself is unbounded (JSON is cheap,
/// F2.5); only the display is bounded so a pathological caller cannot
/// marshal a huge array. A caller wanting more pages uses `offset`.
const MAX_LIST_LIMIT: u32 = 500;

#[derive(Debug)]
pub(crate) struct HistoryInterface {
    history: HistoryHandle,
}

impl HistoryInterface {
    pub(crate) fn new(history: HistoryHandle) -> Self {
        Self { history }
    }
}

#[zbus::interface(name = "cz.zajca.Zwhisper1.History1")]
impl HistoryInterface {
    /// Recent entries, most-recent-first. `limit == 0` means "the
    /// daemon's display cap"; any larger value is clamped to it.
    async fn list_sessions(
        &self,
        limit: u32,
        offset: u32,
    ) -> zbus::fdo::Result<Vec<HistorySession>> {
        let effective = if limit == 0 {
            MAX_LIST_LIMIT
        } else {
            limit.min(MAX_LIST_LIMIT)
        };
        let entries = self.history.list(effective, offset).await;
        info!(
            count = entries.len(),
            limit, offset, "History1.ListSessions"
        );
        Ok(entries)
    }

    /// One entry by `session_id`; `SessionUnknown` when absent.
    async fn get_session(&self, id: &str) -> zbus::fdo::Result<HistorySession> {
        self.history
            .get(id)
            .await
            .ok_or_else(|| RpcError::SessionUnknown { id: id.to_owned() }.into())
    }

    /// Re-transcribe a session from its FLAC. Gated on the audio RFC
    /// (F2.4): returns the most informative typed error — `SessionUnknown`
    /// when the entry is gone, `AudioNotFound` when the FLAC was deleted,
    /// otherwise `RetryUnavailable` until Phase 4.
    async fn retry(&self, id: &str) -> zbus::fdo::Result<String> {
        info!(%id, "History1.Retry");
        let Some(entry) = self.history.get(id).await else {
            return Err(RpcError::SessionUnknown { id: id.to_owned() }.into());
        };
        if entry.audio_path.is_empty() || !std::path::Path::new(&entry.audio_path).exists() {
            return Err(RpcError::AudioNotFound {
                path: entry.audio_path,
            }
            .into());
        }
        Err(RpcError::RetryUnavailable.into())
    }

    /// Drop the index entry and, only when `delete_files`, the
    /// referenced files (F2.5). Idempotent: forgetting an absent id is
    /// `Ok`.
    async fn forget(&self, id: &str, delete_files: bool) -> zbus::fdo::Result<()> {
        info!(%id, delete_files, "History1.Forget");
        self.history
            .forget(id, delete_files)
            .await
            .map_err(zbus::fdo::Error::from)
    }

    /// Per-interface protocol version (F4.1).
    #[zbus(property)]
    #[allow(clippy::unused_self, reason = "zbus property handlers must take &self")]
    fn protocol_version(&self) -> &'static str {
        zwhisper_ipc::PROTOCOL_VERSION
    }
}
