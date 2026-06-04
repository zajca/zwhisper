//! `History1` D-Bus interface — proxy (client) side.
//!
//! RFC-daemon-role Feature 2. Durable session history + retry, owned by
//! the daemon (the single writer). The persisted FLAC remains the real
//! source of truth; the index is a rebuildable, queryable cache. The
//! server-side `#[zbus::interface]` impl lives in
//! `zwhisperd::history_service`.
//!
//! ## Signature reference
//!
//! ```text
//! ListSessions(u limit, u offset) -> a(stssssssss)
//! GetSession(s id)                -> (stssssssss)
//! Retry(s id)                     -> (s job_id)
//! Forget(s id, b delete_files)    -> ()
//! property ProtocolVersion -> s
//! ```
//!
//! ## `Retry` is gated on the audio RFC (F2.4)
//!
//! `Retry` is registered from Phase 2 but returns the typed
//! `RpcError::RetryUnavailable` until Phase 4 wires it to the model
//! registry. This keeps history queryable immediately without building
//! a second model-resolution path. `GetSession` / `Retry` over an entry
//! whose `audio_path` is gone returns `RpcError::AudioNotFound`.

use crate::types::HistorySession;

/// Client-side proxy for the `cz.zajca.Zwhisper1.History1` interface.
#[zbus::proxy(
    interface = "cz.zajca.Zwhisper1.History1",
    default_service = "cz.zajca.Zwhisper1",
    default_path = "/cz/zajca/Zwhisper1",
    gen_blocking = false
)]
pub trait History1 {
    /// Recent entries, most-recent-first, sliced by `offset`/`limit`.
    /// `limit == 0` is treated as "no extra cap beyond the daemon's
    /// own display bound".
    fn list_sessions(&self, limit: u32, offset: u32) -> zbus::Result<Vec<HistorySession>>;

    /// A single entry by `session_id`. Fails with
    /// `RpcError::SessionUnknown` when the id is absent.
    fn get_session(&self, id: &str) -> zbus::Result<HistorySession>;

    /// Re-transcribe a session from its persisted FLAC, returning the
    /// new `job_id`. Returns `RpcError::RetryUnavailable` until Phase 4
    /// (F2.4); `RpcError::AudioNotFound` when the FLAC is gone.
    fn retry(&self, id: &str) -> zbus::Result<String>;

    /// Drop the index entry and, only when `delete_files`, the
    /// referenced audio/transcript files (F2.5). User audio is never
    /// deleted unless `delete_files` is explicitly set.
    fn forget(&self, id: &str, delete_files: bool) -> zbus::Result<()>;

    /// Read-only per-interface protocol version (F4.1).
    #[zbus(property)]
    fn protocol_version(&self) -> zbus::Result<String>;
}
