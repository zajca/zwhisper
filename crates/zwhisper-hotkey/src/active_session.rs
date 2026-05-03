//! Active recording session lookup (reads `active-session.json`).
//!
//! This is the **read-only** counterpart to the daemon's writer
//! at `crates/zwhisperd/src/active_session.rs`. It is consumed by
//! the toggle decision (see `toggle.rs`) and by the tray's
//! reconnect-recovery path.
//!
//! ## Why a separate file at all
//!
//! The wire-frozen `Recorder1.GetStatus` returns only `(state,
//! active_profile, duration_ms)` — no `session_id`. During the
//! transcription drain window, `state` flips back to `"idle"`
//! while the daemon is still writing the WAV out and running
//! whisper-cli (the "β-light" window in the M6 architecture
//! § 1). A toggle that fires inside that window must NOT issue a
//! fresh `StartRecording` — that would either race the previous
//! session's lifecycle or surface as `RpcError::SessionInUse`.
//!
//! The `active-session.json` file lives for exactly this window:
//! the daemon writes it before emitting `StateChanged "recording"`
//! and removes it only after the terminal `StateChanged "idle"`.
//! When the toggle decision sees `Status == Idle` and the file is
//! present, it knows the daemon is still draining and emits a
//! `NoOp { reason: AlreadyDraining }` instead. See decision D1
//! in `docs/M6-plan.md`.
//!
//! ## Path resolution
//!
//! Mirrors the daemon's writer (`active_session.rs:state_file_path`):
//! honour `$XDG_STATE_HOME` first, fall back to `dirs::state_dir`,
//! then to `$HOME/.local/state`. Always with `zwhisper/active-session.json`
//! as the suffix.
//!
//! ## Schema and forward-compat
//!
//! The on-disk schema is `(schema_version, session_id, profile,
//! started_at_unix_ms)`. We surface a strongly-typed
//! [`ActiveSessionRef`] using `chrono::DateTime<Utc>` for
//! `started_at` so callers do not have to care about the
//! ms-since-epoch encoding. Unknown `schema_version` values
//! fail closed (return `None` + `tracing::warn!`).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::warn;

/// Schema version this reader understands. Mirrors the daemon's
/// `SCHEMA_VERSION` constant — bumping the daemon's value without
/// updating this is a deliberate breakage so the tray refuses to
/// consume a future-shaped file.
const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// Strongly-typed view of `active-session.json` for callers.
///
/// `started_at` is converted from the on-disk `started_at_unix_ms`
/// field so the consumer never has to think about epochs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSessionRef {
    pub session_id: String,
    pub profile: String,
    pub started_at: DateTime<Utc>,
}

/// On-disk schema. Field names MUST match the daemon's writer
/// in `crates/zwhisperd/src/active_session.rs`.
#[derive(Debug, Deserialize)]
struct OnDiskSchema {
    schema_version: u32,
    session_id: String,
    profile: String,
    started_at_unix_ms: u64,
}

/// Resolve the canonical state-file path. Mirrors the daemon's
/// writer; see module docs for the precedence order.
#[must_use]
pub fn state_file_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(dirs::state_dir)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zwhisper").join("active-session.json")
}

/// Read the canonical `active-session.json` if it exists.
///
/// Returns `None` for any of:
/// - file missing (the common steady-state case),
/// - file unreadable,
/// - JSON parse failure (corrupt / partial write),
/// - unknown `schema_version`,
/// - `started_at_unix_ms` overflowing the `i64` range used by
///   `chrono::DateTime`.
///
/// Every `Some(...)` -> `None` failure path emits a
/// `tracing::warn!` so a malformed file does not silently
/// disable the A1 fix.
#[must_use]
pub fn read_active_session() -> Option<ActiveSessionRef> {
    read_active_session_at(&state_file_path())
}

/// Test-friendly variant: read from an explicit path.
#[must_use]
pub fn read_active_session_at(path: &Path) -> Option<ActiveSessionRef> {
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Steady-state — file is supposed to be absent when
            // the daemon is not recording. NOT a warning.
            return None;
        }
        Err(e) => {
            warn!(
                error = %e,
                path = %path.display(),
                "could not read active-session.json",
            );
            return None;
        }
    };

    let parsed: OnDiskSchema = match serde_json::from_slice(&raw) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                error = %e,
                path = %path.display(),
                "could not parse active-session.json",
            );
            return None;
        }
    };

    if parsed.schema_version != SUPPORTED_SCHEMA_VERSION {
        warn!(
            on_disk = parsed.schema_version,
            supported = SUPPORTED_SCHEMA_VERSION,
            path = %path.display(),
            "active-session.json schema_version mismatch; ignoring file",
        );
        return None;
    }

    let Some(started_at) = unix_ms_to_datetime(parsed.started_at_unix_ms) else {
        warn!(
            started_at_unix_ms = parsed.started_at_unix_ms,
            path = %path.display(),
            "active-session.json started_at_unix_ms is out of range",
        );
        return None;
    };

    Some(ActiveSessionRef {
        session_id: parsed.session_id,
        profile: parsed.profile,
        started_at,
    })
}

fn unix_ms_to_datetime(ms: u64) -> Option<DateTime<Utc>> {
    let signed = i64::try_from(ms).ok()?;
    DateTime::<Utc>::from_timestamp_millis(signed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_json(contents: &str) -> NamedTempFile {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(contents.as_bytes()).unwrap();
        tmp.flush().unwrap();
        tmp
    }

    #[test]
    fn returns_none_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-existed.json");
        assert!(read_active_session_at(&path).is_none());
    }

    #[test]
    fn parses_valid_fixture() {
        let json = r#"{
            "schema_version": 1,
            "session_id": "11111111-2222-3333-4444-555555555555",
            "profile": "default",
            "started_at_unix_ms": 1714665600000
        }"#;
        let tmp = write_json(json);
        let parsed = read_active_session_at(tmp.path()).expect("should parse");
        assert_eq!(parsed.session_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(parsed.profile, "default");
        assert_eq!(parsed.started_at.timestamp_millis(), 1_714_665_600_000);
    }

    #[test]
    #[tracing_test::traced_test]
    fn returns_none_and_warns_for_corrupt_json() {
        let tmp = write_json("not json at all {{{");
        assert!(read_active_session_at(tmp.path()).is_none());
        assert!(
            logs_contain("could not parse active-session.json"),
            "expected warn line for corrupt JSON",
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn returns_none_and_warns_for_partial_json() {
        // Missing required field: `session_id`.
        let json = r#"{"schema_version": 1, "profile": "default", "started_at_unix_ms": 1}"#;
        let tmp = write_json(json);
        assert!(read_active_session_at(tmp.path()).is_none());
        assert!(
            logs_contain("could not parse active-session.json"),
            "expected warn line for partial JSON",
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn returns_none_and_warns_for_unknown_schema_version() {
        let json = r#"{
            "schema_version": 999,
            "session_id": "abc",
            "profile": "default",
            "started_at_unix_ms": 1714665600000
        }"#;
        let tmp = write_json(json);
        assert!(read_active_session_at(tmp.path()).is_none());
        assert!(
            logs_contain("schema_version mismatch"),
            "expected warn line for unknown schema_version",
        );
    }

    #[test]
    fn unix_ms_zero_is_epoch() {
        let dt = unix_ms_to_datetime(0).unwrap();
        assert_eq!(dt.timestamp_millis(), 0);
    }

    #[test]
    fn unix_ms_overflow_returns_none() {
        // u64::MAX cannot be represented as a millisecond
        // count in the chrono i64 epoch.
        assert!(unix_ms_to_datetime(u64::MAX).is_none());
    }
}
