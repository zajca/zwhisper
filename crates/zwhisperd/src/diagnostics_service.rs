//! Server-side `cz.zajca.Zwhisper1.Diagnostics1` interface plus the
//! [`FailureReporter`] every failure site funnels through
//! (RFC-actionable-errors § F7).
//!
//! Mirrors the proxy trait in `zwhisper-ipc::diagnostics` — same method,
//! same signal, same wire signatures. The mirror is mandatory because
//! zbus's `#[interface]` macro decorates an `impl` on a server-owned
//! struct while `#[proxy]` decorates a free trait (see
//! `recorder_service.rs` for the full rationale).
//!
//! ## Why a reporter rather than direct emission
//!
//! There are six places the daemon can fail, spread over three modules,
//! and two of them previously recorded nothing anywhere. Routing all of
//! them through one handle means the "store it, then emit it, then let
//! the frozen signal follow" ordering is written once, and a seventh
//! failure path added later gets the behaviour by construction.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};
use zbus::object_server::SignalEmitter;
use zwhisper_core::diagnostics::FailureReason;
use zwhisper_ipc::{LastFailure, OBJECT_PATH};

/// Shared, in-memory record of the daemon's last failure.
///
/// Deliberately **not** persisted: it answers "what just went wrong?"
/// for a client that missed the signal, and a failure from a previous
/// daemon lifetime is not that. The durable record is `history.json`.
type LastFailureCell = Arc<Mutex<LastFailure>>;

/// State held by the `Diagnostics1` interface impl.
#[derive(Debug)]
pub(crate) struct DiagnosticsInterface {
    last: LastFailureCell,
}

impl DiagnosticsInterface {
    pub(crate) fn new(last: LastFailureCell) -> Self {
        Self { last }
    }
}

#[zbus::interface(name = "cz.zajca.Zwhisper1.Diagnostics1")]
impl DiagnosticsInterface {
    /// The last failure the daemon observed, or a snapshot with an empty
    /// `code` when there has been none since startup.
    ///
    /// This is the method that makes the one-shot `zwhisper status`
    /// useful after a failure: `Recorder1.GetStatus` is only ever `idle`
    /// or `recording`, so without this a client that was not subscribed
    /// at the moment of the failure could not learn of it at all.
    async fn get_last_failure(&self) -> LastFailure {
        // A poisoned lock means an earlier panic in our own code. The
        // honest answer is then "no failure on record" rather than
        // propagating a D-Bus error for a diagnostic query.
        self.last
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Per-interface protocol version, mirroring `Jobs1` / `History1`.
    #[zbus(property)]
    #[allow(clippy::unused_self, reason = "zbus property handlers must take &self")]
    fn protocol_version(&self) -> &'static str {
        zwhisper_ipc::PROTOCOL_VERSION
    }

    #[zbus(signal)]
    async fn failure_reported(
        emitter: &SignalEmitter<'_>,
        session_id: &str,
        job_id: &str,
        code: &str,
        message: &str,
        action: &str,
    ) -> zbus::Result<()>;
}

/// Handle every failure site uses to publish a [`FailureReason`].
///
/// Cloneable and cheap: two `Arc`s. The connection arrives through the
/// same `OnceLock` the job queue uses, because the interfaces are
/// registered before the connection exists.
#[derive(Debug, Clone)]
pub(crate) struct FailureReporter {
    conn: Arc<OnceLock<zbus::Connection>>,
    last: LastFailureCell,
}

impl FailureReporter {
    pub(crate) fn new(conn: Arc<OnceLock<zbus::Connection>>) -> Self {
        Self {
            conn,
            last: Arc::new(Mutex::new(LastFailure::default())),
        }
    }

    /// The cell the `Diagnostics1` interface reads for
    /// `GetLastFailure`.
    pub(crate) fn last_failure_cell(&self) -> LastFailureCell {
        Arc::clone(&self.last)
    }

    /// Record and announce a failure.
    ///
    /// `session_id` is empty for a failure with no recording session (a
    /// pre-capture refusal, or a standalone `transcribe --queue` job);
    /// `job_id` is empty for every recording-side failure.
    ///
    /// **Call this before** the corresponding terminal
    /// `Recorder1.StateChanged "failed"` / `Jobs1.JobFailed`: that
    /// ordering is the contract clients rely on to have the reason in
    /// hand by the time the frozen signal arrives.
    ///
    /// Emission is best-effort — a client that is not listening simply
    /// misses it, exactly like every other signal in the system (IDEA
    /// §5 forbids a persistent outbox). The snapshot is stored first, so
    /// `GetLastFailure` still answers even when emission fails.
    pub(crate) async fn report(&self, session_id: &str, job_id: &str, reason: &FailureReason) {
        let snapshot = LastFailure {
            session_id: session_id.to_owned(),
            job_id: job_id.to_owned(),
            code: reason.code.as_str().to_owned(),
            message: reason.message.clone(),
            action: reason.action.clone(),
            at_ms: now_ms(),
        };

        match self.last.lock() {
            Ok(mut guard) => *guard = snapshot,
            Err(e) => warn!(error = %e, "last-failure cell poisoned; GetLastFailure will be stale"),
        }

        debug!(
            code = reason.code.as_str(),
            %session_id,
            %job_id,
            message = %reason.message,
            action = %reason.action,
            "reporting failure",
        );

        let Some(conn) = self.conn.get() else {
            warn!("Diagnostics1 connection not yet initialised; dropping FailureReported");
            return;
        };
        let iface = match conn
            .object_server()
            .interface::<_, DiagnosticsInterface>(OBJECT_PATH)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "could not acquire Diagnostics1 interface ref");
                return;
            }
        };
        if let Err(e) = iface
            .failure_reported(
                session_id,
                job_id,
                reason.code.as_str(),
                &reason.message,
                &reason.action,
            )
            .await
        {
            warn!(error = %e, "failed to emit FailureReported");
        }
    }
}

/// Unix-epoch milliseconds. A clock before the epoch yields `0` rather
/// than panicking — a nonsense timestamp on a diagnostic is strictly
/// better than taking the daemon down.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use zwhisper_core::diagnostics::{FailureCode, FailureReason};

    #[test]
    fn a_fresh_reporter_has_no_failure_on_record() {
        let reporter = FailureReporter::new(Arc::new(OnceLock::new()));
        let cell = reporter.last_failure_cell();
        assert!(!cell.lock().unwrap().is_present());
    }

    #[tokio::test]
    async fn report_stores_the_snapshot_even_without_a_connection() {
        // Emission needs a live bus; storing must not. A client calling
        // GetLastFailure has to see the failure regardless of whether
        // the signal went out.
        let reporter = FailureReporter::new(Arc::new(OnceLock::new()));
        let cell = reporter.last_failure_cell();
        reporter
            .report(
                "sess-1",
                "",
                &FailureReason::mic_muted("Built-in Mic", "alsa_input.pci", 52),
            )
            .await;

        let stored = cell.lock().unwrap().clone();
        assert!(stored.is_present());
        assert_eq!(stored.code, FailureCode::MicMuted.as_str());
        assert_eq!(stored.session_id, "sess-1");
        assert!(stored.job_id.is_empty());
        assert!(stored.message.contains("Built-in Mic"));
        assert!(stored.action.contains("wpctl set-mute 52 0"));
        assert!(stored.at_ms > 0);
    }

    #[tokio::test]
    async fn the_latest_report_wins() {
        let reporter = FailureReporter::new(Arc::new(OnceLock::new()));
        let cell = reporter.last_failure_cell();
        reporter
            .report("s", "", &FailureReason::interrupted("s"))
            .await;
        reporter
            .report(
                "",
                "job-9",
                &FailureReason::empty_transcript("parakeet", "v3", "auto"),
            )
            .await;

        let stored = cell.lock().unwrap().clone();
        assert_eq!(stored.code, FailureCode::EmptyTranscript.as_str());
        assert_eq!(stored.job_id, "job-9");
        assert!(stored.session_id.is_empty());
    }
}
