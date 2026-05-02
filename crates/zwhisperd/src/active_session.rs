//! `active-session.json` state-file writer.
//!
//! ## Why this exists (post-2026-05-02 review fix)
//!
//! The wire-frozen [`Status`](zwhisper_ipc::Status) struct returned
//! by `Recorder1.GetStatus` does NOT carry the active session id
//! (its signature is `(s state, s active_profile, t duration_ms)`).
//! That is fine in steady-state, because clients learn the
//! `session_id` from the `StateChanged "recording"` signal. But a
//! tray that crashes mid-recording and reconnects later sees
//! `state == "recording"` from the snapshot and has no way to
//! recover the session id — `Stop recording` from the menu would
//! then send `stop_recording("")` to the daemon, which would
//! correctly reject it as `SessionUnknown`.
//!
//! Without changing the wire format (M3 is locked), we mirror the
//! C2 pattern used for `last-session.json`: write a state file to
//! `$XDG_STATE_HOME/zwhisper/active-session.json` BEFORE emitting
//! `StateChanged "recording"`, and remove it after the terminal
//! `StateChanged "idle"` / `"failed"`. The tray reads it on
//! snapshot when it observes `state == "recording" | "stopping"`.
//!
//! ## Ordering invariant
//!
//! Identical to C2: write atomically (tempfile + `fsync` + rename)
//! BEFORE the signal emit. A tray that bootstraps inside the
//! signal-delivery window therefore sees the file matching the
//! signal it just received.
//!
//! ## File location and permissions
//!
//! `$XDG_STATE_HOME/zwhisper/active-session.json` (defaults to
//! `~/.local/state/zwhisper/`). Permissions `0o600` to match the
//! `FileSink` invariant.
//!
//! ## Lifetime
//!
//! - Write right before `StateChanged "recording"` is emitted.
//! - Remove (best-effort) right after the terminal `StateChanged
//!   "idle"` or `"failed"` is emitted by the lifecycle task.
//! - Failure to remove is logged at WARN; a stale file is harmless
//!   because the tray's snapshot only consults it when
//!   `state == "recording" | "stopping"`.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Schema version for forward-compat. Readers reject unknown
/// versions instead of silently ignoring fields.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// On-disk schema. All fields always present (no `Option`s) so the
/// reader can rely on every key existing — adding optional fields
/// in the future is a schema-version bump.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ActiveSession {
    pub(crate) schema_version: u32,
    pub(crate) session_id: String,
    pub(crate) profile: String,
    pub(crate) started_at_unix_ms: u64,
}

impl ActiveSession {
    pub(crate) fn new(session_id: &str, profile: &str) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            session_id: session_id.to_owned(),
            profile: profile.to_owned(),
            started_at_unix_ms: now_unix_ms(),
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
    base.join("zwhisper").join("active-session.json")
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ActiveSessionError {
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

/// Persist `state` atomically, returning only after `fsync`
/// completes. The caller is responsible for the C2-style ordering
/// (call BEFORE emitting `StateChanged "recording"`).
pub(crate) fn write_atomic(state: &ActiveSession) -> Result<PathBuf, ActiveSessionError> {
    let path = state_file_path();
    write_atomic_to(state, &path)?;
    Ok(path)
}

/// Test-friendly variant: write to an explicit path so unit tests
/// can target a `tempfile::TempDir` without env mutation.
pub(crate) fn write_atomic_to(
    state: &ActiveSession,
    path: &Path,
) -> Result<(), ActiveSessionError> {
    let parent = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    fs::create_dir_all(&parent).map_err(|source| ActiveSessionError::CreateDir {
        path: parent.clone(),
        source,
    })?;

    let payload = serde_json::to_vec(state)?;

    let tmp_name = format!(
        "active-session-{pid}-{ts}.tmp",
        pid = std::process::id(),
        ts = state.started_at_unix_ms,
    );
    let tmp_path = parent.join(tmp_name);

    let mut tmp_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp_path)
        .map_err(|source| ActiveSessionError::CreateTemp {
            dir: parent.clone(),
            source,
        })?;

    if let Err(source) = tmp_file.write_all(&payload) {
        let _ = fs::remove_file(&tmp_path);
        return Err(ActiveSessionError::Write(source));
    }

    if let Err(source) = tmp_file.sync_all() {
        let _ = fs::remove_file(&tmp_path);
        return Err(ActiveSessionError::Sync(source));
    }

    drop(tmp_file);

    fs::rename(&tmp_path, path).map_err(|source| ActiveSessionError::Rename {
        from: tmp_path.clone(),
        to: path.to_path_buf(),
        source,
    })?;

    if let Ok(dir) = File::open(&parent) {
        if let Err(e) = dir.sync_all() {
            warn!(
                error = %e,
                directory = %parent.display(),
                "could not fsync parent dir after active-session.json rename",
            );
        }
    }

    Ok(())
}

/// Best-effort removal. Called on terminal `StateChanged`. Failure
/// to remove is logged but does NOT propagate — a stale file is
/// only consulted by the tray when state is `recording`/`stopping`,
/// which won't be the case after the terminal emit.
pub(crate) fn clear() {
    let path = state_file_path();
    clear_at(&path);
}

pub(crate) fn clear_at(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            warn!(
                error = %e,
                path = %path.display(),
                "could not remove active-session.json",
            );
        }
    }
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
    fn new_populates_required_fields() {
        let s = ActiveSession::new("sid", "default");
        assert_eq!(s.schema_version, SCHEMA_VERSION);
        assert_eq!(s.session_id, "sid");
        assert_eq!(s.profile, "default");
        assert!(s.started_at_unix_ms > 0);
    }

    #[test]
    fn write_atomic_creates_file_with_0600_perms() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("active-session.json");
        let state = ActiveSession::new("sid", "default");

        write_atomic_to(&state, &path).unwrap();

        let meta = fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn write_atomic_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("active-session.json");
        let state = ActiveSession::new("sid-7", "meeting");

        write_atomic_to(&state, &path).unwrap();

        let read: ActiveSession = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(read, state);
    }

    #[test]
    fn clear_removes_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("active-session.json");
        let state = ActiveSession::new("sid", "default");
        write_atomic_to(&state, &path).unwrap();

        assert!(path.exists());
        clear_at(&path);
        assert!(!path.exists());
    }

    #[test]
    fn clear_on_missing_file_is_noop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("never-existed.json");
        // Must not panic / log error at higher than warn level.
        clear_at(&path);
    }

    #[test]
    fn no_temp_file_left_behind_on_success() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("active-session.json");
        let state = ActiveSession::new("sid", "default");
        write_atomic_to(&state, &path).unwrap();

        let leftover = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("active-session-")
            })
            .count();
        assert_eq!(leftover, 0);
    }
}
