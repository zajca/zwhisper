//! Tunable timeouts for the daemon's shutdown path.
//!
//! Per CLAUDE.md, all configuration values live in a dedicated
//! module rather than scattered as `const` items across the source
//! tree. The two constants here are tuning knobs of the shutdown
//! state machine; keeping them together documents the relationship
//! between the two timeouts (the start-drain runs first, the
//! lifecycle-drain runs after — they should always be ordered such
//! that `INFLIGHT_START_DRAIN_TIMEOUT < SHUTDOWN_DRAIN_TIMEOUT`).

use std::time::Duration;

/// Maximum time the daemon waits for the in-flight session's
/// lifecycle task to finish draining after SIGTERM/SIGINT before
/// giving up and exiting anyway. Keeps shutdown responsive even
/// when the recorder is wedged.
///
/// **Rationale.** 30 s is generous enough that even a slow whisper.cpp
/// finalization on a small model can complete (M1 measurements: a
/// 30-second recording with `small` model finishes in <10 s on the
/// reference hardware), short enough that systemd's
/// `TimeoutStopSec=` defaults (90 s) leave ample margin for the
/// process exit itself.
pub(crate) const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time the daemon waits for in-flight `start_recording`
/// calls to finish (so their lifecycle handles get registered)
/// before draining lifecycle tasks.
///
/// **Rationale.** Short on purpose: the synchronous prelude inside
/// `start_recording` only takes a few hundred milliseconds even on
/// slow hardware, so 5 s is two orders of magnitude of headroom.
/// Keeping this much shorter than [`SHUTDOWN_DRAIN_TIMEOUT`] means a
/// SIGTERM landing in the brief await-heavy window between
/// `try_reserve` and `spawn_lifecycle` does not eat the entire
/// shutdown budget.
pub(crate) const INFLIGHT_START_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Default transcription-job concurrency (RFC-daemon-role F1.3).
/// **Decided: global serialized = 1.** whisper-cli is heavy; running
/// two at once thrashes CPU/RAM for no throughput win. Per-backend
/// parallel lanes (e.g. I/O-bound Deepgram) are deferred (YAGNI).
pub(crate) const DEFAULT_JOB_CONCURRENCY: usize = 1;

/// Environment variable that overrides [`DEFAULT_JOB_CONCURRENCY`].
pub(crate) const JOB_CONCURRENCY_ENV: &str = "ZWHISPER_JOB_CONCURRENCY";

/// Resolve the effective job concurrency. The default IS the design
/// value (not a silent invented default); the env var is an explicit
/// opt-in override. An unset/empty/unparseable/zero value falls back to
/// the default with a WARN so the operator sees the rejection rather
/// than silently getting surprising behaviour.
pub(crate) fn job_concurrency() -> usize {
    match std::env::var(JOB_CONCURRENCY_ENV) {
        Err(_) => DEFAULT_JOB_CONCURRENCY,
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => {
                tracing::warn!(
                    value = %raw,
                    "{JOB_CONCURRENCY_ENV} must be a positive integer; using default {DEFAULT_JOB_CONCURRENCY}",
                );
                DEFAULT_JOB_CONCURRENCY
            }
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn inflight_drain_strictly_shorter_than_shutdown_drain() {
        // The two timeouts run in series during shutdown(); inverting
        // them would mean the inflight-start drain could eat the
        // entire shutdown budget and leave no time for lifecycle
        // task drain.
        assert!(
            INFLIGHT_START_DRAIN_TIMEOUT < SHUTDOWN_DRAIN_TIMEOUT,
            "inflight drain ({INFLIGHT_START_DRAIN_TIMEOUT:?}) must be shorter than shutdown drain ({SHUTDOWN_DRAIN_TIMEOUT:?})",
        );
    }

    #[test]
    fn shutdown_drain_fits_systemd_default_timeoutstopsec() {
        // systemd default `TimeoutStopSec=90s`. We must finish well
        // inside that envelope so the kernel doesn't have to SIGKILL
        // us mid-finalization.
        assert!(
            SHUTDOWN_DRAIN_TIMEOUT <= Duration::from_secs(60),
            "shutdown drain ({SHUTDOWN_DRAIN_TIMEOUT:?}) too close to the systemd default 90 s envelope",
        );
    }
}
