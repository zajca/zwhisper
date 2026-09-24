//! Integration tests for the `Diagnostics1` surface
//! (RFC-actionable-errors § F7).
//!
//! The contract these pin down:
//!
//! - Every job failure produces a `FailureReported` carrying a stable
//!   code **and** a non-empty action — the whole point of the feature is
//!   that a failure tells the user what to do.
//! - `FailureReported` is delivered **before** the matching
//!   `Jobs1.JobFailed`, so a client that reacts to the frozen signal
//!   already holds the reason.
//! - `GetLastFailure` answers for a client that missed the signal, and
//!   reports "nothing has gone wrong" as an empty code rather than an
//!   error.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::{DbusFixture, FixtureSkip};
use futures_util::StreamExt;

/// Generous: the job has to be scheduled, run a real backend attempt,
/// and fail. Slack for a loaded CI box.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(20);

async fn try_fixture(test_name: &str) -> Option<DbusFixture> {
    let mut fixture = match DbusFixture::try_new() {
        Ok(f) => f,
        Err(e @ (FixtureSkip::NoDbusDaemon | FixtureSkip::NoDbusConfig)) => {
            eprintln!("[SKIP] {test_name}: {e}");
            return None;
        }
        Err(FixtureSkip::Other(msg)) => {
            eprintln!("[SKIP] {test_name}: fixture setup failed: {msg}");
            return None;
        }
    };
    if let Err(e) = fixture.spawn_zwhisperd().await {
        eprintln!("[SKIP] {test_name}: zwhisperd failed to claim bus: {e}");
        return None;
    }
    Some(fixture)
}

/// Write a file that passes path validation but cannot possibly decode,
/// so the transcribe job fails deterministically on any host.
fn undecodable_audio(fixture: &DbusFixture, name: &str) -> String {
    let audio = fixture.state_home().join(name);
    std::fs::create_dir_all(audio.parent().unwrap()).unwrap();
    std::fs::write(&audio, b"not really flac").unwrap();
    std::fs::canonicalize(&audio).unwrap().display().to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn a_fresh_daemon_reports_no_failure() {
    let Some(fixture) = try_fixture("a_fresh_daemon_reports_no_failure").await else {
        return;
    };
    let diag = fixture
        .proxy_diagnostics()
        .await
        .expect("Diagnostics1 proxy");
    let snapshot = diag.get_last_failure().await.expect("GetLastFailure");
    assert!(
        !snapshot.is_present(),
        "a daemon that has not failed must report an empty code, got {snapshot:?}",
    );
    assert!(snapshot.code.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_job_reports_a_coded_reason_with_an_action() {
    let Some(fixture) = try_fixture("a_failing_job_reports_a_coded_reason_with_an_action").await
    else {
        return;
    };
    let audio = undecodable_audio(&fixture, "clip-coded.flac");
    let jobs = fixture.proxy_jobs().await.expect("Jobs1 proxy");
    let diag = fixture
        .proxy_diagnostics()
        .await
        .expect("Diagnostics1 proxy");

    // Subscribe before submitting: the job can fail before we would
    // otherwise be listening.
    let mut reported = diag
        .receive_failure_reported()
        .await
        .expect("subscribe FailureReported");

    let job_id = jobs
        .transcribe_file(&audio, "whisper-cpp", "small", "auto", "detached")
        .await
        .expect("TranscribeFile returns a job id");

    let signal = tokio::time::timeout(SIGNAL_TIMEOUT, reported.next())
        .await
        .expect("FailureReported within the timeout")
        .expect("stream not closed");
    let args = signal.args().expect("decode FailureReported args");

    assert_eq!(args.job_id, job_id);
    assert!(!args.code.is_empty(), "the code is the stable contract");
    assert!(
        zwhisper_core::diagnostics::FailureCode::from_wire(args.code).is_some(),
        "`{}` is not a known FailureCode",
        args.code,
    );
    assert!(!args.message.is_empty(), "message must not be empty");
    assert!(
        !args.action.is_empty(),
        "an action is the whole point: {:?}",
        args,
    );

    // And the snapshot now answers for a client that missed the signal.
    let snapshot = diag.get_last_failure().await.expect("GetLastFailure");
    assert!(snapshot.is_present());
    assert_eq!(snapshot.job_id, job_id);
    assert_eq!(snapshot.code, args.code);
    assert!(snapshot.at_ms > 0);
}

#[tokio::test(flavor = "current_thread")]
async fn failure_reported_arrives_before_job_failed() {
    let Some(fixture) = try_fixture("failure_reported_arrives_before_job_failed").await else {
        return;
    };
    let audio = undecodable_audio(&fixture, "clip-order.flac");
    let jobs = fixture.proxy_jobs().await.expect("Jobs1 proxy");
    let diag = fixture
        .proxy_diagnostics()
        .await
        .expect("Diagnostics1 proxy");

    let mut reported = diag
        .receive_failure_reported()
        .await
        .expect("subscribe FailureReported");
    let mut failed = jobs
        .receive_job_failed()
        .await
        .expect("subscribe JobFailed");

    jobs.transcribe_file(&audio, "whisper-cpp", "small", "auto", "detached")
        .await
        .expect("TranscribeFile returns a job id");

    // Whichever stream yields first decides. The daemon emits the
    // reason and only then the frozen signal, and D-Bus preserves
    // per-sender ordering, so `FailureReported` must win. If it does
    // not, a client reacting to `JobFailed` would render a failure with
    // no reason attached — the exact gap this feature closes.
    let first = tokio::time::timeout(SIGNAL_TIMEOUT, async {
        tokio::select! {
            biased;
            r = reported.next() => r.map(|_| "FailureReported"),
            f = failed.next() => f.map(|_| "JobFailed"),
        }
    })
    .await
    .expect("one of the two signals within the timeout")
    .expect("stream not closed");

    assert_eq!(
        first, "FailureReported",
        "FailureReported must precede JobFailed for the same job",
    );
}
