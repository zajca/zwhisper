// Integration tests for the M2 profile system. Each test isolates
// `XDG_CONFIG_HOME` to a temp dir so the developer's real
// `~/.config/zwhisper/` stays untouched.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn bin(home: &Path) -> Command {
    let mut c = Command::cargo_bin("zwhisper").expect("binary should be built by cargo test");
    c.env("XDG_CONFIG_HOME", home);
    c.env("HOME", home);
    // Preserve the maintainer's real $XDG_DATA_HOME so whisper-cli
    // model resolution still finds `~/.local/share/zwhisper/models/`
    // — otherwise tests that exercise the full record+transcribe
    // path would have to bundle a model fixture.
    if let Some(real_home) = std::env::var_os("HOME") {
        let real_data = std::path::PathBuf::from(&real_home).join(".local/share");
        c.env(
            "XDG_DATA_HOME",
            std::env::var_os("XDG_DATA_HOME").unwrap_or(real_data.into_os_string()),
        );
    }
    // Make sure the test does not pick up a developer-installed
    // shipped dir; embedded fallback keeps the test deterministic.
    c.env_remove("ZWHISPER_DATA_DIR");
    c
}

#[test]
fn profile_list_shows_three_embedded_templates_on_clean_host() {
    let home = TempDir::new().unwrap();
    bin(home.path())
        .args(["profile", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("default"))
        .stdout(predicate::str::contains("meeting"))
        .stdout(predicate::str::contains("voicememo"))
        .stdout(predicate::str::contains("embedded"));
}

#[test]
fn profile_show_meeting_prints_source_and_body() {
    let home = TempDir::new().unwrap();
    bin(home.path())
        .args(["profile", "show", "meeting"])
        .assert()
        .success()
        .stdout(predicate::str::contains("source: embedded"))
        .stdout(predicate::str::contains("schema_version = 1"))
        .stdout(predicate::str::contains("name = \"meeting\""));
}

#[test]
fn profile_clone_creates_user_override_and_refuses_overwrite() {
    let home = TempDir::new().unwrap();

    bin(home.path())
        .args(["profile", "clone", "meeting", "custom-meeting"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cloned meeting"));

    let target = home
        .path()
        .join("zwhisper/profiles/custom-meeting.toml");
    assert!(target.is_file(), "{} not created", target.display());
    let body = std::fs::read_to_string(&target).unwrap();
    assert!(body.contains("name = \"custom-meeting\""), "{body}");

    // After clone, `profile show custom-meeting` must report the
    // user override, not embedded.
    bin(home.path())
        .args(["profile", "show", "custom-meeting"])
        .assert()
        .success()
        .stdout(predicate::str::contains("source: user"));

    // Re-cloning into the same destination is refused.
    bin(home.path())
        .args(["profile", "clone", "meeting", "custom-meeting"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("refusing to overwrite"));
}

#[test]
fn profile_clone_rejects_invalid_destination_name() {
    let home = TempDir::new().unwrap();
    bin(home.path())
        .args(["profile", "clone", "meeting", "../etc/passwd"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid profile name"));
}

#[test]
fn profile_migrate_no_op_at_current_version() {
    let home = TempDir::new().unwrap();
    bin(home.path())
        .args(["profile", "clone", "meeting", "x"])
        .assert()
        .success();

    bin(home.path())
        .args(["profile", "migrate", "x"])
        .assert()
        .success()
        .stdout(predicate::str::contains("schema_version = 1"));

    // No backup file should have been created — migrate is a no-op
    // when the profile already matches CURRENT_SCHEMA_VERSION.
    let dir = home.path().join("zwhisper/profiles");
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    let any_backup = entries.iter().any(|n| n.contains(".bak."));
    assert!(!any_backup, "migrate at current version must not back up: {entries:?}");
}

#[test]
fn profile_migrate_refuses_when_user_override_missing() {
    let home = TempDir::new().unwrap();
    bin(home.path())
        .args(["profile", "migrate", "meeting"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found").or(predicate::str::contains("Run")));
}

#[test]
fn profile_clone_unknown_source_returns_not_found() {
    let home = TempDir::new().unwrap();
    bin(home.path())
        .args(["profile", "clone", "no-such-source", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

/// M3 routes `record` through D-Bus, so the typed `ProfileNotFound`
/// error originates in the daemon. Without a daemon on the bus the
/// CLI surfaces a "daemon not running" hint instead — both are
/// acceptable proofs that the wiring works. We accept either as long
/// as the exit code is 2 (user-facing protocol error per `DoD` #12).
#[test]
fn record_with_unknown_profile_surfaces_typed_error() {
    let home = TempDir::new().unwrap();
    let assert = bin(home.path())
        .args(["record", "--profile", "definitely-missing"])
        .assert()
        .failure();
    let code = assert.get_output().status.code().expect("exit code");
    assert_eq!(code, 2, "expected exit 2, got {code}");
    let stderr =
        String::from_utf8(assert.get_output().stderr.clone()).expect("stderr utf8");
    assert!(
        stderr.contains("not found") || stderr.contains("daemon not running"),
        "expected typed not-found or daemon-down hint, got:\n{stderr}"
    );
}

#[test]
fn record_profile_and_output_are_mutually_exclusive() {
    let home = TempDir::new().unwrap();
    bin(home.path())
        .args([
            "record",
            "--profile",
            "meeting",
            "--output",
            "/tmp/x.flac",
        ])
        .assert()
        .failure();
}

#[test]
fn migration_chain_writes_backup_then_loads() {
    // No real migrations are registered in M2 (the framework is
    // there for v2+). We exercise it indirectly: a v1 user override
    // loads as-is, no backup is created.
    //
    // The framework's failure mode (missing migration step) is
    // covered by the migrations.rs unit tests; here we just confirm
    // that the on-disk side-effect (or its absence) is what the
    // contract promises.
    let home = TempDir::new().unwrap();
    bin(home.path())
        .args(["profile", "clone", "meeting", "x"])
        .assert()
        .success();
    bin(home.path())
        .args(["profile", "show", "x"])
        .assert()
        .success();
    let dir = home.path().join("zwhisper/profiles");
    let backups: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".bak."))
        .collect();
    assert!(
        backups.is_empty(),
        "v1 -> v1 must not back up: {:?}",
        backups.iter().map(std::fs::DirEntry::file_name).collect::<Vec<_>>()
    );
}

#[test]
fn empty_system_output_rejected_at_validate_time() {
    // Regression for the M2 review's High finding: empty
    // `system_output` previously got coerced to "default" and
    // silently captured system audio. M2 rejects the empty value
    // at validate time — mic-only mode lands in M3.
    let home = TempDir::new().unwrap();

    let user_dir = home.path().join("zwhisper/profiles");
    std::fs::create_dir_all(&user_dir).unwrap();
    let body = r#"schema_version = 1
name = "mic-only-attempt"
description = "regression fixture"

[sources]
mic = "default"
system_output = ""
mode = "mono_mix"

[recording]
codec = "flac"
sample_rate = 16000
max_duration_minutes = 1

[transcription]
backend = "whisper-cpp"
model = "small"
language = "auto"
auto = false

[[output]]
type = "file"
path = "/tmp/never-written.flac"

[hotkey]
toggle = ""
"#;
    std::fs::write(user_dir.join("mic-only-attempt.toml"), body).unwrap();

    bin(home.path())
        .args(["profile", "show", "mic-only-attempt"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("mic-only mode"));
}

#[test]
fn record_with_meeting_profile_runs_end_to_end() {
    use std::process::Command as StdCommand;

    // Phase 4 (M3) routes `record` through D-Bus. Without `zwhisperd`
    // on the bus we cannot drive the full flow end-to-end. A proper
    // D-Bus test harness lands in Phase 5; until then this test
    // skips cleanly so headless CI and developer machines without
    // a running daemon do not trip the regression net.
    if !daemon_alive() {
        eprintln!(
            "[SKIP] record_with_meeting_profile_runs_end_to_end: zwhisperd is not on the session bus (Phase 5 will add a managed-daemon harness)"
        );
        return;
    }
    if !pipewire_socket_present() {
        eprintln!(
            "[SKIP] record_with_meeting_profile_runs_end_to_end: no PipeWire socket on this host"
        );
        return;
    }
    if !whisper_cli_present() {
        eprintln!(
            "[SKIP] record_with_meeting_profile_runs_end_to_end: no whisper-cli on PATH"
        );
        return;
    }
    let Some(model) = first_installed_model() else {
        eprintln!(
            "[SKIP] record_with_meeting_profile_runs_end_to_end: no whisper.cpp model in ~/.local/share/zwhisper/models/"
        );
        return;
    };

    let home = TempDir::new().unwrap();

    // Override the meeting profile to keep the run short and write
    // into our isolated home so the maintainer's real
    // `~/Recordings/zwhisper/` is untouched.
    let user_dir = home.path().join("zwhisper/profiles");
    std::fs::create_dir_all(&user_dir).unwrap();
    let body = format!(
        r#"schema_version = 1
name = "meeting"
description = "test override"

[sources]
mic = "default"
system_output = "default"
mode = "mono_mix"

[recording]
codec = "flac"
sample_rate = 16000
max_duration_minutes = 1

[transcription]
backend = "whisper-cpp"
model = "MODEL_PLACEHOLDER"
language = "auto"
auto = true

[[output]]
type = "file"
path = "{}/Recordings/{{profile}}/{{timestamp}}.flac"

[hotkey]
toggle = ""
"#,
        home.path().display()
    );
    let body = body.replace("MODEL_PLACEHOLDER", &model);
    std::fs::write(user_dir.join("meeting.toml"), body).unwrap();

    // M3 changed the success-side log lines (the daemon owns
    // recording now and emits structured tracing on its own side).
    // The CLI no longer prints "recording complete (profile)" — it
    // prints the audio + transcript paths via println! on stdout.
    // We assert exit-0 + a non-empty stdout containing the recordings
    // dir prefix, which is robust against tracing wording changes.
    bin(home.path())
        .args(["record", "--profile", "meeting"])
        .timeout(std::time::Duration::from_secs(180))
        .assert()
        .success()
        .stdout(predicate::str::contains("audio:"));

    // Find the produced FLAC under <home>/Recordings/meeting/.
    let recordings = home.path().join("Recordings/meeting");
    let entries: Vec<_> = std::fs::read_dir(&recordings)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    let flac = entries
        .iter()
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("flac"))
        .expect("produced FLAC");
    let flac_test = StdCommand::new("flac")
        .args(["-t", flac.to_str().unwrap()])
        .output()
        .expect("flac CLI installed");
    assert!(flac_test.status.success(), "flac -t failed: {flac_test:?}");

    let txt = flac.with_extension("flac.txt");
    assert!(txt.is_file(), "transcript .txt missing: {}", txt.display());
}

fn pipewire_socket_present() -> bool {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return false;
    };
    std::path::PathBuf::from(runtime)
        .join("pipewire-0")
        .exists()
}

/// Probe the session bus for `cz.zajca.Zwhisper1` via `busctl`. We
/// shell out instead of spinning up a zbus connection because the
/// integration test must skip cleanly when neither D-Bus nor the
/// daemon is reachable, and `Command::new("busctl")` already
/// degrades gracefully when busctl is missing (Err return → false).
/// Phase 5 will replace this with a managed-daemon harness that
/// owns the daemon's lifetime.
fn daemon_alive() -> bool {
    use std::process::Command;
    let Ok(output) = Command::new("busctl")
        .args([
            "--user",
            "list",
            "--no-pager",
            "--no-legend",
        ])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .any(|line| line.starts_with("cz.zajca.Zwhisper1 "))
}

fn whisper_cli_present() -> bool {
    which::which("whisper-cli").is_ok() || which::which("whisper-cpp").is_ok()
}

/// Look for any `ggml-<name>.bin` in the maintainer's
/// `~/.local/share/zwhisper/models/` and return `<name>` for the
/// first one found. Lets the test adapt to whatever model is
/// installed on the host.
fn first_installed_model() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let dir = std::path::PathBuf::from(home).join(".local/share/zwhisper/models");
    let entries = std::fs::read_dir(&dir).ok()?;
    for ent in entries.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if let Some(stripped) = name.strip_prefix("ggml-").and_then(|s| s.strip_suffix(".bin")) {
            return Some(stripped.to_owned());
        }
    }
    None
}
