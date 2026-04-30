#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use assert_cmd::Command;
use predicates::prelude::*;

fn bin() -> Command {
    Command::cargo_bin("zwhisper").expect("binary should be built by cargo test")
}

#[test]
fn prints_help() {
    bin()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("zwhisper"))
        .stdout(predicate::str::contains("record"))
        .stdout(predicate::str::contains("transcribe"));
}

#[test]
fn prints_version() {
    bin()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn status_runs_without_daemon() {
    bin()
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("walking skeleton"));
}

/// Detect whether a `PipeWire` daemon is reachable on the test
/// host. We treat the presence of `$XDG_RUNTIME_DIR/pipewire-0`
/// as the canonical signal — that is the unix socket every
/// `pipewiresrc` element ultimately connects to. Headless CI
/// runners (and the Arch sandbox jobs) do not have this socket,
/// so the live recording test cleanly skips instead of pretending
/// `PipeWire` is absent.
fn pipewire_socket_present() -> bool {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return false;
    };
    std::path::PathBuf::from(runtime)
        .join("pipewire-0")
        .exists()
}

/// End-to-end audio capture against a live `PipeWire` daemon. The
/// test is **always compiled** (so the live path keeps its callers
/// honest) and runtime-skips when no `PipeWire` socket is reachable.
/// CI without audio hardware sees a clear "[SKIP]" line instead of
/// a silent gap; the maintainer's box always exercises the full
/// encoder + filesink path. The `audio-it` feature is kept as a
/// historical marker but no longer gates compilation.
#[test]
fn record_writes_valid_flac() {
    use std::process::Command as StdCommand;

    if !pipewire_socket_present() {
        eprintln!(
            "[SKIP] record_writes_valid_flac: no $XDG_RUNTIME_DIR/pipewire-0 socket on this host"
        );
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("zwhisper-it.flac");
    bin()
        .args([
            "record",
            "--output",
            path.to_str().expect("utf8 path"),
            "--duration",
            "1",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("recording complete"));

    let flac_test = StdCommand::new("flac")
        .args(["-t", path.to_str().expect("utf8")])
        .output()
        .expect("flac CLI must be installed for audio-it tests");
    assert!(
        flac_test.status.success(),
        "flac -t rejected the output: {}",
        String::from_utf8_lossy(&flac_test.stderr)
    );
}

#[test]
fn transcribe_is_not_implemented_yet() {
    bin()
        .args(["transcribe", "/tmp/does-not-exist.flac"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not implemented"));
}

#[test]
fn record_requires_output_argument() {
    bin()
        .args(["record"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--output"));
}
