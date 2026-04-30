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
        .stdout(predicate::str::contains("not running"));
}

#[test]
fn record_is_not_implemented_yet() {
    bin()
        .args([
            "record",
            "--output",
            "/tmp/zwhisper-test.flac",
            "--duration",
            "1",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not implemented"));
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
