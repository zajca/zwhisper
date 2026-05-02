//! `last-session.json` state-file writer.
//!
//! M4 binding amendment **C2**: the tray reads this file on startup
//! to populate the "Open last recording" / "Open last transcript"
//! menu entries when it joined the bus too late to receive the live
//! `RecordingComplete` / `TranscriptComplete` signals (IDEA.md § 5,
//! M4-plan § "Late-start handling").
//!
//! ## Ordering invariant (C2)
//!
//! The file MUST be flushed to disk **before** the daemon emits the
//! corresponding D-Bus signal. Otherwise a tray that does its
//! bootstrap snapshot inside the signal-delivery window reads stale
//! or empty data. The implementation enforces this by:
//!
//! 1. Writing into a sibling tempfile.
//! 2. Calling `File::sync_all()` on the tempfile handle.
//! 3. Atomically renaming the tempfile over the canonical path.
//! 4. Returning to the caller, who then emits the signal.
//!
//! The daemon's lifecycle task (`crate::lifecycle`) runs the writer
//! before each signal emission. Failure to write is logged at WARN
//! and never aborts the lifecycle: the on-disk audio + transcript
//! are still the source of truth (IDEA.md § 5 *"Single source of
//! truth zůstává soubor."*).
//!
//! ## Two-phase write
//!
//! Phase 1 (after `RecordingComplete`): `transcript_path = null`,
//! `backend = ""`. Lets the tray surface the audio file even when
//! transcription crashes mid-flight.
//!
//! Phase 2 (after `TranscriptComplete`): both paths populated.
//!
//! ## File location
//!
//! `$XDG_STATE_HOME/zwhisper/last-session.json` (defaults to
//! `~/.local/state/zwhisper/last-session.json` per the XDG Base
//! Directory spec). Permissions `0600` (file mode) to mirror the
//! `FileSink` invariant.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Schema version. Future migrations bump this; readers reject
/// unknown versions instead of silently ignoring fields.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// On-disk schema. `transcript_path` and `backend` are empty strings
/// (never `None`) on the audio-only phase to keep wire-format
/// stability across future readers — every field is always present.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct LastSession {
    pub(crate) schema_version: u32,
    pub(crate) session_id: String,
    pub(crate) audio_path: String,
    pub(crate) transcript_path: String,
    pub(crate) backend: String,
    pub(crate) completed_at_unix_ms: u64,
}

impl LastSession {
    /// Audio-only phase: emitted right after `RecordingComplete`.
    pub(crate) fn audio_only(session_id: &str, audio_path: &Path) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            session_id: session_id.to_owned(),
            audio_path: audio_path.display().to_string(),
            transcript_path: String::new(),
            backend: String::new(),
            completed_at_unix_ms: now_unix_ms(),
        }
    }

    /// Full phase: emitted right after `TranscriptComplete`.
    pub(crate) fn with_transcript(
        session_id: &str,
        audio_path: &Path,
        transcript_path: &Path,
        backend: &str,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            session_id: session_id.to_owned(),
            audio_path: audio_path.display().to_string(),
            transcript_path: transcript_path.display().to_string(),
            backend: backend.to_owned(),
            completed_at_unix_ms: now_unix_ms(),
        }
    }
}

/// Resolve the canonical state-file path. Honours `$XDG_STATE_HOME`
/// when set, otherwise falls back to `~/.local/state/`.
pub(crate) fn state_file_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(dirs::state_dir)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zwhisper").join("last-session.json")
}

/// Errors the writer can report. Tracing-friendly Display impl.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LastSessionError {
    #[error("create state directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("create temp file in {dir}: {source}")]
    CreateTemp {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("serialize state: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("write state: {0}")]
    Write(#[source] std::io::Error),
    #[error("flush state to disk: {0}")]
    Sync(#[source] std::io::Error),
    #[error("rename {from} -> {to}: {source}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Persist `state` atomically. Returns only after `fsync` on the
/// final file completes — the C2 ordering invariant is the caller's
/// responsibility (call `write_atomic` BEFORE emitting the signal).
pub(crate) fn write_atomic(state: &LastSession) -> Result<PathBuf, LastSessionError> {
    let path = state_file_path();
    write_atomic_to(state, &path)?;
    Ok(path)
}

/// Test-friendly variant: write to an explicit path so unit tests can
/// target a `tempfile::TempDir` without patching globals.
pub(crate) fn write_atomic_to(state: &LastSession, path: &Path) -> Result<(), LastSessionError> {
    let parent = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    fs::create_dir_all(&parent).map_err(|source| LastSessionError::CreateDir {
        path: parent.clone(),
        source,
    })?;

    let payload = serde_json::to_vec(state)?;

    // Open the tempfile in the same directory so the rename is
    // guaranteed to be atomic (same filesystem, same mount).
    let tmp_name = format!(
        "last-session-{pid}-{ts}.tmp",
        pid = std::process::id(),
        ts = state.completed_at_unix_ms,
    );
    let tmp_path = parent.join(tmp_name);

    let mut tmp_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp_path)
        .map_err(|source| LastSessionError::CreateTemp {
            dir: parent.clone(),
            source,
        })?;

    if let Err(source) = tmp_file.write_all(&payload) {
        let _ = fs::remove_file(&tmp_path);
        return Err(LastSessionError::Write(source));
    }

    if let Err(source) = tmp_file.sync_all() {
        let _ = fs::remove_file(&tmp_path);
        return Err(LastSessionError::Sync(source));
    }

    drop(tmp_file);

    fs::rename(&tmp_path, path).map_err(|source| LastSessionError::Rename {
        from: tmp_path.clone(),
        to: path.to_path_buf(),
        source,
    })?;

    // Best-effort fsync on the parent directory so the rename hits
    // disk too. Failures here are non-fatal: the file content is
    // durable, only the directory entry might be lost on a hard
    // crash, which we accept (transcript file itself is the source
    // of truth).
    if let Ok(dir) = File::open(&parent) {
        if let Err(e) = dir.sync_all() {
            warn!(
                error = %e,
                directory = %parent.display(),
                "could not fsync parent dir after last-session.json rename",
            );
        }
    }

    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn audio_only_serializes_with_empty_transcript_fields() {
        let s = LastSession::audio_only("abc", Path::new("/tmp/x.flac"));
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"transcript_path\":\"\""));
        assert!(json.contains("\"backend\":\"\""));
    }

    #[test]
    fn write_atomic_creates_file_with_0600_perms() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("last-session.json");
        let state = LastSession::audio_only("sid", Path::new("/tmp/x.flac"));

        write_atomic_to(&state, &path).unwrap();

        let meta = fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[test]
    fn write_atomic_round_trips_full_state() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("last-session.json");
        let state = LastSession::with_transcript(
            "sid-42",
            Path::new("/tmp/audio.flac"),
            Path::new("/tmp/audio.flac.txt"),
            "whisper-cli",
        );

        write_atomic_to(&state, &path).unwrap();

        let read: LastSession = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(read, state);
        assert_eq!(read.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn write_atomic_overwrites_previous_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("last-session.json");

        let first = LastSession::audio_only("first", Path::new("/tmp/a.flac"));
        write_atomic_to(&first, &path).unwrap();

        let second = LastSession::with_transcript(
            "first",
            Path::new("/tmp/a.flac"),
            Path::new("/tmp/a.flac.txt"),
            "whisper-cli",
        );
        write_atomic_to(&second, &path).unwrap();

        let read: LastSession = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(read, second);
    }

    #[test]
    fn no_temp_file_left_behind_on_success() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("last-session.json");
        let state = LastSession::audio_only("sid", Path::new("/tmp/x.flac"));

        write_atomic_to(&state, &path).unwrap();

        let leftover = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("last-session-"))
            .count();
        assert_eq!(leftover, 0);
    }
}
