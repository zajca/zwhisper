//! Durable session history (RFC-daemon-role Feature 2).
//!
//! The daemon is the single durable owner of session history. This
//! module holds:
//!
//! - the on-disk model (`history.json` under `$XDG_STATE_HOME/zwhisper`,
//!   NOT in `~/Recordings`) — a versioned, **rebuildable cache** over
//!   the real source of truth (the FLAC files) (F2.1);
//! - the [`writer`] task: the single serialized writer/reader that owns
//!   the file exclusively, fed via an mpsc channel (F2.2) — there is no
//!   read-modify-write from multiple callers, so concurrent jobs can
//!   never lose each other's updates;
//! - startup recovery ([`writer::recover`]) marking `transcribing`
//!   entries `interrupted` without auto-retry (F2.3);
//! - the orphan-[`reap`] helper used by recovery.
//!
//! Writes are atomic (temp + fsync + rename, `0600`), mirroring
//! `crate::last_session`.

pub(crate) mod reap;
pub(crate) mod writer;

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::warn;
use zwhisper_ipc::HistorySession;

pub(crate) use writer::{HistoryHandle, spawn_writer};

/// Schema version of `history.json`. Bumped on any breaking field
/// change; readers reject unknown *future* versions rather than
/// silently dropping fields. v1 is the floor.
pub(crate) const HISTORY_SCHEMA_VERSION: u32 = 1;

/// Lifecycle status of a recorded session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HistoryStatus {
    /// Audio captured; no transcript attempted yet.
    Recorded,
    /// A transcription job is running (or was, at the moment of a hard
    /// crash). Distinct startup-recovery input — never a terminal state.
    Transcribing,
    /// The daemon found this entry `transcribing` on startup: the prior
    /// run died mid-transcribe. NOT auto-retried (F2.3) and NOT silently
    /// `failed` — a distinct, user-visible state.
    Interrupted,
    /// Transcript produced and recorded.
    Done,
    /// Transcription failed; the FLAC is preserved for a retry.
    Failed,
}

impl HistoryStatus {
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Transcribing => "transcribing",
            Self::Interrupted => "interrupted",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// One persisted session. The richer-than-wire shape carries fields the
/// CLI never renders (codec/native_rate/channels/whisper_pid) but that a
/// future `Retry` (Phase 4) and recovery need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HistoryEntry {
    pub(crate) session_id: String,
    pub(crate) created_at_ms: u64,
    pub(crate) profile: String,
    pub(crate) audio_path: String,
    pub(crate) codec: String,
    pub(crate) native_rate: u32,
    pub(crate) channels: u16,
    /// txt first, json second when present; empty when no transcript.
    #[serde(default)]
    pub(crate) transcript_paths: Vec<String>,
    pub(crate) backend: String,
    pub(crate) model: String,
    pub(crate) lang: String,
    pub(crate) status: HistoryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
    /// PID of the `whisper-cli` subprocess while `transcribing`, so
    /// startup recovery can reap an orphan (F2.3). Cleared on terminal
    /// status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) whisper_pid: Option<u32>,
}

impl HistoryEntry {
    /// Project to the wire struct (`transcript_path` = first path or
    /// `""`; `last_error` flattened to `""`).
    pub(crate) fn to_wire(&self) -> HistorySession {
        HistorySession {
            session_id: self.session_id.clone(),
            created_at_ms: self.created_at_ms,
            profile: self.profile.clone(),
            audio_path: self.audio_path.clone(),
            backend: self.backend.clone(),
            model: self.model.clone(),
            lang: self.lang.clone(),
            status: self.status.as_wire().to_owned(),
            transcript_path: self.transcript_paths.first().cloned().unwrap_or_default(),
            last_error: self.last_error.clone().unwrap_or_default(),
        }
    }
}

/// On-disk envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HistoryFile {
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) sessions: Vec<HistoryEntry>,
}

impl Default for HistoryFile {
    fn default() -> Self {
        Self {
            schema_version: HISTORY_SCHEMA_VERSION,
            sessions: Vec::new(),
        }
    }
}

/// Errors the store can report.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HistoryError {
    #[error(
        "history schema_version {found} is newer than supported {supported}; upgrade zwhisperd"
    )]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("create state directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("read history file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse history file: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("write history file: {0}")]
    Write(#[source] std::io::Error),
    #[error("flush history file to disk: {0}")]
    Sync(#[source] std::io::Error),
    #[error("rename {from} -> {to}: {source}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Canonical path: `$XDG_STATE_HOME/zwhisper/history.json` (mirrors the
/// `last-session.json` resolution).
pub(crate) fn history_file_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(dirs::state_dir)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zwhisper").join("history.json")
}

/// Load `history.json`, applying the migration floor. A missing file is
/// not an error — it yields an empty `HistoryFile` (the index is a
/// rebuildable cache). A *newer* schema is a hard error so we never
/// truncate fields written by a future daemon.
pub(crate) fn load_from(path: &Path) -> Result<HistoryFile, HistoryError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HistoryFile::default()),
        Err(source) => {
            return Err(HistoryError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let file: HistoryFile = serde_json::from_slice(&bytes)?;
    if file.schema_version > HISTORY_SCHEMA_VERSION {
        return Err(HistoryError::UnsupportedSchema {
            found: file.schema_version,
            supported: HISTORY_SCHEMA_VERSION,
        });
    }
    // v1 is the floor; no older versions exist to migrate yet. Future
    // versions add a match arm here, mirroring profile/migrations.
    Ok(file)
}

/// Atomically persist the whole file (temp + fsync + rename, `0600`).
pub(crate) fn write_atomic_to(file: &HistoryFile, path: &Path) -> Result<(), HistoryError> {
    let parent = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    fs::create_dir_all(&parent).map_err(|source| HistoryError::CreateDir {
        path: parent.clone(),
        source,
    })?;

    let payload = serde_json::to_vec_pretty(file)?;
    let tmp_name = format!(
        "history-{pid}-{ts}.tmp",
        pid = std::process::id(),
        ts = now_unix_ms()
    );
    let tmp_path = parent.join(tmp_name);

    let mut tmp = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp_path)
        .map_err(HistoryError::Write)?;
    if let Err(e) = tmp.write_all(&payload) {
        let _ = fs::remove_file(&tmp_path);
        return Err(HistoryError::Write(e));
    }
    if let Err(e) = tmp.sync_all() {
        let _ = fs::remove_file(&tmp_path);
        return Err(HistoryError::Sync(e));
    }
    drop(tmp);
    fs::rename(&tmp_path, path).map_err(|source| HistoryError::Rename {
        from: tmp_path.clone(),
        to: path.to_path_buf(),
        source,
    })?;
    if let Ok(dir) = File::open(&parent) {
        if let Err(e) = dir.sync_all() {
            warn!(error = %e, directory = %parent.display(), "could not fsync parent dir after history.json rename");
        }
    }
    Ok(())
}

pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(id: &str, status: HistoryStatus) -> HistoryEntry {
        HistoryEntry {
            session_id: id.to_owned(),
            created_at_ms: 1,
            profile: "default".to_owned(),
            audio_path: "/tmp/a.flac".to_owned(),
            codec: "flac".to_owned(),
            native_rate: 48_000,
            channels: 1,
            transcript_paths: vec![],
            backend: "whisper-cpp".to_owned(),
            model: "small".to_owned(),
            lang: "auto".to_owned(),
            status,
            last_error: None,
            whisper_pid: None,
        }
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("history.json");
        let f = load_from(&p).unwrap();
        assert_eq!(f.schema_version, HISTORY_SCHEMA_VERSION);
        assert!(f.sessions.is_empty());
    }

    #[test]
    fn round_trips_atomically() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nested").join("history.json");
        let mut f = HistoryFile::default();
        f.sessions.push(entry("s1", HistoryStatus::Done));
        write_atomic_to(&f, &p).unwrap();
        let back = load_from(&p).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn newer_schema_is_rejected() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("history.json");
        let f = HistoryFile {
            schema_version: HISTORY_SCHEMA_VERSION + 1,
            sessions: vec![],
        };
        write_atomic_to(&f, &p).unwrap();
        let err = load_from(&p).unwrap_err();
        assert!(matches!(err, HistoryError::UnsupportedSchema { .. }));
    }

    #[test]
    fn to_wire_flattens_optionals() {
        let mut e = entry("s2", HistoryStatus::Failed);
        e.last_error = Some("boom".to_owned());
        e.transcript_paths = vec!["/t.txt".to_owned(), "/t.json".to_owned()];
        let w = e.to_wire();
        assert_eq!(w.status, "failed");
        assert_eq!(w.transcript_path, "/t.txt");
        assert_eq!(w.last_error, "boom");
    }

    #[test]
    fn no_temp_file_left_behind() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("history.json");
        write_atomic_to(&HistoryFile::default(), &p).unwrap();
        let leftover = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("history-"))
            .count();
        assert_eq!(leftover, 0);
    }
}
