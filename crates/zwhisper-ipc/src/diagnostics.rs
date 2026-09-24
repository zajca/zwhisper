//! `Diagnostics1` D-Bus interface — proxy (client) side.
//!
//! `Recorder1` is frozen at `StateChanged(s new_state, s session_id)`
//! and `Status = (sst)`, neither of which has room for a reason. Worse,
//! `Recorder1.GetStatus` can only ever return `idle` or `recording`, so
//! a client that was not listening at the moment of the failure has no
//! way to learn that one happened at all.
//!
//! This interface adds the detail alongside the frozen surface rather
//! than mutating it (RFC-actionable-errors § F7):
//!
//! ```text
//! GetLastFailure() -> (s session_id, s job_id, s code, s message,
//!                      s action, t at_ms)                  // (ssssst)
//! FailureReported  (s session_id, s job_id, s code, s message, s action)
//! ```
//!
//! ## Ordering (locked in by a signal-ordering test)
//!
//! `FailureReported` is emitted **strictly before** the corresponding
//! terminal `Recorder1.StateChanged "failed"` or `Jobs1.JobFailed` for
//! the same work item — the same discipline as
//! `RecordingComplete`-before-`StateChanged`. A client that already
//! watches `StateChanged` therefore has the reason in hand by the time
//! the state arrives, with no extra round-trip and no race.
//!
//! ## Empty ids
//!
//! `session_id` is empty when no recording session exists yet (a
//! pre-capture refusal) or when the failure belongs to a standalone
//! `zwhisper transcribe --queue` job. `job_id` is empty for every
//! recording-side failure. At least one is always set except for a
//! pre-capture refusal, where neither exists and the profile is the only
//! context — which the message carries.

use serde::{Deserialize, Serialize};
use zvariant::Type;

/// Snapshot returned by `Diagnostics1.GetLastFailure`.
///
/// Wire signature: `(ssssst)`. An **empty `code`** means the daemon has
/// not observed a failure since it started; every other field is then
/// empty / zero too. Consumers must check `code` rather than assuming a
/// reply implies a failure.
///
/// `at_ms` is a Unix-epoch millisecond timestamp, unsigned for the same
/// reason `Status.duration_ms` is (M3 stress-test C6 — a timestamp is
/// never negative).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
pub struct LastFailure {
    pub session_id: String,
    pub job_id: String,
    /// Stable machine-readable code
    /// (`zwhisper_core::diagnostics::FailureCode`), or `""` when the
    /// daemon has not failed since startup.
    pub code: String,
    pub message: String,
    pub action: String,
    pub at_ms: u64,
}

impl LastFailure {
    /// Whether this snapshot describes an actual failure. A reply with
    /// an empty `code` is the "nothing has gone wrong" answer, not a
    /// failure with a missing code.
    #[must_use]
    pub fn is_present(&self) -> bool {
        !self.code.is_empty()
    }
}

/// Client-side proxy for the `cz.zajca.Zwhisper1.Diagnostics1`
/// interface. Async-only, matching the other proxies in this crate.
#[zbus::proxy(
    interface = "cz.zajca.Zwhisper1.Diagnostics1",
    default_service = "cz.zajca.Zwhisper1",
    default_path = "/cz/zajca/Zwhisper1",
    gen_blocking = false
)]
pub trait Diagnostics1 {
    /// The last terminal failure the daemon observed, or a snapshot
    /// with an empty `code` when there has been none.
    ///
    /// This is what makes the one-shot `zwhisper status` able to report
    /// a failure at all: `Recorder1.GetStatus` never returns `failed`.
    fn get_last_failure(&self) -> zbus::Result<LastFailure>;

    /// A failure, with its code, message, and suggested action.
    ///
    /// Emitted before the matching `Recorder1.StateChanged "failed"` /
    /// `Jobs1.JobFailed`. `session_id` / `job_id` are empty when not
    /// applicable; see the module docs.
    #[zbus(signal)]
    fn failure_reported(
        &self,
        session_id: &str,
        job_id: &str,
        code: &str,
        message: &str,
        action: &str,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_failure_serializes_to_dbus_signature_ssssst() {
        // RFC-actionable-errors § F7 pins `GetLastFailure` at
        // `(ssssst)`. Drift means every client mis-reads the reply.
        assert_eq!(LastFailure::SIGNATURE.to_string(), "(ssssst)");
    }

    #[test]
    fn default_snapshot_reports_no_failure() {
        assert!(!LastFailure::default().is_present());
    }

    #[test]
    fn a_snapshot_with_a_code_is_present() {
        let snapshot = LastFailure {
            code: "mic_muted".to_owned(),
            ..LastFailure::default()
        };
        assert!(snapshot.is_present());
    }
}
