//! M4 binding amendment **C2** integration tests.
//!
//! The C2 ordering invariant: the daemon MUST flush
//! `last-session.json` to disk **before** emitting the
//! corresponding D-Bus signal. A tray that bootstraps inside the
//! signal-delivery window relies on this so its "Open last
//! recording" / "Open last transcript" menu entries match the
//! freshly-arrived signal.
//!
//! Two-phase write per M4-plan § "C2 binding amendment":
//! - After `RecordingComplete`: `transcript_path` empty.
//! - After `TranscriptComplete`: both paths populated.
//!
//! The audio-only test path covers both the success (transcribe
//! disabled or fast) case and the recording-failure case where the
//! transcript never lands.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{DbusFixture, FixtureSkip};
use futures_util::StreamExt;
use serde::Deserialize;

fn pipewire_socket_present() -> bool {
    if let Some(runtime) = dirs::runtime_dir() {
        if runtime.join("pipewire-0").exists() {
            return true;
        }
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        if PathBuf::from(runtime).join("pipewire-0").exists() {
            return true;
        }
    }
    false
}

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

fn is_recording_failed(err: &zbus::Error) -> bool {
    if let zbus::Error::MethodError(name, _, _) = err {
        return name.as_str().ends_with(".RecordingFailed");
    }
    false
}

/// Mirror of `crates/zwhisperd/src/last_session.rs::LastSession`.
/// Kept as a test-local copy so we are reading the on-disk format,
/// not the in-memory representation.
#[derive(Debug, Deserialize)]
struct LastSessionOnDisk {
    schema_version: u32,
    session_id: String,
    audio_path: String,
    transcript_path: String,
    backend: String,
    completed_at_unix_ms: u64,
}

/// **C2 ordering test.**
///
/// On `RecordingComplete` the file is on disk with `transcript_path:
/// ""` and `audio_path` populated. On `TranscriptComplete` (when
/// transcribe runs) the file is on disk with both paths populated.
#[tokio::test(flavor = "current_thread")]
async fn last_session_file_persisted_before_recording_complete_signal() {
    if !pipewire_socket_present() {
        eprintln!(
            "[SKIP] last_session_file_persisted_before_recording_complete_signal: PipeWire unavailable",
        );
        return;
    }
    let Some(fixture) =
        try_fixture("last_session_file_persisted_before_recording_complete_signal").await
    else {
        return;
    };
    let proxy = fixture.proxy_recorder().await.expect("Recorder1 proxy");
    let state_file = fixture
        .state_home()
        .join("zwhisper")
        .join("last-session.json");

    let mut state_stream = proxy
        .receive_state_changed()
        .await
        .expect("subscribe StateChanged");
    let mut rec_complete_stream = proxy
        .receive_recording_complete()
        .await
        .expect("subscribe RecordingComplete");

    let session_id = match proxy.start_recording("default").await {
        Ok(id) => id,
        Err(err) if is_recording_failed(&err) => {
            eprintln!(
                "[SKIP] last_session_file_persisted_before_recording_complete_signal: PipeWire unavailable (RecordingFailed)",
            );
            return;
        }
        Err(err) => panic!("StartRecording errored unexpectedly: {err}"),
    };

    // Wait for "recording" so the recorder is mid-stream.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let sig = tokio::time::timeout(remaining, state_stream.next())
            .await
            .expect("StateChanged \"recording\" within 3 s")
            .expect("stream not closed");
        let args = sig.args().expect("decode signal args");
        if args.session_id == session_id && args.new_state == "recording" {
            break;
        }
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    proxy
        .stop_recording(&session_id)
        .await
        .expect("StopRecording must succeed");

    // Wait for RecordingComplete and verify the state-file is on
    // disk *before* this signal returned to us.
    let outer_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let remaining = outer_deadline.saturating_duration_since(tokio::time::Instant::now());
    let sig = tokio::time::timeout(remaining, rec_complete_stream.next())
        .await
        .expect("RecordingComplete within 10 s")
        .expect("stream not closed");
    let args = sig.args().expect("decode RecordingComplete args");
    assert_eq!(args.session_id, session_id);

    // C2 invariant: the file MUST exist on disk by the time the
    // signal is observed. No retries, no sleeps — the daemon
    // promise is "fsync, then signal".
    let bytes = std::fs::read(&state_file).unwrap_or_else(|e| {
        panic!(
            "expected last-session.json at {} after RecordingComplete: {e}",
            state_file.display(),
        )
    });
    let parsed: LastSessionOnDisk =
        serde_json::from_slice(&bytes).expect("last-session.json parses with schema v1");
    assert_eq!(parsed.schema_version, 1);
    assert_eq!(parsed.session_id, session_id);
    assert_eq!(parsed.audio_path, args.audio_path);

    // Phase 1 (audio-only) marker: transcript fields empty.
    if parsed.transcript_path.is_empty() {
        assert!(
            parsed.backend.is_empty(),
            "backend must be empty when transcript missing"
        );
    } else {
        // Race: TranscriptComplete already overwrote the file
        // before we got here. That's fine — the schema must still
        // match the second-phase invariant.
        assert!(!parsed.backend.is_empty());
    }
    assert!(parsed.completed_at_unix_ms > 0);
}

/// `TranscriptComplete` follow-up: state-file shows both paths.
#[tokio::test(flavor = "current_thread")]
async fn last_session_file_persisted_before_transcript_complete_signal() {
    if !pipewire_socket_present() {
        eprintln!(
            "[SKIP] last_session_file_persisted_before_transcript_complete_signal: PipeWire unavailable",
        );
        return;
    }
    let Some(fixture) =
        try_fixture("last_session_file_persisted_before_transcript_complete_signal").await
    else {
        return;
    };
    let proxy = fixture.proxy_recorder().await.expect("Recorder1 proxy");
    let state_file = fixture
        .state_home()
        .join("zwhisper")
        .join("last-session.json");

    let mut state_stream = proxy
        .receive_state_changed()
        .await
        .expect("subscribe StateChanged");
    let mut transcript_stream = proxy
        .receive_transcript_complete()
        .await
        .expect("subscribe TranscriptComplete");

    let session_id = match proxy.start_recording("default").await {
        Ok(id) => id,
        Err(err) if is_recording_failed(&err) => {
            eprintln!(
                "[SKIP] last_session_file_persisted_before_transcript_complete_signal: PipeWire unavailable (RecordingFailed)",
            );
            return;
        }
        Err(err) => panic!("StartRecording errored unexpectedly: {err}"),
    };

    // Wait for "recording".
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let sig = tokio::time::timeout(remaining, state_stream.next())
            .await
            .expect("StateChanged \"recording\" within 3 s")
            .expect("stream not closed");
        let args = sig.args().expect("decode signal args");
        if args.session_id == session_id && args.new_state == "recording" {
            break;
        }
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    proxy
        .stop_recording(&session_id)
        .await
        .expect("StopRecording must succeed");

    // Wait up to 60 s for TranscriptComplete (whisper.cpp can be
    // slow on first-cold model load). If transcribe is disabled
    // for the default profile or whisper-cli is missing, the
    // signal will never arrive — skip rather than fail.
    let outer_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let remaining = outer_deadline.saturating_duration_since(tokio::time::Instant::now());
    let Ok(maybe_sig) = tokio::time::timeout(remaining, transcript_stream.next()).await else {
        eprintln!(
            "[SKIP] last_session_file_persisted_before_transcript_complete_signal: TranscriptComplete never arrived (transcribe disabled or whisper-cli missing)",
        );
        return;
    };
    let sig = maybe_sig.expect("stream not closed");
    let args = sig.args().expect("decode TranscriptComplete args");
    assert_eq!(args.session_id, session_id);

    let bytes = std::fs::read(&state_file).unwrap_or_else(|e| {
        panic!(
            "expected last-session.json at {} after TranscriptComplete: {e}",
            state_file.display(),
        )
    });
    let parsed: LastSessionOnDisk =
        serde_json::from_slice(&bytes).expect("last-session.json parses with schema v1");
    assert_eq!(parsed.schema_version, 1);
    assert_eq!(parsed.session_id, session_id);
    assert_eq!(parsed.transcript_path, args.transcript_path);
    assert_eq!(parsed.backend, args.backend);
    assert!(!parsed.audio_path.is_empty());
    assert!(parsed.completed_at_unix_ms > 0);
}
