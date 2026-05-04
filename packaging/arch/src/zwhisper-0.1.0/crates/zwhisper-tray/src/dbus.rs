//! D-Bus connection helpers and `last-session.json` reader.
//!
//! The signal pump (`pump.rs`) uses [`connect_session`] to attach to
//! the user session bus and [`read_last_session`] to populate the
//! tray's "Open last recording" menu when the tray joined the bus
//! after the live signals already fired (M4-plan § "Late-start
//! handling").

use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::state::{ActiveSessionInfo, LastCompleted};

/// Connect to the user session bus.
///
/// Honours `DBUS_SESSION_BUS_ADDRESS` if set (via the implicit
/// `Builder::session()` semantics in zbus 5.15) and otherwise falls
/// back to the platform default. We use the `Builder` form rather
/// than the convenience [`zbus::Connection::session`] so future
/// phases can pass options without changing the call sites.
pub async fn connect_session() -> zbus::Result<zbus::Connection> {
    zbus::connection::Builder::session()?.build().await
}

/// Resolve the path of the daemon's `last-session.json`.
///
/// Mirrors `zwhisperd::last_session::state_file_path` byte-for-byte:
///
/// 1. If `XDG_STATE_HOME` is set and absolute, use it.
/// 2. Else fall back to `dirs::state_dir()`.
/// 3. Else fall back to `~/.local/state/`.
///
/// Then append `zwhisper/last-session.json`.
#[must_use]
pub fn last_session_path() -> PathBuf {
    last_session_path_for_xdg_state_home(std::env::var_os("XDG_STATE_HOME").as_deref())
}

/// Test-friendly variant of [`last_session_path`].
///
/// The 2024 edition makes `std::env::set_var` unsafe and the
/// workspace denies `unsafe_code`, so we expose this helper instead
/// of mutating the environment in tests. Pass `Some(value)` to
/// emulate `XDG_STATE_HOME` being set, `None` to emulate it being
/// absent.
#[must_use]
pub fn last_session_path_for_xdg_state_home(xdg_state_home: Option<&std::ffi::OsStr>) -> PathBuf {
    let base = xdg_state_home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(dirs::state_dir)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zwhisper").join("last-session.json")
}

/// Read and parse the daemon's `last-session.json`.
///
/// Returns `None` when the file is missing (debug-logged: a brand-new
/// install simply has not produced one yet) or when parsing fails
/// (warn-logged: this indicates the daemon wrote an unexpected
/// shape).
pub async fn read_last_session() -> Option<LastCompleted> {
    let path = last_session_path();
    read_last_session_at(&path).await
}

/// Test-friendly variant: read from an explicit path.
pub async fn read_last_session_at(path: &Path) -> Option<LastCompleted> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "no last-session.json on disk yet");
            return None;
        }
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "failed to read last-session.json",
            );
            return None;
        }
    };

    parse_last_session_or_log(&bytes, path)
}

fn parse_last_session_or_log(bytes: &[u8], path: &Path) -> Option<LastCompleted> {
    match LastCompleted::from_state_file_bytes(bytes) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "failed to parse last-session.json",
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// active-session.json (post-2026-05-02 review fix)
// ---------------------------------------------------------------------------

/// Resolve the path of the daemon's `active-session.json`.
///
/// Mirrors `zwhisperd::active_session::state_file_path` byte-for-byte
/// using the same `XDG_STATE_HOME` resolution as
/// [`last_session_path`].
#[must_use]
pub fn active_session_path() -> PathBuf {
    active_session_path_for_xdg_state_home(std::env::var_os("XDG_STATE_HOME").as_deref())
}

/// Test-friendly variant of [`active_session_path`].
#[must_use]
pub fn active_session_path_for_xdg_state_home(xdg_state_home: Option<&std::ffi::OsStr>) -> PathBuf {
    let base = xdg_state_home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(dirs::state_dir)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zwhisper").join("active-session.json")
}

/// Read and parse the daemon's `active-session.json`.
///
/// Returns `None` when the file is missing (no session active —
/// debug-logged) or when parsing fails (warn-logged: indicates the
/// daemon wrote an unexpected shape).
pub async fn read_active_session() -> Option<ActiveSessionInfo> {
    let path = active_session_path();
    read_active_session_at(&path).await
}

/// Test-friendly variant: read from an explicit path.
pub async fn read_active_session_at(path: &Path) -> Option<ActiveSessionInfo> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "no active-session.json on disk");
            return None;
        }
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "failed to read active-session.json",
            );
            return None;
        }
    };

    match ActiveSessionInfo::from_state_file_bytes(&bytes) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "failed to parse active-session.json",
            );
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn xdg_state_home_when_absolute_is_used() {
        let xdg = OsString::from("/custom/state");
        let p = last_session_path_for_xdg_state_home(Some(xdg.as_os_str()));
        assert_eq!(p, PathBuf::from("/custom/state/zwhisper/last-session.json"));
    }

    #[test]
    fn xdg_state_home_relative_is_ignored() {
        // `state_file_path` filters non-absolute paths the same way.
        let xdg = OsString::from("relative/path");
        let p = last_session_path_for_xdg_state_home(Some(xdg.as_os_str()));
        assert!(
            p.ends_with("zwhisper/last-session.json"),
            "got {}",
            p.display()
        );
        // The absolute filter means we should NOT have prepended
        // "relative/path" anywhere in the result.
        let s = p.to_string_lossy();
        assert!(!s.contains("relative/path"), "got {s}");
    }

    #[test]
    fn xdg_state_home_unset_falls_back() {
        let p = last_session_path_for_xdg_state_home(None);
        assert!(
            p.ends_with("zwhisper/last-session.json"),
            "got {}",
            p.display()
        );
    }

    #[tokio::test]
    async fn read_last_session_at_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert!(read_last_session_at(&path).await.is_none());
    }

    #[tokio::test]
    async fn read_last_session_at_parses_audio_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-session.json");
        let body = br#"{
            "schema_version": 1,
            "session_id": "abc",
            "audio_path": "/tmp/a.flac",
            "transcript_path": "",
            "backend": "",
            "completed_at_unix_ms": 1700000000000
        }"#;
        tokio::fs::write(&path, body).await.unwrap();
        let parsed = read_last_session_at(&path).await.unwrap();
        assert_eq!(parsed.session_id, "abc");
        assert!(parsed.transcript_path.is_none());
    }

    #[tokio::test]
    async fn read_last_session_at_invalid_json_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-session.json");
        tokio::fs::write(&path, b"not json").await.unwrap();
        assert!(read_last_session_at(&path).await.is_none());
    }

    #[test]
    fn active_session_path_resolves_under_xdg_state_home() {
        let xdg = OsString::from("/abs/state");
        let p = active_session_path_for_xdg_state_home(Some(xdg.as_os_str()));
        assert_eq!(p, PathBuf::from("/abs/state/zwhisper/active-session.json"));
    }

    #[tokio::test]
    async fn read_active_session_at_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert!(read_active_session_at(&path).await.is_none());
    }

    #[tokio::test]
    async fn read_active_session_at_parses_valid_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active-session.json");
        let body = br#"{
            "schema_version": 1,
            "session_id": "live-sid",
            "profile": "default",
            "started_at_unix_ms": 1700000000000
        }"#;
        tokio::fs::write(&path, body).await.unwrap();
        let parsed = read_active_session_at(&path).await.unwrap();
        assert_eq!(parsed.session_id, "live-sid");
    }

    #[tokio::test]
    async fn read_active_session_at_invalid_json_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active-session.json");
        tokio::fs::write(&path, b"not json").await.unwrap();
        assert!(read_active_session_at(&path).await.is_none());
    }
}
