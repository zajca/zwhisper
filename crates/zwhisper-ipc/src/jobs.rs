//! `Jobs1` D-Bus interface — proxy (client) side.
//!
//! RFC-daemon-role Feature 1. A NEW interface, deliberately **not**
//! folded onto `Recorder1`: recording is a single-slot resource;
//! transcription is a multi-item queue with different lifetimes and
//! concurrency. The server-side `#[zbus::interface]` impl lives in
//! `zwhisperd::jobs_service` and mirrors this method/signal set
//! verbatim (same architectural split as `Recorder1`, see
//! `crate::recorder`).
//!
//! ## Signature reference
//!
//! ```text
//! TranscribeFile(s path, s backend, s model, s lang) -> (s job_id)
//! Cancel(s job_id) -> ()
//! ListJobs() -> a(ssst)        // [(job_id, state, label, submitted_ms)]
//! property ProtocolVersion -> s
//! signal JobCompleted(s job_id, s submit_mode, s profile, aas outputs,
//!                     s transcript_path, t bytes, s backend)
//! signal JobFailed(s job_id, s error)
//! signal JobProgress(s job_id, s state)
//! ```
//!
//! ## Why `JobCompleted` is distinct from `Recorder1.TranscriptComplete`
//!
//! Finding arch#1 / DA#3: the two signals MUST never be conflated.
//! `job_id` is a *job* namespace, not a session id; an old subscriber
//! on `Recorder1.TranscriptComplete` is unaffected by anything on this
//! interface.
//!
//! ## `bytes` is `t` (u64), not `x`
//!
//! The RFC sketch wrote `x bytes`, but we use **unsigned** `t` for
//! consistency with the frozen `Recorder1.TranscriptComplete` (which is
//! `t`) and stress-test correction C6 ("a size cannot be negative").
//! This is a new signal, so there is no freeze to break.
//!
//! ## `outputs` encoding (`aas`)
//!
//! The daemon resolves `profile.outputs` at completion time (F3.1) and
//! carries it here so the `deliver` consumer never re-reads the profile
//! from disk. Each `OutputDest` is encoded as a string vector:
//! `File { path }` -> `["file", path]`, `Clipboard` -> `["clipboard"]`,
//! `Notification` -> `["notification"]`.
//!
//! `submit_mode` ∈ `"foreground" | "detached" | "auto"` drives the
//! consumer's intent-based stale-clipboard guard (F3.3).

use crate::types::JobInfo;

/// Client-side proxy for the `cz.zajca.Zwhisper1.Jobs1` interface.
#[zbus::proxy(
    interface = "cz.zajca.Zwhisper1.Jobs1",
    default_service = "cz.zajca.Zwhisper1",
    default_path = "/cz/zajca/Zwhisper1",
    gen_blocking = false
)]
pub trait Jobs1 {
    /// Enqueue a standalone transcription. Returns immediately with a
    /// freshly-minted `job_id` (UUID v4). The audio `path` is validated
    /// daemon-side (regular readable file); failure surfaces as
    /// `RpcError::AudioNotFound` / `RpcError::InvalidPath`.
    ///
    /// `submit_mode` is the caller's intent for the stale-clipboard
    /// guard (F3.3): `"foreground"` (the user is actively waiting — the
    /// CLI's `--queue`/dictation path) or `"detached"` (`--detach`). It
    /// rides back out in `JobCompleted` so the `deliver` consumer can
    /// decide inject-vs-notify without re-deriving intent. Unknown
    /// values are treated as `"detached"` (the safe default). The
    /// `"auto"` mode is daemon-internal (post-record auto-transcribe)
    /// and never sent by a client.
    fn transcribe_file(
        &self,
        path: &str,
        backend: &str,
        model: &str,
        lang: &str,
        submit_mode: &str,
    ) -> zbus::Result<String>;

    /// Cancel a queued or running job (best-effort). Fails with
    /// `RpcError::JobUnknown` when the id is not tracked.
    fn cancel(&self, job_id: &str) -> zbus::Result<()>;

    /// Snapshot of the queue: `[(job_id, state, label, submitted_ms)]`.
    fn list_jobs(&self) -> zbus::Result<Vec<JobInfo>>;

    /// Read-only per-interface protocol version (F4.1). A client probes
    /// this to confirm the daemon implements `Jobs1` and degrades
    /// gracefully against an older daemon that lacks the interface.
    #[zbus(property)]
    fn protocol_version(&self) -> zbus::Result<String>;

    /// Emitted when a job's transcript is on disk. DISTINCT from
    /// `Recorder1.TranscriptComplete` — never conflate the two.
    #[zbus(signal)]
    fn job_completed(
        &self,
        job_id: &str,
        submit_mode: &str,
        profile: &str,
        outputs: Vec<Vec<String>>,
        transcript_path: &str,
        bytes: u64,
        backend: &str,
    ) -> zbus::Result<()>;

    /// Emitted when a job fails. `error` is a human-readable, secret-free
    /// message (F1.4).
    #[zbus(signal)]
    fn job_failed(&self, job_id: &str, error: &str) -> zbus::Result<()>;

    /// State-transition notification: `queued -> running -> done|failed`
    /// (or `cancelled`). A pure state signal — no percent/ETA.
    #[zbus(signal)]
    fn job_progress(&self, job_id: &str, state: &str) -> zbus::Result<()>;
}
