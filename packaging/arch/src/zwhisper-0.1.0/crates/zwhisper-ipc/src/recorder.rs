//! `Recorder1` D-Bus interface — proxy (client) side.
//!
//! The server-side `#[zbus::interface]` impl lives in `zwhisperd`
//! (Phase 3). Phase 2 ships only the proxy trait so the contract is
//! pinned in one place.
//!
//! **Why proxy-only, not a shared trait:** zbus 5.15's `#[interface]`
//! macro requires a real `impl` block on a server-owned struct that
//! holds its state — it cannot decorate a free `trait` definition the
//! way `#[proxy]` can. The recommended pattern (per the zbus 5.15
//! `attr.proxy` and `attr.interface` docs) is to keep the proxy trait
//! in the shared crate and let the daemon mirror the method set
//! verbatim in its own `#[interface]` block. Phase 3 owns that mirror.
//!
//! The constants below are the canonical strings the daemon must use
//! for its `#[interface(name = …)]` attribute and for the names of
//! signal handlers — keeping the proxy side and the server side in
//! lock-step.
//!
//! ## Lifecycle ordering (locked-in by C5 + C9)
//!
//! 1. `StartRecording` -> daemon emits `StateChanged "starting"` then
//!    `StateChanged "recording"`, returns `session_id` to the caller.
//! 2. On stop / EOS: daemon emits `RecordingComplete{ session_id,
//!    audio_path }`, releases the session slot, then runs the
//!    transcribe step.
//! 3. After transcribe: daemon emits `TranscriptComplete{ session_id,
//!    transcript_path, bytes, backend }`, then a terminal
//!    `StateChanged "idle"` (or `"failed"` on transcribe failure).
//!
//! `RecordingComplete` is always emitted strictly before the terminal
//! `StateChanged "idle"` for the same `session_id`. Phase 5 has a
//! signal-ordering test (`recording_complete_arrives_before_state_changed_idle`)
//! that locks this in.
//!
//! ## Signature reference
//!
//! ```text
//! StartRecording(s profile_name) -> (s session_id)
//! StopRecording (s session_id)   -> (s session_id)
//! GetStatus()                    -> (s state, s active_profile, t duration_ms)   // (sst)
//! StateChanged       (s new_state, s session_id)
//! RecordingComplete  (s session_id, s audio_path)
//! TranscriptComplete (s session_id, s transcript_path, t bytes, s backend)
//! ```

use crate::types::Status;

/// Client-side proxy for the `cz.zajca.Zwhisper1.Recorder1` interface.
///
/// `gen_blocking = false` keeps the async-only API: zwhisper-cli runs
/// on tokio and we do not need the blocking variant. The expanded
/// proxy struct is `Recorder1Proxy<'_>`.
#[zbus::proxy(
    interface = "cz.zajca.Zwhisper1.Recorder1",
    default_service = "cz.zajca.Zwhisper1",
    default_path = "/cz/zajca/Zwhisper1",
    gen_blocking = false
)]
pub trait Recorder1 {
    /// Start a new recording session against the named profile.
    /// Returns the freshly-minted `session_id` (UUID v4, hyphenated).
    /// Fails with `RpcError::SessionInUse` if a session is already
    /// active, or `RpcError::ProfileNotFound` / `ProfileLoadFailed`
    /// when the profile is missing or invalid.
    fn start_recording(&self, profile_name: &str) -> zbus::Result<String>;

    /// Stop the recording with the given `session_id`. Returns the same
    /// `session_id` on success (handy for chained CLI flows). Fails
    /// with `RpcError::SessionUnknown` when the id does not match the
    /// active session.
    fn stop_recording(&self, session_id: &str) -> zbus::Result<String>;

    /// Snapshot of the daemon's current state. Wire signature `(sst)`.
    fn get_status(&self) -> zbus::Result<Status>;

    /// State transition notification. The first argument is one of
    /// `idle | starting | recording | stopping | failed` (mirrors
    /// `RecorderState::Display` in `zwhisper-core::audio::state`).
    #[zbus(signal)]
    fn state_changed(&self, new_state: &str, session_id: &str) -> zbus::Result<()>;

    /// Emitted when the audio file has been written to disk and the
    /// session slot has been released. The transcribe step begins
    /// after this signal fires.
    #[zbus(signal)]
    fn recording_complete(&self, session_id: &str, audio_path: &str) -> zbus::Result<()>;

    /// Emitted after the post-process pipeline finishes successfully.
    /// `bytes` is the size of the transcript file in bytes; `backend`
    /// is a free-form identifier (e.g. `"whisper-cli"`).
    #[zbus(signal)]
    fn transcript_complete(
        &self,
        session_id: &str,
        transcript_path: &str,
        bytes: u64,
        backend: &str,
    ) -> zbus::Result<()>;

    /// Read-only property: workspace-wide protocol version
    /// (M8 DoD #11). Wire signature `s`. Equal to
    /// [`crate::PROTOCOL_VERSION`] on a daemon built from the same
    /// workspace as the client. Mismatched values trip the M8
    /// pre-flight handshake and the client refuses to make further
    /// RPCs.
    ///
    /// Pre-0.1.0 daemons do not implement this property; zbus
    /// surfaces the call as `MethodCallNotImplemented` /
    /// `UnknownProperty`, which the client maps to
    /// [`crate::ProtocolMismatch::legacy_daemon`].
    #[zbus(property)]
    fn protocol_version(&self) -> zbus::Result<String>;
}
