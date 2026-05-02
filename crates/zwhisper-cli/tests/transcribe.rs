#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

//! Phase 5b integration tests: exercise the real `whisper-cli`
//! binary (and the live `PipeWire` + `GStreamer` capture path for
//! the end-to-end test). Both tests use the project's runtime-skip
//! pattern — they print `[SKIP]` and return successfully when
//! prerequisites (`whisper-cli` on PATH, a downloaded model, a
//! reachable `PipeWire` socket) are not present, so headless CI
//! runners produce a clean green build instead of false failures.
//!
//! These tests are deliberately NOT compile-time gated behind a
//! feature flag: `cargo test --workspace` and `cargo test
//! --workspace --no-default-features` must both compile and run
//! them. The shape of the artefacts is asserted (text/JSON exist;
//! JSON has a `transcription` array) but no transcript content is
//! checked — `whisper.cpp` on silence is free to emit garbage or
//! nothing at all.

use assert_cmd::Command;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Locate the `whisper-cli` binary the way the production
/// `discovery` module does (PATH first, then `~/.local/bin`). Kept
/// as a small test-side helper because the CLI is binary-only and
/// integration tests cannot import items from it.
fn find_whisper_cli() -> Option<PathBuf> {
    for name in ["whisper-cli", "whisper-cpp"] {
        if let Ok(found) = which::which(name) {
            return Some(found);
        }
    }
    let candidate = dirs::home_dir()?.join(".local/bin/whisper-cli");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Look for a downloaded ggml model under the user's local data
/// directory. Returns the bare model name (e.g. `tiny`, `small.en`)
/// stripped of the `ggml-` prefix and `.bin` suffix — that is what
/// `--model` expects on the CLI. None means "no models on disk;
/// runtime-skip".
fn find_a_model() -> Option<String> {
    let dir = dirs::data_local_dir()?.join("zwhisper/models");
    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let raw = entry.file_name();
        let Some(name) = raw.to_str() else { continue };
        // Case-sensitive prefix/suffix here is intentional: the
        // production downloader writes lower-case `ggml-*.bin`
        // names verbatim.
        if name.starts_with("ggml-") && Path::new(name).extension() == Some(OsStr::new("bin")) {
            let stripped = name
                .trim_start_matches("ggml-")
                .trim_end_matches(".bin")
                .to_string();
            return Some(stripped);
        }
    }
    None
}

/// Mirrors the M0 test helper: presence of the `PipeWire` unix
/// socket is the canonical signal that a daemon is reachable. No
/// daemon → `[SKIP]`, no false negatives on headless CI.
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

/// `Path::with_extension` would *replace* `.flac`; we want to
/// *append* `.txt` / `.json` so the artefact lives next to
/// `silence.flac` as `silence.flac.txt` etc. Build the path by
/// pushing onto the `OsString`.
fn append_extension(base: &Path, extra: &str) -> PathBuf {
    let mut buf = base.as_os_str().to_owned();
    buf.push(extra);
    PathBuf::from(buf)
}

/// Drives `zwhisper transcribe` end-to-end against a real
/// `whisper-cli` binary on the project's silent FLAC fixture and
/// asserts both `<audio>.txt` and `<audio>.json` are written next
/// to the input. Skips with a `[SKIP]` log when `whisper-cli` is
/// not on PATH or no ggml model is present in
/// `~/.local/share/zwhisper/models/`.
#[test]
fn transcribe_writes_txt_and_json() {
    if find_whisper_cli().is_none() {
        eprintln!(
            "[SKIP] transcribe_writes_txt_and_json: whisper-cli not on PATH; install whisper.cpp to run this test"
        );
        return;
    }

    let Some(model_name) = find_a_model() else {
        eprintln!(
            "[SKIP] transcribe_writes_txt_and_json: no ggml-*.bin model in ~/.local/share/zwhisper/models/; download e.g. ggml-tiny.bin to run this test"
        );
        return;
    };

    let work = tempfile::tempdir().expect("tempdir");
    let audio = work.path().join("silence.flac");
    std::fs::copy("tests/fixtures/silence-1s.flac", &audio).expect("copy fixture");

    let output = Command::cargo_bin("zwhisper")
        .expect("binary should be built by cargo test")
        .args([
            "transcribe",
            audio.to_str().expect("utf8"),
            "--backend",
            "whisper-cpp",
            "--model",
            &model_name,
            "--language",
            "en",
        ])
        .output()
        .expect("zwhisper transcribe should run");

    assert!(
        output.status.success(),
        "zwhisper transcribe failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let txt = append_extension(&audio, ".txt");
    let json = append_extension(&audio, ".json");
    assert!(
        txt.exists(),
        "expected text artefact at {} to exist",
        txt.display()
    );
    assert!(
        json.exists(),
        "expected JSON artefact at {} to exist",
        json.display()
    );

    let body = std::fs::read_to_string(&json).expect("read JSON artefact");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("JSON must parse");
    let is_array = parsed
        .get("transcription")
        .is_some_and(serde_json::Value::is_array);
    assert!(
        is_array,
        "JSON artefact missing `transcription` array; body was:\n{body}"
    );
}

/// M3 (Phase 4) routes `record` through D-Bus, so an end-to-end
/// record+transcribe test must drive the daemon. Phase 5 will add a
/// managed-daemon harness; until then this test skips cleanly when
/// `zwhisperd` is not on the session bus, when `PipeWire` is
/// unavailable, when `whisper-cli` is missing, or when no
/// `ggml-*.bin` model is installed. Each branch prints a distinct
/// `[SKIP]` line so the reason is visible with `cargo test --
/// --nocapture`.
#[test]
fn record_then_transcribe_end_to_end() {
    if !daemon_alive() {
        eprintln!(
            "[SKIP] record_then_transcribe_end_to_end: zwhisperd is not on the session bus (Phase 5 will add a managed-daemon harness)"
        );
        return;
    }
    if !pipewire_socket_present() {
        eprintln!(
            "[SKIP] record_then_transcribe_end_to_end: no $XDG_RUNTIME_DIR/pipewire-0 socket on this host"
        );
        return;
    }

    if find_whisper_cli().is_none() {
        eprintln!(
            "[SKIP] record_then_transcribe_end_to_end: whisper-cli not on PATH; install whisper.cpp to run this test"
        );
        return;
    }

    let Some(_model_name) = find_a_model() else {
        eprintln!(
            "[SKIP] record_then_transcribe_end_to_end: no ggml-*.bin model in ~/.local/share/zwhisper/models/; download e.g. ggml-tiny.bin to run this test"
        );
        return;
    };

    // Drive via the embedded `default` profile — the M3 narrow
    // requires --profile, and `default` ships transcription.auto =
    // true so the CLI prints both audio and transcript paths.
    let output = Command::cargo_bin("zwhisper")
        .expect("binary should be built by cargo test")
        .args(["record", "--profile", "default"])
        .output()
        .expect("zwhisper record should run via D-Bus");

    assert!(
        output.status.success(),
        "record failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("audio:"),
        "expected an audio path in stdout; got:\n{stdout}"
    );
}

/// Probe the session bus for `cz.zajca.Zwhisper1` via `busctl`. See
/// `tests/profile.rs::daemon_alive` for the rationale.
fn daemon_alive() -> bool {
    let Ok(output) = Command::new("busctl")
        .args(["--user", "list", "--no-pager", "--no-legend"])
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
