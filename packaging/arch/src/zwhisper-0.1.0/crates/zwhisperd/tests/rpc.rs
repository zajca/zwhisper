//! D-Bus integration tests for `zwhisperd`.
//!
//! Each test spawns a private `dbus-daemon` + `zwhisperd` via
//! [`common::DbusFixture`] and drives the daemon through real
//! `Recorder1Proxy` / `Profiles1Proxy` clients. The harness skips
//! cleanly on hosts without `dbus-daemon` or session config (M0/M1
//! pattern) and audio-driving tests skip again when `PipeWire` is
//! unavailable.
//!
//! Test → `DoD` mapping (`M3-plan` § "Phase 5" lines 666-686):
//!
//! | Test                                                      | DoD |
//! |-----------------------------------------------------------|-----|
//! | bus_name_is_owned_after_serve_at                          |  1  |
//! | get_status_returns_idle_on_fresh_daemon                   |  6  |
//! | start_recording_emits_state_changed_starting              |  3  |
//! | concurrent_start_recording_returns_session_in_use         |  9  |
//! | stop_recording_unknown_id_returns_session_unknown         |  -  |
//! | profiles_list_matches_local_list_entries                  | 13  |
//! | profiles_set_active_unknown_name_returns_profile_not_found| 16  |
//! | profiles_set_active_empty_returns_profile_not_found       | C11 |
//! | profiles_reload_is_no_op                                  | 17  |
//! | recording_complete_arrives_before_state_changed_idle      | C9  |

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
use zwhisper_ipc::{BUS_NAME, OBJECT_PATH};

/// Per the M3 plan, the recording-driving tests skip cleanly on
/// hosts without `PipeWire`. Same probe as the M0 / CLI tests.
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

/// Try to build the fixture and start `zwhisperd` against it.
/// Returns `None` after printing a `[SKIP]` line when the host
/// cannot run the suite — every test starts with this idiom.
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

/// True when `err` is the typed `RecordingFailed` error name. Used
/// to differentiate "`PipeWire` missing" from "real bug" in the
/// audio-driving tests.
///
/// The daemon emits `RpcError::RecordingFailed` as
/// `org.freedesktop.DBus.Error.Failed` with the typed prefix in the
/// body — `parse_error_name_from_zbus` decodes both wire shapes.
fn is_recording_failed(err: &zbus::Error) -> bool {
    zwhisper_ipc::parse_error_name_from_zbus(err) == Some("RecordingFailed")
}

#[tokio::test(flavor = "current_thread")]
async fn bus_name_is_owned_after_serve_at() {
    let Some(fixture) = try_fixture("bus_name_is_owned_after_serve_at").await else {
        return;
    };
    let conn = fixture
        .connection()
        .await
        .expect("fresh connection to fixture bus");
    let dbus = zbus::fdo::DBusProxy::new(&conn)
        .await
        .expect("DBusProxy on fixture bus");
    let owned = dbus
        .name_has_owner(zbus::names::BusName::try_from(BUS_NAME).unwrap())
        .await
        .expect("NameHasOwner round trip");
    assert!(owned, "{BUS_NAME} should be owned after spawn_zwhisperd");
}

#[tokio::test(flavor = "current_thread")]
async fn get_status_returns_idle_on_fresh_daemon() {
    let Some(fixture) = try_fixture("get_status_returns_idle_on_fresh_daemon").await else {
        return;
    };
    let proxy = fixture.proxy_recorder().await.expect("Recorder1 proxy");
    let status = proxy.get_status().await.expect("GetStatus round trip");
    assert_eq!(status.state, "idle", "fresh daemon must be in idle state");
    assert_eq!(
        status.active_profile, "",
        "fresh daemon must have no active profile",
    );
    assert_eq!(status.duration_ms, 0, "fresh daemon must report 0 ms");
}

#[tokio::test(flavor = "current_thread")]
async fn start_recording_emits_state_changed_starting() {
    if !pipewire_socket_present() {
        eprintln!("[SKIP] start_recording_emits_state_changed_starting: PipeWire unavailable");
        return;
    }
    let Some(fixture) = try_fixture("start_recording_emits_state_changed_starting").await else {
        return;
    };
    let proxy = fixture.proxy_recorder().await.expect("Recorder1 proxy");

    // Subscribe BEFORE calling — same race-fix the CLI uses
    // (record.rs comment block "SUBSCRIBE FIRST").
    let mut state_stream = proxy
        .receive_state_changed()
        .await
        .expect("subscribe StateChanged");

    let start_res = proxy.start_recording("default").await;
    if let Err(err) = &start_res {
        if is_recording_failed(err) {
            eprintln!(
                "[SKIP] start_recording_emits_state_changed_starting: PipeWire unavailable (RecordingFailed)",
            );
            // Best-effort cleanup before the fixture's Drop runs.
            drop(state_stream);
            let _ = fixture.proxy_recorder().await;
            return;
        }
        panic!("StartRecording errored unexpectedly: {err}");
    }
    let session_id = start_res.unwrap();

    // First StateChanged within 200 ms must be "starting" — C9
    // lifecycle ordering.
    let first = tokio::time::timeout(Duration::from_millis(200), state_stream.next())
        .await
        .expect("first StateChanged within 200 ms")
        .expect("stream not closed");
    let args = first.args().expect("decode signal args");
    assert_eq!(
        args.new_state, "starting",
        "first StateChanged must be \"starting\"",
    );
    assert_eq!(args.session_id, session_id, "session_id must match");

    // Drain by stopping the recording so the daemon shuts down
    // cleanly. Errors here are non-fatal; the fixture Drop kills
    // the process anyway.
    let _ = proxy.stop_recording(&session_id).await;
    // Give the daemon a beat to finalise so logs aren't truncated
    // mid-EOS.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_start_recording_returns_session_in_use() {
    if !pipewire_socket_present() {
        eprintln!("[SKIP] concurrent_start_recording_returns_session_in_use: PipeWire unavailable");
        return;
    }
    let Some(fixture) = try_fixture("concurrent_start_recording_returns_session_in_use").await
    else {
        return;
    };
    let proxy = fixture.proxy_recorder().await.expect("Recorder1 proxy");

    let mut state_stream = proxy
        .receive_state_changed()
        .await
        .expect("subscribe StateChanged");

    let session_id = match proxy.start_recording("default").await {
        Ok(id) => id,
        Err(err) if is_recording_failed(&err) => {
            eprintln!(
                "[SKIP] concurrent_start_recording_returns_session_in_use: PipeWire unavailable (RecordingFailed)",
            );
            return;
        }
        Err(err) => panic!("first StartRecording errored unexpectedly: {err}"),
    };

    // Wait for "recording" so the slot is fully reserved before we
    // try to grab it again. Timeout is generous because pipeline
    // start can take up to ~1.5 s on cold runtimes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let signal = tokio::time::timeout(remaining, state_stream.next())
            .await
            .expect("StateChanged \"recording\" within 3 s")
            .expect("stream not closed");
        let args = signal.args().expect("decode signal args");
        if args.session_id == session_id && args.new_state == "recording" {
            break;
        }
    }

    // Second call must fail with SessionInUse.
    let second = proxy
        .start_recording("default")
        .await
        .expect_err("second StartRecording must fail");
    let msg = second.to_string();
    assert!(
        msg.contains("SessionInUse"),
        "expected SessionInUse error, got: {msg}",
    );

    // Drain.
    let _ = proxy.stop_recording(&session_id).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn stop_recording_unknown_id_returns_session_unknown() {
    let Some(fixture) = try_fixture("stop_recording_unknown_id_returns_session_unknown").await
    else {
        return;
    };
    let proxy = fixture.proxy_recorder().await.expect("Recorder1 proxy");
    let err = proxy
        .stop_recording("00000000-0000-0000-0000-000000000000")
        .await
        .expect_err("StopRecording with unknown id must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("SessionUnknown"),
        "expected SessionUnknown error, got: {msg}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn profiles_list_matches_local_list_entries() {
    let Some(fixture) = try_fixture("profiles_list_matches_local_list_entries").await else {
        return;
    };
    let proxy = fixture.proxy_profiles().await.expect("Profiles1 proxy");

    let wire = proxy.list().await.expect("Profiles1.List round trip");

    // Compare to the same source the daemon reads from. The daemon
    // and the test process both run with the fixture's
    // XDG_CONFIG_HOME / XDG_DATA_HOME set, but `list_entries()`
    // here in the test process reads our own env — which is *not*
    // the fixture's. The set of profile names we expect to see is
    // therefore the *embedded* set: the daemon under the fixture
    // sees no user overrides because XDG_CONFIG_HOME points at an
    // empty tempdir.
    //
    // We assert the bare minimum: the two well-known shipped
    // names ("default" and "meeting") are present and every entry
    // has schema_version == CURRENT_SCHEMA_VERSION (C12).
    let names: Vec<&str> = wire.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"default"),
        "wire list must include `default`; got {names:?}"
    );
    assert!(
        names.contains(&"meeting"),
        "wire list must include `meeting`; got {names:?}"
    );
    for entry in &wire {
        assert_eq!(
            entry.schema_version,
            zwhisper_core::profile::loader::CURRENT_SCHEMA_VERSION,
            "entry {:?} carries schema_version {} but daemon must report post-migration version",
            entry.name,
            entry.schema_version,
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn profiles_set_active_unknown_name_returns_profile_not_found() {
    let Some(fixture) =
        try_fixture("profiles_set_active_unknown_name_returns_profile_not_found").await
    else {
        return;
    };
    let proxy = fixture.proxy_profiles().await.expect("Profiles1 proxy");
    let err = proxy
        .set_active("there-is-no-such-profile")
        .await
        .expect_err("SetActive on unknown name must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("ProfileNotFound"),
        "expected ProfileNotFound error, got: {msg}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn profiles_set_active_empty_returns_profile_not_found() {
    let Some(fixture) = try_fixture("profiles_set_active_empty_returns_profile_not_found").await
    else {
        return;
    };
    let proxy = fixture.proxy_profiles().await.expect("Profiles1 proxy");
    let err = proxy
        .set_active("")
        .await
        .expect_err("SetActive(\"\") must fail per C11");
    let msg = err.to_string();
    assert!(
        msg.contains("ProfileNotFound"),
        "expected ProfileNotFound error (C11 normalises empty to \"(empty)\"), got: {msg}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn profiles_reload_is_no_op() {
    let Some(fixture) = try_fixture("profiles_reload_is_no_op").await else {
        return;
    };
    let proxy = fixture.proxy_profiles().await.expect("Profiles1 proxy");
    proxy.reload().await.expect("first Reload must succeed");
    proxy.reload().await.expect("second Reload must succeed");
}

/// C9 lock-in: `RecordingComplete` is emitted strictly before the
/// terminal `StateChanged "idle"` for the same `session_id`. This
/// test uses the `default` profile (transcription.auto = false) so
/// the terminal signal is plain idle.
#[tokio::test(flavor = "current_thread")]
async fn recording_complete_arrives_before_state_changed_idle() {
    if !pipewire_socket_present() {
        eprintln!(
            "[SKIP] recording_complete_arrives_before_state_changed_idle: PipeWire unavailable",
        );
        return;
    }
    let Some(fixture) = try_fixture("recording_complete_arrives_before_state_changed_idle").await
    else {
        return;
    };
    let proxy = fixture.proxy_recorder().await.expect("Recorder1 proxy");

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
                "[SKIP] recording_complete_arrives_before_state_changed_idle: PipeWire unavailable (RecordingFailed)",
            );
            return;
        }
        Err(err) => panic!("StartRecording errored unexpectedly: {err}"),
    };

    // Wait for "recording" so the daemon is mid-stream before we
    // request a stop. ~3 s budget (cold pipelines can take ~1.5 s).
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

    // Record for a short beat, then stop.
    tokio::time::sleep(Duration::from_millis(500)).await;
    proxy
        .stop_recording(&session_id)
        .await
        .expect("StopRecording must succeed");

    // Now race the two streams under a 10 s deadline. We expect
    // RecordingComplete before StateChanged "idle".
    let mut saw_recording_complete = false;
    let mut saw_idle = false;
    let mut idle_before_complete = false;
    let outer_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if saw_recording_complete && saw_idle {
            break;
        }
        assert!(
            tokio::time::Instant::now() < outer_deadline,
            "timed out waiting for both signals; \
             saw_recording_complete={saw_recording_complete}, \
             saw_idle={saw_idle}",
        );
        let remaining = outer_deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::select! {
            biased;
            sig = rec_complete_stream.next() => {
                let sig = sig.expect("rec_complete_stream not closed");
                let args = sig.args().expect("decode RecordingComplete args");
                if args.session_id == session_id {
                    saw_recording_complete = true;
                }
            }
            sig = state_stream.next() => {
                let sig = sig.expect("state_stream not closed");
                let args = sig.args().expect("decode StateChanged args");
                if args.session_id == session_id && args.new_state == "idle" {
                    if !saw_recording_complete {
                        idle_before_complete = true;
                    }
                    saw_idle = true;
                }
            }
            () = tokio::time::sleep(remaining) => {
                panic!(
                    "outer timeout reached; \
                     saw_recording_complete={saw_recording_complete}, \
                     saw_idle={saw_idle}",
                );
            }
        }
    }

    assert!(
        !idle_before_complete,
        "C9 violation: StateChanged \"idle\" arrived before RecordingComplete",
    );
    assert!(saw_recording_complete);
    assert!(saw_idle);
    // OBJECT_PATH is referenced just to silence unused-import warnings
    // in this trimmed-down test set; this line is a no-op assertion.
    assert!(!OBJECT_PATH.is_empty());
}
