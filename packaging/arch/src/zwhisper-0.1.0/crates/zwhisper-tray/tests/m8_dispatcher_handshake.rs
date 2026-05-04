//! M8 follow-up — `run_dispatcher` enforces the protocol-version
//! handshake on its own connection (post-M8 review fix).
//!
//! Bug being pinned: the original M8 only ran the handshake inside
//! `run_pump`. The dispatcher and the hotkey listener owned
//! independent `zbus::Connection`s and would happily flip
//! `daemon_ready_tx = true` after a successful `GetStatus` against
//! a mismatched daemon — releasing the hotkey buffer to dispatch
//! `start_recording` against incompatible wire surface.
//!
//! These tests pin two invariants over a real (private) D-Bus:
//!
//! 1. When the daemon advertises a mismatched `ProtocolVersion`,
//!    `run_dispatcher` returns cleanly **without** flipping
//!    `daemon_ready_tx`, **and** broadcasts a workspace shutdown
//!    via the supplied `Sender<()>`.
//!
//! 2. The legacy-daemon path (no property at all) is treated the
//!    same way — same sticky exit, same shutdown broadcast.
//!
//! Skip-on-no-bus follows the existing fixture pattern from
//! `crates/zwhisper-cli/tests/m8_version_handshake.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::pedantic
)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use zbus::object_server::SignalEmitter;
use zwhisper_ipc::{BUS_NAME, OBJECT_PATH, ProfileEntry, ProfileEntryV2, Status};
use zwhisper_tray::cmd::run_dispatcher;
use zwhisper_tray::state::TrayState;

const HANDSHAKE_DEADLINE_MS: u64 = 1500;

/// Fake `Recorder1` that lets each test parameterise the version
/// reported via the `ProtocolVersion` property. Returning `None`
/// from `advertised_version` makes the property look like a
/// pre-0.1.0 daemon (zbus surfaces it as `UnknownProperty`). The
/// other methods exist only because zbus's interface impl needs
/// them on the `Recorder1` surface; the dispatcher under test
/// only ever calls `protocol_version` and (on Match) `get_status`.
#[derive(Debug, Clone)]
struct FakeRecorder {
    advertised_version: Option<String>,
}

#[zbus::interface(name = "cz.zajca.Zwhisper1.Recorder1")]
impl FakeRecorder {
    #[zbus(property)]
    fn protocol_version(&self) -> zbus::fdo::Result<String> {
        match &self.advertised_version {
            Some(v) => Ok(v.clone()),
            None => Err(zbus::fdo::Error::UnknownProperty(
                "ProtocolVersion (legacy daemon stand-in)".to_owned(),
            )),
        }
    }

    async fn get_status(&self) -> zbus::fdo::Result<Status> {
        Ok(Status {
            state: "idle".to_owned(),
            active_profile: String::new(),
            duration_ms: 0,
        })
    }

    async fn start_recording(
        &self,
        _profile_name: &str,
        #[zbus(signal_emitter)] _emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed("not supported".to_owned()))
    }

    async fn stop_recording(&self, _session_id: &str) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed("not supported".to_owned()))
    }

    #[zbus(signal)]
    async fn state_changed(
        emitter: &SignalEmitter<'_>,
        new_state: &str,
        session_id: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn recording_complete(
        emitter: &SignalEmitter<'_>,
        session_id: &str,
        audio_path: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn transcript_complete(
        emitter: &SignalEmitter<'_>,
        session_id: &str,
        transcript_path: &str,
        bytes: u64,
        backend: &str,
    ) -> zbus::Result<()>;
}

/// Minimal `Profiles1` stand-in. The dispatcher builds a
/// `Profiles1Proxy` before any RPC, so the bus must export the
/// interface even though the handshake test never hits any of
/// its methods.
#[derive(Debug, Clone)]
struct FakeProfiles;

#[zbus::interface(name = "cz.zajca.Zwhisper1.Profiles1")]
impl FakeProfiles {
    async fn list(&self) -> zbus::fdo::Result<Vec<ProfileEntry>> {
        Ok(Vec::new())
    }

    async fn list_v2(&self) -> zbus::fdo::Result<Vec<ProfileEntryV2>> {
        Ok(Vec::new())
    }

    async fn get_active(&self) -> zbus::fdo::Result<String> {
        Ok(String::new())
    }

    async fn set_active(&self, _name: &str) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn reload(&self) -> zbus::fdo::Result<()> {
        Ok(())
    }
}

struct Fixture {
    daemon_proc: std::process::Child,
    address: String,
    _state_dir: tempfile::TempDir,
    fake_join: Option<tokio::task::JoinHandle<()>>,
    fake_shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(tx) = self.fake_shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.fake_join.take() {
            join.abort();
        }
        let _ = self.daemon_proc.kill();
        let _ = self.daemon_proc.wait();
    }
}

fn locate_session_conf() -> Option<PathBuf> {
    for p in [
        "/usr/share/dbus-1/session.conf",
        "/etc/dbus-1/session.conf",
    ] {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    None
}

fn wait_for_socket(path: &std::path::Path, attempts: u32, slice: Duration) -> Result<(), String> {
    for _ in 0..attempts {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(slice);
    }
    Err(format!(
        "socket {} did not appear after {} attempts",
        path.display(),
        attempts
    ))
}

async fn try_fixture(test_name: &str, advertised_version: Option<String>) -> Option<Fixture> {
    if which::which("dbus-daemon").is_err() {
        eprintln!("[SKIP] {test_name}: dbus-daemon not on PATH");
        return None;
    }
    let Some(conf) = locate_session_conf() else {
        eprintln!("[SKIP] {test_name}: no dbus session.conf available");
        return None;
    };

    let tmp = tempfile::tempdir().ok()?;
    let socket_path = tmp.path().join("bus.sock");
    let address = format!("unix:path={}", socket_path.display());

    let daemon_proc = Command::new("dbus-daemon")
        .arg(format!("--config-file={}", conf.display()))
        .arg(format!("--address={address}"))
        .arg("--nofork")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    if let Err(e) = wait_for_socket(&socket_path, 100, Duration::from_millis(20)) {
        eprintln!("[SKIP] {test_name}: {e}");
        return None;
    }

    // Build the FakeRecorder + FakeProfiles connection on the
    // test's own runtime — `run_dispatcher` is in-process, so we
    // do not need a separate OS thread (and using one collides
    // with tokio's "no nested runtime" rule).
    let conn = match zbus::connection::Builder::address(address.as_str()) {
        Ok(b) => match b
            .name(BUS_NAME)
            .and_then(|b| b.serve_at(OBJECT_PATH, FakeRecorder { advertised_version }))
            .and_then(|b| b.serve_at(OBJECT_PATH, FakeProfiles))
        {
            Ok(b) => match b.build().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[SKIP] {test_name}: zbus build: {e}");
                    return None;
                }
            },
            Err(e) => {
                eprintln!("[SKIP] {test_name}: zbus serve_at: {e}");
                return None;
            }
        },
        Err(e) => {
            eprintln!("[SKIP] {test_name}: zbus address: {e}");
            return None;
        }
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let join = tokio::spawn(async move {
        // Hold the connection until shutdown so the bus name stays
        // owned for the dispatcher under test.
        let _conn = conn;
        let _ = shutdown_rx.await;
    });

    Some(Fixture {
        daemon_proc,
        address,
        _state_dir: tmp,
        fake_join: Some(join),
        fake_shutdown_tx: Some(shutdown_tx),
    })
}

/// Drive `run_dispatcher` once against the fixture and return the
/// observed `daemon_ready` final value plus whether `shutdown` was
/// broadcast within the test deadline. The dispatcher returns
/// cleanly on the mismatch path; on the match path we send our own
/// shutdown to terminate it after the handshake completes.
async fn drive_dispatcher_once(
    address: &str,
    expect_match: bool,
) -> (bool, bool) {
    let conn = zbus::connection::Builder::address(address)
        .expect("address parse")
        .build()
        .await
        .expect("connect to fixture bus");

    let (_cmd_tx, cmd_rx) = mpsc::channel(8);
    let (state_tx, state_rx) = watch::channel(TrayState::default());
    let (daemon_ready_tx, mut daemon_ready_rx) = watch::channel(false);
    let (shutdown_broadcast, mut shutdown_rx) = watch::channel(());

    // The dispatcher consumes its own `shutdown_rx`; clone the
    // receiver from the same channel so the broadcast it sends on
    // mismatch reaches BOTH the test (via `shutdown_rx`) and the
    // dispatcher itself (which would normally observe a sibling
    // pump's broadcast).
    let dispatcher_shutdown_rx = shutdown_broadcast.subscribe();
    let dispatcher_shutdown_broadcast = shutdown_broadcast.clone();

    let join = tokio::spawn(async move {
        let _ = run_dispatcher(
            conn,
            cmd_rx,
            state_tx,
            state_rx,
            daemon_ready_tx,
            dispatcher_shutdown_broadcast,
            dispatcher_shutdown_rx,
        )
        .await;
    });

    let saw_shutdown = tokio::time::timeout(
        Duration::from_millis(HANDSHAKE_DEADLINE_MS),
        shutdown_rx.changed(),
    )
    .await
    .is_ok();

    if expect_match && !saw_shutdown {
        // The match path keeps the dispatcher running; tear it
        // down so the test does not hang on join().
        let _ = shutdown_broadcast.send(());
    }

    let _ = tokio::time::timeout(Duration::from_millis(HANDSHAKE_DEADLINE_MS), join).await;

    let final_ready = *daemon_ready_rx.borrow_and_update();
    (final_ready, saw_shutdown)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_refuses_mismatched_daemon_and_broadcasts_shutdown() {
    let Some(fixture) = try_fixture(
        "dispatcher_refuses_mismatched_daemon_and_broadcasts_shutdown",
        Some("99.99.99".to_owned()),
    )
    .await
    else {
        return;
    };
    let (final_ready, saw_shutdown) = drive_dispatcher_once(&fixture.address, false).await;
    assert!(
        !final_ready,
        "daemon-ready gate must STAY false on protocol mismatch"
    );
    assert!(
        saw_shutdown,
        "dispatcher must broadcast a workspace shutdown on mismatch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_refuses_legacy_daemon_and_broadcasts_shutdown() {
    let Some(fixture) = try_fixture(
        "dispatcher_refuses_legacy_daemon_and_broadcasts_shutdown",
        None,
    )
    .await
    else {
        return;
    };
    let (final_ready, saw_shutdown) = drive_dispatcher_once(&fixture.address, false).await;
    assert!(
        !final_ready,
        "daemon-ready gate must STAY false on legacy daemon"
    );
    assert!(
        saw_shutdown,
        "dispatcher must broadcast a workspace shutdown on legacy daemon"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_proceeds_when_daemon_version_matches() {
    let Some(fixture) = try_fixture(
        "dispatcher_proceeds_when_daemon_version_matches",
        Some(env!("CARGO_PKG_VERSION").to_owned()),
    )
    .await
    else {
        return;
    };
    let (final_ready, saw_shutdown) = drive_dispatcher_once(&fixture.address, true).await;
    assert!(
        final_ready,
        "daemon-ready gate MUST flip to true after a matched handshake + GetStatus probe"
    );
    // On the match path the dispatcher does NOT broadcast on its
    // own — the test driver issues shutdown after the deadline so
    // join() can complete. The handshake+probe semantics are
    // captured by `final_ready`.
    let _ = saw_shutdown;
}
