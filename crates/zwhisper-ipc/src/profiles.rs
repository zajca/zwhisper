//! `Profiles1` D-Bus interface — proxy (client) side.
//!
//! Same architectural choice as `Recorder1`: the proxy trait is the
//! shared contract; the daemon mirrors the method set in its own
//! `#[zbus::interface]` impl in Phase 3.
//!
//! ## Signature reference
//!
//! ```text
//! List() -> a(ssu)            // [(name, description, schema_version)]
//! GetActive() -> s
//! SetActive(s name)
//! Reload()
//! ```
//!
//! ## Semantics (locked-in by Phase 2)
//!
//! - **`List`**: enumerates all loaded profiles. `schema_version` per
//!   `ProfileEntry` is **post-migration** (C12) — always equal to
//!   `CURRENT_SCHEMA_VERSION` for any successfully-loaded profile.
//! - **`GetActive`**: returns the daemon's in-memory "last used"
//!   profile hint. Empty string on first run / after restart (M3
//!   lock-in § 9).
//! - **`SetActive`**: sets the in-memory hint. Per C11, an empty
//!   `name` argument fails with `RpcError::ProfileNotFound { name:
//!   "(empty)" }`.
//! - **`Reload`**: re-scans the profile directory. Profile lookups
//!   already happen on every `StartRecording` (M3 lock-in § 8), so
//!   `Reload` is effectively a no-op cache buster — but it stays in
//!   the API for forward-compat with M4-shaped property notifications.

use crate::types::ProfileEntry;

/// Client-side proxy for the `cz.zajca.Zwhisper1.Profiles1` interface.
#[zbus::proxy(
    interface = "cz.zajca.Zwhisper1.Profiles1",
    default_service = "cz.zajca.Zwhisper1",
    default_path = "/cz/zajca/Zwhisper1",
    gen_blocking = false
)]
pub trait Profiles1 {
    /// Enumerate every profile the daemon currently knows about.
    fn list(&self) -> zbus::Result<Vec<ProfileEntry>>;

    /// Return the in-memory "last used" profile hint. Empty string if
    /// the daemon has not seen a profile this lifetime.
    fn get_active(&self) -> zbus::Result<String>;

    /// Set the in-memory "last used" profile hint. Validates that the
    /// profile exists; empty string is rejected with
    /// `RpcError::ProfileNotFound { name: "(empty)" }`.
    fn set_active(&self, name: &str) -> zbus::Result<()>;

    /// Re-scan the profile directory. Effectively a no-op cache buster
    /// because `StartRecording` reads from disk every time — kept in
    /// the API for forward-compat.
    fn reload(&self) -> zbus::Result<()>;
}
