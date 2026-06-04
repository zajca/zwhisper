#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use assert_cmd::Command;
use predicates::prelude::*;

fn bin() -> Command {
    let mut cmd = Command::cargo_bin("zwhisper").expect("binary should be built by cargo test");
    let state_dir = std::env::temp_dir().join("zwhisper-cli-tests-state");
    std::fs::create_dir_all(&state_dir).expect("test state dir should be writable");
    cmd.env("XDG_STATE_HOME", state_dir);
    cmd
}

#[test]
fn prints_help() {
    bin()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("zwhisper"))
        .stdout(predicate::str::contains("record"))
        .stdout(predicate::str::contains("model"))
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

/// Phase 4 (M3) replaces the M2 placeholder string with a daemon
/// RPC. Developer machines may have a live daemon; otherwise the CLI
/// prints the actionable "daemon not running" hint to stderr and exits
/// 2 (per `DoD` #12 the "user-facing protocol error" code).
#[test]
fn status_reports_live_daemon_or_actionable_hint() {
    let assert = bin().arg("status").assert();
    let output = assert.get_output();
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout should be utf8");
    let stderr = String::from_utf8(output.stderr.clone()).expect("stderr should be utf8");

    if output.status.success() {
        assert!(stdout.contains("state:"), "stdout missing state:\n{stdout}");
        return;
    }

    let code = output.status.code().expect("exit code");
    assert_eq!(code, 2, "expected exit 2 when daemon is down, got {code}");
    assert!(
        stderr.contains("daemon not running"),
        "stderr missing 'daemon not running' hint:\n{stderr}"
    );
    assert!(
        stderr.contains("systemctl --user start zwhisperd")
            || stderr.contains("cz.zajca.Zwhisper1.service"),
        "stderr missing actionable systemctl/activation hint:\n{stderr}"
    );
}

/// Phase 4 narrowed `record` to require `--profile`. The bare-flag
/// invocation (`--output --duration ...`) still parses at the clap
/// layer (so the regression-net tests stay green) but the runtime
/// dispatcher returns exit 2 with a hint pointing at `--profile
/// default`. The previous live-FLAC test moved into
/// `tests/profile.rs::record_with_meeting_profile_runs_end_to_end`
/// (which already skips cleanly when no daemon / `PipeWire`).
#[test]
fn record_without_profile_returns_exit_2() {
    let assert = bin()
        .args(["record", "--output", "/tmp/x.flac", "--duration", "1"])
        .assert()
        .failure();
    let code = assert.get_output().status.code().expect("exit code");
    assert_eq!(
        code, 2,
        "expected exit 2 for M3 narrow violation, got {code}"
    );
    let stderr =
        String::from_utf8(assert.get_output().stderr.clone()).expect("stderr should be utf8");
    assert!(
        stderr.contains("--profile"),
        "stderr missing --profile hint:\n{stderr}"
    );
}

/// `transcribe` against a missing file must surface a typed error
/// from the façade — not the old "not implemented" `bail!` and not a
/// panic. We do not pin the exact variant here because the failure
/// host-dependently splits across `BackendUnavailable` (no whisper-cli
/// on PATH) vs `InputAudio` (audio missing) — both are acceptable
/// proofs that Phase 4 wired the call all the way to the backend.
#[test]
fn transcribe_missing_input_returns_typed_error() {
    let assert = bin()
        .args(["transcribe", "/tmp/zwhisper-does-not-exist.flac"])
        .assert()
        .failure();
    let stderr =
        String::from_utf8(assert.get_output().stderr.clone()).expect("stderr should be utf8");
    assert!(
        !stderr.contains("not implemented"),
        "Phase 4 should have removed the placeholder bail message; stderr was:\n{stderr}"
    );
    let acceptable = stderr.contains("failed to open audio file")
        || stderr.contains("no whisper.cpp binary found")
        || stderr.contains("model")
        || stderr.contains("whisper.cpp");
    assert!(
        acceptable,
        "expected a typed transcribe error in stderr; got:\n{stderr}"
    );
}

/// Unknown backend ids must be rejected by the façade up-front,
/// before any subprocess work — so this test is reliable on every
/// host regardless of whether whisper-cli is installed.
#[test]
fn transcribe_unknown_backend_returns_backend_unknown_error() {
    let assert = bin()
        .args([
            "transcribe",
            "/tmp/zwhisper-does-not-exist.flac",
            "--backend",
            "foobar",
            "--model",
            "small",
            "--language",
            "en",
        ])
        .assert()
        .failure();
    let stderr =
        String::from_utf8(assert.get_output().stderr.clone()).expect("stderr should be utf8");
    assert!(
        stderr.contains("unknown backend"),
        "expected `unknown backend` in stderr; got:\n{stderr}"
    );
    assert!(
        stderr.contains("whisper-cpp"),
        "expected supported set listing `whisper-cpp` in stderr; got:\n{stderr}"
    );
}

#[test]
fn model_list_prints_known_manifest_models() {
    bin()
        .args(["model", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("tiny"))
        .stdout(predicate::str::contains("large-v3"));
}

#[test]
fn model_path_prints_expected_model_file() {
    bin()
        .args(["model", "path", "tiny"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ggml-tiny.bin"));
}

#[test]
fn model_path_rejects_unknown_model() {
    bin()
        .args(["model", "path", "not-a-known-model"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown model"));
}

#[test]
fn record_requires_output_argument() {
    bin()
        .args(["record"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--output"));
}

// ===========================================================================
// RFC-mic-setup — `audio {devices,meter,calibrate}` (Wave 2A).
//
// These exercise the clap surface only (`--help` parsing + subcommand
// listing + flag visibility). They run on every host because they never
// touch a live PipeWire — the actual pw-cat / wpctl behaviour is
// hardware-verified by the parent, not here.
// ===========================================================================

#[cfg(feature = "setup")]
#[test]
fn audio_help_lists_subcommands() {
    bin()
        .args(["audio", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("devices"))
        .stdout(predicate::str::contains("meter"))
        .stdout(predicate::str::contains("calibrate"));
}

#[cfg(feature = "setup")]
#[test]
fn audio_is_listed_in_top_level_help() {
    bin()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("audio"));
}

#[cfg(feature = "setup")]
#[test]
fn audio_devices_help_mentions_json_flag() {
    bin()
        .args(["audio", "devices", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

#[cfg(feature = "setup")]
#[test]
fn audio_meter_help_mentions_source_flag() {
    bin()
        .args(["audio", "meter", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--source"));
}

#[cfg(feature = "setup")]
#[test]
fn audio_calibrate_help_shows_all_flags() {
    bin()
        .args(["audio", "calibrate", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--source"))
        .stdout(predicate::str::contains("--profile"))
        .stdout(predicate::str::contains("--target-peak-db"))
        .stdout(predicate::str::contains("--seconds"))
        .stdout(predicate::str::contains("--apply"))
        .stdout(predicate::str::contains("--set-default"))
        .stdout(predicate::str::contains("--max-volume"));
}

#[cfg(feature = "setup")]
#[test]
fn audio_help_lists_setup_subcommand() {
    bin()
        .args(["audio", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("setup"));
}

#[cfg(feature = "setup")]
#[test]
fn audio_setup_help_shows_optional_flags() {
    bin()
        .args(["audio", "setup", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--profile"))
        .stdout(predicate::str::contains("--target-peak-db"))
        .stdout(predicate::str::contains("--max-volume"));
}

#[cfg(feature = "setup")]
#[test]
fn audio_requires_a_subcommand() {
    bin()
        .arg("audio")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}
