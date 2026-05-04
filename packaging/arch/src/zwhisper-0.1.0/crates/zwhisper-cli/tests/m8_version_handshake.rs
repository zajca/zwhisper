//! M8 — CLI pre-flight version handshake (DoD #12).
//!
//! These tests host an in-process zbus interface that mimics the
//! daemon's `Recorder1` surface and asserts the CLI binary's
//! exit-code contract:
//!
//! - daemon advertises a different `ProtocolVersion`     → exit 4
//! - daemon does not implement the property at all       → exit 4
//! - daemon advertises the matching version              → handshake OK
//!   (continues into the next call; in this fixture we then return
//!    the canonical "no active session" idle status, exit 0)
//!
//! The fixture spawns a private `dbus-daemon`, advertises the bus
//! name `cz.zajca.Zwhisper1`, runs the `zwhisper` binary against it
//! via `assert_cmd`, and asserts on stdout/stderr/exit code. Skips
//! cleanly when `dbus-daemon` is unavailable.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::pedantic
)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use assert_cmd::cargo::CommandCargoExt;
use tokio::sync::oneshot;
use zbus::object_server::SignalEmitter;
use zwhisper_ipc::{BUS_NAME, OBJECT_PATH, Status};

const PROBE_BUS_TIMEOUT_MS: u64 = 2000;

/// In-process Recorder1 stand-in. Every test parameterises the
/// version string returned over the wire; legacy-daemon tests build
/// a stand-in that omits the property entirely.
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

    /// Trivial GetStatus so a "Match" handshake can also drive the
    /// real `zwhisper status` command end-to-end.
    async fn get_status(&self) -> zbus::fdo::Result<Status> {
        Ok(Status {
            state: "idle".to_owned(),
            active_profile: String::new(),
            duration_ms: 0,
        })
    }

    // The remaining Recorder1 methods are not exercised by the
    // handshake test; we omit them so the in-process stand-in stays
    // small. zbus only resolves what the proxy actually calls.
    async fn start_recording(
        &self,
        _profile_name: &str,
        #[zbus(signal_emitter)] _emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed(
            "fake recorder does not support start_recording".to_owned(),
        ))
    }

    async fn stop_recording(&self, _session_id: &str) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed(
            "fake recorder does not support stop_recording".to_owned(),
        ))
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

struct Fixture {
    daemon_proc: std::process::Child,
    address: String,
    _state_dir: tempfile::TempDir,
    fake_runtime_thread: Option<std::thread::JoinHandle<()>>,
    fake_shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(tx) = self.fake_shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.fake_runtime_thread.take() {
            let _ = handle.join();
        }
        // Best-effort kill the dbus-daemon child.
        let _ = self.daemon_proc.kill();
        let _ = self.daemon_proc.wait();
    }
}

fn locate_session_conf() -> Option<PathBuf> {
    let candidates = ["/usr/share/dbus-1/session.conf", "/etc/dbus-1/session.conf"];
    candidates.iter().find_map(|p| {
        let pb = PathBuf::from(p);
        if pb.exists() { Some(pb) } else { None }
    })
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

/// Spin up a private dbus-daemon, then host `FakeRecorder` on the
/// well-known `cz.zajca.Zwhisper1` name + `/cz/zajca/Zwhisper1` path
/// using a dedicated tokio runtime running on a worker thread. The
/// returned fixture stays alive until dropped.
fn try_fixture(test_name: &str, advertised_version: Option<String>) -> Option<Fixture> {
    if which::which("dbus-daemon").is_err() {
        eprintln!("[SKIP] {test_name}: dbus-daemon not on PATH");
        return None;
    }
    let Some(conf) = locate_session_conf() else {
        eprintln!("[SKIP] {test_name}: no dbus session.conf available");
        return None;
    };

    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[SKIP] {test_name}: tempdir failed: {e}");
            return None;
        }
    };
    let socket_path = tmp.path().join("bus.sock");
    let address = format!("unix:path={}", socket_path.display());

    let daemon_proc = match Command::new("dbus-daemon")
        .arg(format!("--config-file={}", conf.display()))
        .arg(format!("--address={address}"))
        .arg("--nofork")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[SKIP] {test_name}: dbus-daemon spawn failed: {e}");
            return None;
        }
    };

    if let Err(e) = wait_for_socket(&socket_path, 100, Duration::from_millis(20)) {
        eprintln!("[SKIP] {test_name}: {e}");
        return None;
    }

    // Spawn a worker thread that owns its own current-thread tokio
    // runtime and hosts the FakeRecorder on the bus. The shutdown
    // oneshot tears the runtime down deterministically when the
    // fixture drops.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let address_for_thread = address.clone();
    let join = std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("tokio runtime build: {e}")));
                return;
            }
        };
        rt.block_on(async move {
            let conn = match zbus::connection::Builder::address(address_for_thread.as_str()) {
                Ok(b) => match b
                    .name(BUS_NAME)
                    .and_then(|b| b.serve_at(OBJECT_PATH, FakeRecorder { advertised_version }))
                {
                    Ok(b) => match b.build().await {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("zbus build: {e}")));
                            return;
                        }
                    },
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("zbus serve_at: {e}")));
                        return;
                    }
                },
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("zbus address: {e}")));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            // Park until the test asks us to stop. `conn` lives the
            // whole time so the bus name stays owned.
            let _ = shutdown_rx.await;
            drop(conn);
        });
    });

    match ready_rx.recv_timeout(Duration::from_millis(PROBE_BUS_TIMEOUT_MS)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("[SKIP] {test_name}: fake daemon failed to start: {e}");
            return None;
        }
        Err(e) => {
            eprintln!("[SKIP] {test_name}: fake daemon did not become ready in time: {e}");
            return None;
        }
    }

    Some(Fixture {
        daemon_proc,
        address,
        _state_dir: tmp,
        fake_runtime_thread: Some(join),
        fake_shutdown_tx: Some(shutdown_tx),
    })
}

fn run_zwhisper_status(address: &str) -> std::process::Output {
    let mut cmd = Command::cargo_bin("zwhisper").expect("zwhisper bin exists");
    cmd.arg("status");
    cmd.env("DBUS_SESSION_BUS_ADDRESS", address);
    cmd.env_remove("DBUS_STARTER_BUS_TYPE");
    cmd.env_remove("DBUS_STARTER_ADDRESS");
    cmd.output().expect("run zwhisper")
}

#[test]
fn cli_refuses_mismatched_daemon_version() {
    let Some(fixture) = try_fixture(
        "cli_refuses_mismatched_daemon_version",
        Some("99.99.99".to_owned()),
    ) else {
        return;
    };
    let out = run_zwhisper_status(&fixture.address);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code().expect("CLI must exit cleanly");
    assert_eq!(code, 4, "stderr was: {stderr}");
    assert!(
        stderr.contains("daemon protocol mismatch: expected"),
        "stderr did not contain canonical mismatch line: {stderr}"
    );
    assert!(
        stderr.contains("got 99.99.99"),
        "stderr did not echo the daemon's reported version: {stderr}"
    );
}

#[test]
fn cli_refuses_legacy_daemon_without_property() {
    let Some(fixture) = try_fixture("cli_refuses_legacy_daemon_without_property", None) else {
        return;
    };
    let out = run_zwhisper_status(&fixture.address);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code().expect("CLI must exit cleanly");
    assert_eq!(code, 4, "stderr was: {stderr}");
    assert!(
        stderr.contains("got pre-0.1.0"),
        "stderr did not surface legacy-daemon sentinel: {stderr}"
    );
    assert!(
        stderr.contains("Reinstall zwhisperd"),
        "stderr did not include the reinstall hint: {stderr}"
    );
}

#[test]
fn cli_proceeds_when_daemon_version_matches() {
    let Some(fixture) = try_fixture(
        "cli_proceeds_when_daemon_version_matches",
        Some(env!("CARGO_PKG_VERSION").to_owned()),
    ) else {
        return;
    };
    let out = run_zwhisper_status(&fixture.address);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code().expect("CLI must exit cleanly");
    assert_eq!(
        code, 0,
        "expected exit 0 on matched handshake, stderr={stderr}, stdout={stdout}"
    );
    assert!(
        stdout.contains("state: idle"),
        "stdout missing idle state line: {stdout}"
    );
}
