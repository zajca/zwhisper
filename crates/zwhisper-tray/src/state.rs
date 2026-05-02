//! Tray-side state model.
//!
//! The tray is a thin client of the M3 daemon: it receives signals
//! over D-Bus, mutates an in-memory [`TrayState`], and broadcasts the
//! new snapshot to the renderer via a `tokio::sync::watch` channel.
//!
//! This module contains:
//!
//! - [`IconState`] — the user-visible state machine (mirrors the
//!   daemon's wire strings, but adds a tray-only `DaemonOffline`
//!   variant for when the bus name has no owner — see M4-plan §
//!   "Late-start handling").
//! - [`LastCompleted`] — tray-side mirror of the daemon's
//!   `LastSession` JSON schema (see
//!   `crates/zwhisperd/src/last_session.rs`). Kept tray-local so the
//!   tray binary does not depend on `zwhisperd` (M4-plan § "Crate
//!   dependency graph").
//! - [`TrayState`] — the full in-memory snapshot the renderer reads.
//! - [`PendingCmd`] — placeholder for Phase P4's command pipeline.
//! - Pure-function reducers (`apply_*`) that mutate `TrayState` from
//!   incoming signals. Reducers are pure so they can be unit-tested
//!   without a D-Bus fixture.

use std::path::PathBuf;
use std::time::Instant;

use serde::Deserialize;
use thiserror::Error;
use zwhisper_ipc::ProfileEntry;

/// User-visible state of the daemon, as rendered by the tray icon.
///
/// The five wire strings (`idle`, `starting`, `recording`, `stopping`,
/// `failed`) come straight from the daemon's `Recorder1.StateChanged`
/// signal — see `zwhisper-core::audio::state::RecorderState::Display`.
/// `DaemonOffline` is tray-local: emitted by the signal pump when the
/// bus name has no owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Idle,
    Starting,
    Recording,
    Stopping,
    Failed,
    DaemonOffline,
}

impl IconState {
    /// Map a daemon wire string to an [`IconState`]. Unknown strings
    /// fall back to [`IconState::Failed`] so a forward-incompatible
    /// daemon never silently appears healthy.
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            "idle" => Self::Idle,
            "starting" => Self::Starting,
            "recording" => Self::Recording,
            "stopping" => Self::Stopping,
            // Anything else (including "failed") is rendered as
            // `Failed` — clippy collapses these arms because they
            // share a body, and that's the intended semantics.
            _ => Self::Failed,
        }
    }
}

/// On-disk schema mirror of `zwhisperd::last_session::LastSession`.
///
/// The tray reads this file on startup to populate "Open last
/// recording" / "Open last transcript" when it joined the bus too
/// late to receive the live `RecordingComplete` /
/// `TranscriptComplete` signals (M4-plan § "Late-start handling").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastCompleted {
    pub session_id: String,
    pub audio_path: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub backend: Option<String>,
    pub completed_at_unix_ms: u64,
}

/// Errors returned by [`LastCompleted::from_state_file_bytes`].
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("decode last-session.json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported schema_version {0} (tray expects 1)")]
    UnsupportedSchemaVersion(u32),
    #[error("audio_path is empty")]
    EmptyAudioPath,
}

#[derive(Debug, Deserialize)]
struct LastSessionWire {
    schema_version: u32,
    session_id: String,
    audio_path: String,
    transcript_path: String,
    backend: String,
    completed_at_unix_ms: u64,
}

impl LastCompleted {
    /// Parse the bytes of a `last-session.json` file produced by the
    /// daemon. Empty `transcript_path` / `backend` (audio-only phase)
    /// are normalised to `None` here.
    pub fn from_state_file_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        let wire: LastSessionWire = serde_json::from_slice(bytes)?;

        if wire.schema_version != 1 {
            return Err(ParseError::UnsupportedSchemaVersion(wire.schema_version));
        }
        if wire.audio_path.is_empty() {
            return Err(ParseError::EmptyAudioPath);
        }

        let transcript_path = if wire.transcript_path.is_empty() {
            None
        } else {
            Some(PathBuf::from(wire.transcript_path))
        };
        let backend = if wire.backend.is_empty() {
            None
        } else {
            Some(wire.backend)
        };

        Ok(Self {
            session_id: wire.session_id,
            audio_path: PathBuf::from(wire.audio_path),
            transcript_path,
            backend,
            completed_at_unix_ms: wire.completed_at_unix_ms,
        })
    }
}

/// Phase P4 command pipeline — payload for the menu->dispatcher
/// channel.
///
/// The first three variants (`Start`, `Stop`, `SetActiveProfile`)
/// correspond to RPCs that mutate daemon state and therefore
/// participate in the optimistic action lock (`DoD` #21): the
/// dispatcher sets [`TrayState::pending_cmd`] before firing the RPC,
/// and `apply_state_changed` clears it once the matching
/// `StateChanged` arrives.
///
/// The last two variants (`OpenLastRecording`, `OpenLastTranscript`)
/// are tray-local — they spawn `xdg-open` against a path stored in
/// `last_session` and never touch daemon state. They do NOT
/// participate in the optimistic action lock and therefore are not
/// observed by `apply_state_changed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingCmd {
    Start,
    Stop,
    SetActiveProfile {
        name: String,
    },
    /// Spawn `xdg-open` against `last_session.audio_path`.
    OpenLastRecording,
    /// Spawn `xdg-open` against `last_session.transcript_path` (if
    /// present).
    OpenLastTranscript,
}

/// Snapshot of everything the tray renderer needs to draw a frame.
///
/// The signal pump owns one `Sender<TrayState>` and pushes a fresh
/// `TrayState` whenever any field changes. Renderer / sinks read it
/// via `Receiver<TrayState>::borrow()`.
#[derive(Debug, Clone)]
pub struct TrayState {
    pub icon: IconState,
    pub active_profile: String,
    pub active_session_id: Option<String>,
    pub recording_started_at: Option<Instant>,
    pub last_session: Option<LastCompleted>,
    pub profiles: Vec<ProfileEntry>,
    pub pending_cmd: Option<PendingCmd>,
}

impl Default for TrayState {
    fn default() -> Self {
        Self {
            icon: IconState::DaemonOffline,
            active_profile: String::new(),
            active_session_id: None,
            recording_started_at: None,
            last_session: None,
            profiles: Vec::new(),
            pending_cmd: None,
        }
    }
}

/// Reducer for `Recorder1.StateChanged`.
///
/// - On entry to `recording`, sets `recording_started_at` to `now`.
/// - On entry to `idle`/`failed`, clears `recording_started_at` and
///   `active_session_id`.
/// - On entry to `stopping`, keeps `recording_started_at` so the UI
///   can keep showing the elapsed timer until the file lands.
/// - Clears `pending_cmd` when the new state matches what was
///   pending (Start ↔ recording / starting, Stop ↔ idle / stopping).
pub fn apply_state_changed(state: &mut TrayState, new_state: &str, session_id: &str) {
    let new_icon = IconState::from_wire(new_state);

    match new_icon {
        IconState::Recording => {
            if state.recording_started_at.is_none() {
                state.recording_started_at = Some(Instant::now());
            }
            if !session_id.is_empty() {
                state.active_session_id = Some(session_id.to_owned());
            }
        }
        IconState::Starting | IconState::Stopping => {
            if !session_id.is_empty() {
                state.active_session_id = Some(session_id.to_owned());
            }
        }
        IconState::Idle | IconState::Failed => {
            state.recording_started_at = None;
            state.active_session_id = None;
        }
        IconState::DaemonOffline => {
            // The daemon never sends "DaemonOffline" over the wire —
            // unknown wire strings funnel into `Failed`. This branch
            // exists only because the enum is non-exhaustive at the
            // type level.
        }
    }

    state.icon = new_icon;

    if let Some(pending) = state.pending_cmd.as_ref() {
        let matches = matches!(
            (pending, new_icon),
            (
                PendingCmd::Start,
                IconState::Starting | IconState::Recording
            ) | (PendingCmd::Stop, IconState::Stopping | IconState::Idle)
        );
        if matches {
            state.pending_cmd = None;
        }
    }
}

/// Reducer for `Recorder1.RecordingComplete`. Updates the
/// `last_session` slot with an audio-only mirror.
pub fn apply_recording_complete(
    state: &mut TrayState,
    session_id: &str,
    audio_path: &str,
    completed_at_unix_ms: u64,
) {
    state.last_session = Some(LastCompleted {
        session_id: session_id.to_owned(),
        audio_path: PathBuf::from(audio_path),
        transcript_path: None,
        backend: None,
        completed_at_unix_ms,
    });
}

/// Reducer for `Recorder1.TranscriptComplete`. Replaces the
/// `last_session` slot with a fully-populated mirror.
///
/// **`audio_path` resolution.** The signal payload itself does NOT
/// carry the audio path — only the transcript path, the bytes, and
/// the backend. The caller MUST supply the audio path via
/// `audio_path_from_state_file`, which is the value the daemon
/// wrote into `last-session.json` BEFORE emitting the signal (C2
/// invariant). When the read fails or the file's `session_id`
/// doesn't match, the caller passes `None` and we fall back to the
/// in-memory cache **only if the cached `session_id` matches the
/// signal's `session_id`** — otherwise the cache is stale and we
/// surface an empty path so the menu can disable "Open last
/// recording" rather than open a wrong file (the bug fix from the
/// 2026-05-02 review).
pub fn apply_transcript_complete(
    state: &mut TrayState,
    session_id: &str,
    audio_path_from_state_file: Option<PathBuf>,
    transcript_path: &str,
    backend: &str,
    completed_at_unix_ms: u64,
) {
    let audio_path = audio_path_from_state_file.unwrap_or_else(|| {
        state
            .last_session
            .as_ref()
            .filter(|cached| cached.session_id == session_id)
            .map_or_else(PathBuf::new, |cached| cached.audio_path.clone())
    });

    state.last_session = Some(LastCompleted {
        session_id: session_id.to_owned(),
        audio_path,
        transcript_path: Some(PathBuf::from(transcript_path)),
        backend: if backend.is_empty() {
            None
        } else {
            Some(backend.to_owned())
        },
        completed_at_unix_ms,
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn from_wire_maps_all_five_known_strings() {
        assert_eq!(IconState::from_wire("idle"), IconState::Idle);
        assert_eq!(IconState::from_wire("starting"), IconState::Starting);
        assert_eq!(IconState::from_wire("recording"), IconState::Recording);
        assert_eq!(IconState::from_wire("stopping"), IconState::Stopping);
        assert_eq!(IconState::from_wire("failed"), IconState::Failed);
    }

    #[test]
    fn from_wire_unknown_returns_failed() {
        assert_eq!(IconState::from_wire("garbage"), IconState::Failed);
        assert_eq!(IconState::from_wire(""), IconState::Failed);
        assert_eq!(IconState::from_wire("Idle"), IconState::Failed);
    }

    #[test]
    fn apply_state_changed_idle_to_recording_sets_started_at() {
        let mut s = TrayState::default();
        apply_state_changed(&mut s, "idle", "");
        assert!(s.recording_started_at.is_none());

        apply_state_changed(&mut s, "starting", "sid-1");
        assert!(s.recording_started_at.is_none());
        assert_eq!(s.active_session_id.as_deref(), Some("sid-1"));

        apply_state_changed(&mut s, "recording", "sid-1");
        assert!(s.recording_started_at.is_some());
        assert_eq!(s.icon, IconState::Recording);
    }

    #[test]
    fn apply_state_changed_recording_to_stopping_keeps_started_at() {
        let mut s = TrayState::default();
        apply_state_changed(&mut s, "recording", "sid-1");
        let started_at = s.recording_started_at;
        assert!(started_at.is_some());

        apply_state_changed(&mut s, "stopping", "sid-1");
        assert_eq!(s.recording_started_at, started_at);
        assert_eq!(s.icon, IconState::Stopping);
    }

    #[test]
    fn apply_state_changed_recording_to_idle_clears_started_at() {
        let mut s = TrayState::default();
        apply_state_changed(&mut s, "recording", "sid-1");
        assert!(s.recording_started_at.is_some());
        assert_eq!(s.active_session_id.as_deref(), Some("sid-1"));

        apply_state_changed(&mut s, "idle", "");
        assert!(s.recording_started_at.is_none());
        assert!(s.active_session_id.is_none());
        assert_eq!(s.icon, IconState::Idle);
    }

    #[test]
    fn apply_state_changed_clears_pending_when_state_matches() {
        let mut s = TrayState {
            pending_cmd: Some(PendingCmd::Stop),
            ..TrayState::default()
        };

        apply_state_changed(&mut s, "stopping", "sid-1");
        assert!(s.pending_cmd.is_none(), "stopping clears pending Stop");

        s.pending_cmd = Some(PendingCmd::Stop);
        apply_state_changed(&mut s, "idle", "");
        assert!(s.pending_cmd.is_none(), "idle clears pending Stop");

        s.pending_cmd = Some(PendingCmd::Start);
        apply_state_changed(&mut s, "starting", "sid-2");
        assert!(s.pending_cmd.is_none(), "starting clears pending Start");
    }

    #[test]
    fn apply_state_changed_does_not_clear_pending_on_mismatch() {
        let mut s = TrayState {
            pending_cmd: Some(PendingCmd::Start),
            ..TrayState::default()
        };

        apply_state_changed(&mut s, "stopping", "sid-1");
        assert_eq!(s.pending_cmd, Some(PendingCmd::Start));

        apply_state_changed(&mut s, "failed", "");
        assert_eq!(s.pending_cmd, Some(PendingCmd::Start));
    }

    #[test]
    fn last_completed_parses_audio_only_phase() {
        let json = br#"{
            "schema_version": 1,
            "session_id": "abc",
            "audio_path": "/tmp/a.flac",
            "transcript_path": "",
            "backend": "",
            "completed_at_unix_ms": 1700000000000
        }"#;
        let parsed = LastCompleted::from_state_file_bytes(json).unwrap();
        assert_eq!(parsed.session_id, "abc");
        assert_eq!(parsed.audio_path, PathBuf::from("/tmp/a.flac"));
        assert!(parsed.transcript_path.is_none());
        assert!(parsed.backend.is_none());
        assert_eq!(parsed.completed_at_unix_ms, 1_700_000_000_000);
    }

    #[test]
    fn last_completed_parses_full_phase() {
        let json = br#"{
            "schema_version": 1,
            "session_id": "abc",
            "audio_path": "/tmp/a.flac",
            "transcript_path": "/tmp/a.flac.txt",
            "backend": "whisper-cli",
            "completed_at_unix_ms": 1700000000000
        }"#;
        let parsed = LastCompleted::from_state_file_bytes(json).unwrap();
        assert_eq!(
            parsed.transcript_path,
            Some(PathBuf::from("/tmp/a.flac.txt"))
        );
        assert_eq!(parsed.backend.as_deref(), Some("whisper-cli"));
    }

    #[test]
    fn last_completed_treats_empty_transcript_as_none() {
        let json = br#"{
            "schema_version": 1,
            "session_id": "abc",
            "audio_path": "/tmp/a.flac",
            "transcript_path": "",
            "backend": "whisper-cli",
            "completed_at_unix_ms": 1700000000000
        }"#;
        let parsed = LastCompleted::from_state_file_bytes(json).unwrap();
        assert!(parsed.transcript_path.is_none());
        // Backend without transcript is preserved (defensive — daemon
        // shouldn't write this combination, but parser shouldn't lose
        // data if it does).
        assert_eq!(parsed.backend.as_deref(), Some("whisper-cli"));
    }

    #[test]
    fn last_completed_rejects_unsupported_schema() {
        let json = br#"{
            "schema_version": 999,
            "session_id": "abc",
            "audio_path": "/tmp/a.flac",
            "transcript_path": "",
            "backend": "",
            "completed_at_unix_ms": 0
        }"#;
        let err = LastCompleted::from_state_file_bytes(json).unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedSchemaVersion(999)));
    }

    #[test]
    fn last_completed_rejects_empty_audio_path() {
        let json = br#"{
            "schema_version": 1,
            "session_id": "abc",
            "audio_path": "",
            "transcript_path": "",
            "backend": "",
            "completed_at_unix_ms": 0
        }"#;
        let err = LastCompleted::from_state_file_bytes(json).unwrap_err();
        assert!(matches!(err, ParseError::EmptyAudioPath));
    }

    fn cached_session(session_id: &str, audio: &str) -> LastCompleted {
        LastCompleted {
            session_id: session_id.to_owned(),
            audio_path: PathBuf::from(audio),
            transcript_path: None,
            backend: None,
            completed_at_unix_ms: 0,
        }
    }

    #[test]
    fn apply_transcript_complete_uses_explicit_audio_path() {
        // Happy path: caller read the daemon's state file and
        // passed in the canonical audio path. Reducer trusts it
        // unconditionally; the cache is irrelevant.
        let mut state = TrayState {
            last_session: Some(cached_session("OLD", "/tmp/STALE.flac")),
            ..TrayState::default()
        };
        apply_transcript_complete(
            &mut state,
            "NEW",
            Some(PathBuf::from("/tmp/correct.flac")),
            "/tmp/correct.flac.txt",
            "whisper-cli",
            42,
        );
        let last = state.last_session.as_ref().unwrap();
        assert_eq!(last.session_id, "NEW");
        assert_eq!(last.audio_path, PathBuf::from("/tmp/correct.flac"));
        assert_eq!(
            last.transcript_path,
            Some(PathBuf::from("/tmp/correct.flac.txt"))
        );
    }

    #[test]
    fn apply_transcript_complete_falls_back_to_cache_only_when_session_matches() {
        // Reconnect-window safety: caller could not read the file
        // (None) but the cache has a matching session_id. Falling
        // back to the cache is fine because the cache was set by
        // an earlier RecordingComplete for the SAME session.
        let mut state = TrayState {
            last_session: Some(cached_session("MATCH", "/tmp/match.flac")),
            ..TrayState::default()
        };
        apply_transcript_complete(
            &mut state,
            "MATCH",
            None,
            "/tmp/match.flac.txt",
            "whisper-cli",
            42,
        );
        let last = state.last_session.as_ref().unwrap();
        assert_eq!(last.audio_path, PathBuf::from("/tmp/match.flac"));
    }

    #[test]
    fn apply_transcript_complete_empty_audio_when_cache_session_mismatches() {
        // The bug fix from the 2026-05-02 review: with no explicit
        // path AND a stale cache (different session_id), we MUST
        // NOT silently graft the old audio_path onto the new
        // session. Better to surface an empty path so the menu
        // does not point at the wrong file.
        let mut state = TrayState {
            last_session: Some(cached_session("OLD", "/tmp/wrong.flac")),
            ..TrayState::default()
        };
        apply_transcript_complete(
            &mut state,
            "NEW",
            None,
            "/tmp/new.flac.txt",
            "whisper-cli",
            42,
        );
        let last = state.last_session.as_ref().unwrap();
        assert_eq!(last.session_id, "NEW");
        assert_eq!(
            last.audio_path,
            PathBuf::new(),
            "must not graft stale OLD audio_path onto NEW session",
        );
    }

    #[test]
    fn apply_transcript_complete_empty_audio_when_cache_absent() {
        // No cache, no explicit path → empty (defensive). The
        // dispatcher menu builder treats an empty path as "no
        // audio file" and disables Open last recording.
        let mut state = TrayState {
            last_session: None,
            ..TrayState::default()
        };
        apply_transcript_complete(
            &mut state,
            "NEW",
            None,
            "/tmp/new.flac.txt",
            "whisper-cli",
            42,
        );
        let last = state.last_session.as_ref().unwrap();
        assert_eq!(last.audio_path, PathBuf::new());
    }
}
