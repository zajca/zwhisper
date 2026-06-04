//! `zwhisper output last --to clipboard|notify` — manual one-shot
//! delivery of the most recent finished transcript (RFC-daemon-role F3.2).
//!
//! This is the user's escape hatch when the best-effort `deliver --listen`
//! consumer missed a transcript (it was not running, the session was not
//! graphical, the notification was dismissed, …). It reads the daemon's
//! `last-session.json` state file — the SAME file the daemon writes after
//! every `TranscriptComplete` — loads the referenced transcript, and
//! delivers it to the chosen target.
//!
//! It deliberately does NOT touch the daemon or D-Bus: the state file is
//! the single source of truth for "the last thing that finished", so this
//! command works even when the daemon is down.

use std::path::PathBuf;

use serde::Deserialize;

use super::{EXIT_OK, EXIT_PROTOCOL_ERROR};
use crate::cli::{OutputCmd, OutputTarget};
use crate::commands::deliver::sink;

/// Local mirror of the daemon's `LastSession` on-disk schema
/// (`crates/zwhisperd/src/last_session.rs`). Only the fields this command
/// needs are decoded; we keep the rest to document the contract and to
/// fail loudly if `transcript_path`/`backend` ever change type.
/// `transcript_path` and `backend` are empty strings (never absent) on the
/// audio-only phase.
#[derive(Debug, Deserialize)]
struct LastSession {
    #[allow(dead_code)]
    schema_version: u32,
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    audio_path: String,
    transcript_path: String,
    #[allow(dead_code)]
    backend: String,
    #[allow(dead_code)]
    completed_at_unix_ms: u64,
}

/// Resolve the canonical `last-session.json` path. Replicates the daemon's
/// `last_session::state_file_path()` resolution byte-for-byte: honour an
/// absolute `$XDG_STATE_HOME`, else `dirs::state_dir()`, else
/// `~/.local/state`, else the current directory.
fn state_file_path() -> PathBuf {
    resolve_state_file_path(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        dirs::state_dir(),
        dirs::home_dir(),
    )
}

/// Pure path-resolution core, factored out so the precedence rules can be
/// unit-tested without mutating process env (workspace denies `unsafe`, so
/// `std::env::set_var` is off-limits in tests). Precedence mirrors the
/// daemon exactly: an ABSOLUTE `XDG_STATE_HOME` wins; a relative one is
/// rejected (falls through); then `state_dir`; then `~/.local/state`; then
/// the current directory as a last resort.
fn resolve_state_file_path(
    xdg_state_home: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
) -> PathBuf {
    let base = xdg_state_home
        .filter(|p| p.is_absolute())
        .or(state_dir)
        .or_else(|| home_dir.map(|h| h.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zwhisper").join("last-session.json")
}

/// Synchronous entry point. Like the other CLI commands, we build a
/// one-shot current-thread runtime and translate the async result into a
/// process exit code so we get the full exit-code spread the contract
/// needs (0 success, 2 "nothing to deliver" / read error).
pub(crate) fn run(cmd: &OutputCmd) -> color_eyre::Result<()> {
    let rt = super::build_runtime()?;
    let code = rt.block_on(run_async(cmd));
    if code == EXIT_OK {
        Ok(())
    } else {
        std::process::exit(code);
    }
}

#[allow(clippy::print_stderr)]
async fn run_async(cmd: &OutputCmd) -> i32 {
    let OutputCmd::Last { to } = cmd;

    let path = state_file_path();
    let raw = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("no last session found at {}: {err}", path.display());
            return EXIT_PROTOCOL_ERROR;
        }
    };

    let session: LastSession = match serde_json::from_slice(&raw) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("could not parse {}: {err}", path.display());
            return EXIT_PROTOCOL_ERROR;
        }
    };

    if session.transcript_path.is_empty() {
        // Audio-only phase: a recording finished but transcription did not
        // (yet) produce a transcript. There is nothing to copy/notify.
        eprintln!("no transcript in last session (audio-only; transcription did not complete)");
        return EXIT_PROTOCOL_ERROR;
    }

    let text = match tokio::fs::read_to_string(&session.transcript_path).await {
        Ok(t) => t,
        Err(err) => {
            eprintln!(
                "could not read transcript {}: {err}",
                session.transcript_path
            );
            return EXIT_PROTOCOL_ERROR;
        }
    };

    match to {
        OutputTarget::Clipboard => deliver_clipboard(&text, &session.transcript_path).await,
        OutputTarget::Notify => {
            sink::notify(
                "Transcript ready",
                &format!("Last transcript: {}", session.transcript_path),
            )
            .await;
            EXIT_OK
        }
    }
}

/// Copy `text` into the clipboard via the shared [`sink::ClipboardSink`].
///
/// ## Wayland one-shot limitation (honest caveat)
///
/// `arboard` on Wayland serves the clipboard from the OWNING process: the
/// selection survives only as long as that process lives. A one-shot CLI
/// that calls `set_text` and then exits hands ownership back to the
/// compositor, which on most wlroots-based compositors drops the selection
/// the instant we exit — a subsequent paste then yields nothing. There is
/// no portable way to "detach" the selection from a short-lived process.
///
/// We still perform the `set_text` (it works on desktops with a clipboard
/// manager that takes over the selection, e.g. `wl-clip-persist`,
/// `clipman`, or most X11 setups), and we tell the user about the robust
/// path: keep the `zwhisper deliver --listen` consumer running, which
/// holds its clipboard handle for the whole session (C1) and does not have
/// this problem.
#[allow(clippy::print_stdout)]
async fn deliver_clipboard(text: &str, transcript_path: &str) -> i32 {
    let clipboard = sink::ClipboardSink::new();
    if let Err(err) = clipboard.inject(text).await {
        #[allow(clippy::print_stderr)]
        {
            eprintln!("clipboard injection failed: {err}");
        }
        return EXIT_PROTOCOL_ERROR;
    }
    println!("copied last transcript to clipboard ({transcript_path})");
    // Honest note: on Wayland without a clipboard manager the selection may
    // not survive this process exiting. The deliver --listen daemon is the
    // robust path (it holds the clipboard handle for the session).
    println!(
        "note: on Wayland the selection may not persist after this command exits unless a clipboard manager is running; the `zwhisper deliver --listen` consumer is the robust path."
    );
    EXIT_OK
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn resolve_honours_absolute_xdg_state_home() {
        let path = resolve_state_file_path(
            Some(PathBuf::from("/custom/state")),
            Some(PathBuf::from("/should/be/ignored")),
            Some(PathBuf::from("/home/u")),
        );
        assert_eq!(
            path,
            PathBuf::from("/custom/state/zwhisper/last-session.json")
        );
    }

    #[test]
    fn resolve_ignores_relative_xdg_state_home_and_uses_state_dir() {
        // A relative XDG_STATE_HOME is rejected (mirrors the daemon), so
        // resolution falls through to `state_dir`.
        let path = resolve_state_file_path(
            Some(PathBuf::from("relative/path")),
            Some(PathBuf::from("/var/lib/state")),
            Some(PathBuf::from("/home/u")),
        );
        assert_eq!(
            path,
            PathBuf::from("/var/lib/state/zwhisper/last-session.json")
        );
    }

    #[test]
    fn resolve_falls_back_to_home_local_state() {
        let path = resolve_state_file_path(None, None, Some(PathBuf::from("/home/u")));
        assert_eq!(
            path,
            PathBuf::from("/home/u/.local/state/zwhisper/last-session.json")
        );
    }

    #[test]
    fn resolve_last_resort_is_current_dir() {
        let path = resolve_state_file_path(None, None, None);
        assert_eq!(path, PathBuf::from("./zwhisper/last-session.json"));
    }

    #[test]
    fn last_session_audio_only_has_empty_transcript_path() {
        let json = r#"{
            "schema_version": 1,
            "session_id": "abc",
            "audio_path": "/tmp/a.flac",
            "transcript_path": "",
            "backend": "",
            "completed_at_unix_ms": 0
        }"#;
        let s: LastSession = serde_json::from_str(json).unwrap();
        assert!(s.transcript_path.is_empty());
    }

    #[test]
    fn last_session_full_round_trips() {
        let json = r#"{
            "schema_version": 1,
            "session_id": "abc",
            "audio_path": "/tmp/a.flac",
            "transcript_path": "/tmp/a.flac.txt",
            "backend": "whisper-cli",
            "completed_at_unix_ms": 1700000000000
        }"#;
        let s: LastSession = serde_json::from_str(json).unwrap();
        assert_eq!(s.transcript_path, "/tmp/a.flac.txt");
        assert_eq!(s.backend, "whisper-cli");
    }
}
