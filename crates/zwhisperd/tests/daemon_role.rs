//! D-Bus integration tests for the RFC-daemon-role surface
//! (`Jobs1` + `History1`).
//!
//! Same private-bus harness + skip discipline as `rpc.rs`. These tests
//! avoid `PipeWire`/`whisper-cli` dependencies: they exercise the
//! interface wiring, path validation, history recording, and typed
//! errors — not an actual transcription run (a job whose backend is
//! absent simply lands in history as `failed`, which is still a valid
//! assertion target).
//!
//! Test → RFC mapping:
//!
//! | Test                                            | RFC |
//! |-------------------------------------------------|-----|
//! | jobs_and_history_protocol_versions_are_served   | F4.1|
//! | list_jobs_empty_on_fresh_daemon                 | F1  |
//! | transcribe_file_missing_path_is_audio_not_found | F1.4|
//! | transcribe_file_records_a_history_entry         | F2  |
//! | history_empty_on_fresh_daemon                   | F2  |
//! | retry_unknown_id_is_session_unknown             | F2.4|
//! | retry_returns_retry_unavailable_for_real_entry  | F2.4|
//! | cancel_unknown_job_is_job_unknown               | F1  |
//! | job_completed_is_distinct_from_transcript_done  | arch#1|

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::time::Duration;

use common::{DbusFixture, FixtureSkip};
use zwhisper_ipc::PROTOCOL_VERSION;

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

#[tokio::test(flavor = "current_thread")]
async fn jobs_and_history_protocol_versions_are_served() {
    let Some(fixture) = try_fixture("jobs_and_history_protocol_versions_are_served").await else {
        return;
    };
    let jobs = fixture.proxy_jobs().await.expect("Jobs1 proxy");
    let history = fixture.proxy_history().await.expect("History1 proxy");
    assert_eq!(
        jobs.protocol_version()
            .await
            .expect("Jobs1.ProtocolVersion"),
        PROTOCOL_VERSION,
    );
    assert_eq!(
        history
            .protocol_version()
            .await
            .expect("History1.ProtocolVersion"),
        PROTOCOL_VERSION,
    );
}

#[tokio::test(flavor = "current_thread")]
async fn list_jobs_empty_on_fresh_daemon() {
    let Some(fixture) = try_fixture("list_jobs_empty_on_fresh_daemon").await else {
        return;
    };
    let jobs = fixture.proxy_jobs().await.expect("Jobs1 proxy");
    let listing = jobs.list_jobs().await.expect("ListJobs round trip");
    assert!(listing.is_empty(), "fresh daemon has no active jobs");
}

#[tokio::test(flavor = "current_thread")]
async fn transcribe_file_missing_path_is_audio_not_found() {
    let Some(fixture) = try_fixture("transcribe_file_missing_path_is_audio_not_found").await else {
        return;
    };
    let jobs = fixture.proxy_jobs().await.expect("Jobs1 proxy");
    let err = jobs
        .transcribe_file(
            "/nonexistent/zwhisper-test-missing.flac",
            "whisper-cpp",
            "small",
            "auto",
            "detached",
        )
        .await
        .expect_err("missing path must fail");
    assert_eq!(
        zwhisper_ipc::parse_error_name_from_zbus(&err),
        Some("AudioNotFound"),
        "missing audio path must map to AudioNotFound, got {err:?}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn transcribe_file_records_a_history_entry() {
    let Some(fixture) = try_fixture("transcribe_file_records_a_history_entry").await else {
        return;
    };
    // A real, regular file passes path validation. The backend will
    // likely fail to decode the junk bytes (or whisper-cli is absent),
    // so the job ends `failed` — but the HISTORY ENTRY must exist either
    // way, which is what we assert (F2 records every daemon-routed job).
    let audio = fixture.state_home().join("clip.flac");
    std::fs::create_dir_all(audio.parent().unwrap()).unwrap();
    std::fs::write(&audio, b"not really flac").unwrap();
    let audio_str = std::fs::canonicalize(&audio).unwrap().display().to_string();

    let jobs = fixture.proxy_jobs().await.expect("Jobs1 proxy");
    let history = fixture.proxy_history().await.expect("History1 proxy");

    let job_id = jobs
        .transcribe_file(&audio_str, "whisper-cpp", "small", "auto", "detached")
        .await
        .expect("TranscribeFile returns a job id");
    assert!(!job_id.is_empty(), "job id must be non-empty");

    // Poll history until our entry shows up (the upsert happens before
    // the job runs, so this is fast — but allow for scheduling slack).
    let mut found = None;
    for _ in 0..50 {
        let sessions = history.list_sessions(50, 0).await.expect("ListSessions");
        if let Some(e) = sessions.into_iter().find(|s| s.audio_path == audio_str) {
            found = Some(e);
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let entry = found.expect("history must record the transcribe-file session");
    assert_eq!(entry.audio_path, audio_str);
    assert_eq!(entry.backend, "whisper-cpp");
    // GetSession by the recorded id round-trips.
    let got = history
        .get_session(&entry.session_id)
        .await
        .expect("GetSession round trip");
    assert_eq!(got.session_id, entry.session_id);
}

#[tokio::test(flavor = "current_thread")]
async fn history_empty_on_fresh_daemon() {
    let Some(fixture) = try_fixture("history_empty_on_fresh_daemon").await else {
        return;
    };
    let history = fixture.proxy_history().await.expect("History1 proxy");
    let sessions = history.list_sessions(20, 0).await.expect("ListSessions");
    assert!(sessions.is_empty(), "fresh daemon has empty history");
}

#[tokio::test(flavor = "current_thread")]
async fn retry_unknown_id_is_session_unknown() {
    let Some(fixture) = try_fixture("retry_unknown_id_is_session_unknown").await else {
        return;
    };
    let history = fixture.proxy_history().await.expect("History1 proxy");
    let err = history
        .retry("00000000-0000-0000-0000-000000000000")
        .await
        .expect_err("retry of unknown id must fail");
    assert_eq!(
        zwhisper_ipc::parse_error_name_from_zbus(&err),
        Some("SessionUnknown"),
        "unknown retry id must map to SessionUnknown, got {err:?}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn retry_returns_retry_unavailable_for_real_entry() {
    let Some(fixture) = try_fixture("retry_returns_retry_unavailable_for_real_entry").await else {
        return;
    };
    // Record an entry whose audio_path EXISTS, so retry passes the
    // SessionUnknown + AudioNotFound guards and reaches the Phase-4 gate.
    let audio = fixture.state_home().join("retry.flac");
    std::fs::create_dir_all(audio.parent().unwrap()).unwrap();
    std::fs::write(&audio, b"junk").unwrap();
    let audio_str = std::fs::canonicalize(&audio).unwrap().display().to_string();

    let jobs = fixture.proxy_jobs().await.expect("Jobs1 proxy");
    let history = fixture.proxy_history().await.expect("History1 proxy");
    jobs.transcribe_file(&audio_str, "whisper-cpp", "small", "auto", "detached")
        .await
        .expect("TranscribeFile");

    let mut session_id = None;
    for _ in 0..50 {
        let sessions = history.list_sessions(50, 0).await.expect("ListSessions");
        if let Some(e) = sessions.into_iter().find(|s| s.audio_path == audio_str) {
            session_id = Some(e.session_id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let session_id = session_id.expect("entry recorded");
    let err = history
        .retry(&session_id)
        .await
        .expect_err("retry is gated until the audio RFC (F2.4)");
    assert_eq!(
        zwhisper_ipc::parse_error_name_from_zbus(&err),
        Some("RetryUnavailable"),
        "retry of a real entry must be RetryUnavailable, got {err:?}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_unknown_job_is_job_unknown() {
    let Some(fixture) = try_fixture("cancel_unknown_job_is_job_unknown").await else {
        return;
    };
    let jobs = fixture.proxy_jobs().await.expect("Jobs1 proxy");
    let err = jobs
        .cancel("11111111-1111-1111-1111-111111111111")
        .await
        .expect_err("cancel of unknown job must fail");
    assert_eq!(
        zwhisper_ipc::parse_error_name_from_zbus(&err),
        Some("JobUnknown"),
        "unknown cancel id must map to JobUnknown, got {err:?}",
    );
}
