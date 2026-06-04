//! Wire-format structs shared between `zwhisperd` and `zwhisper-cli`.
//!
//! Both structs derive [`zvariant::Type`] alongside `serde::{Serialize,
//! Deserialize}` so they can be transmitted over D-Bus as native struct
//! tuples without a custom marshaller. The signatures are part of the
//! frozen public API (M3 lock-ins § 3) and are pinned by the unit tests
//! at the bottom of this file.

use serde::{Deserialize, Serialize};
use zvariant::Type;

/// Snapshot returned by `Recorder1.GetStatus`.
///
/// Wire signature: `(sst)` — two strings followed by an unsigned 64-bit
/// integer. `state` is a `zwhisper_core::audio::state::RecorderState`
/// rendered through its `Display` impl (`idle | starting | recording |
/// stopping | failed`). `duration_ms` is monotonic from the start of the
/// current recording, or `0` when no recording is active.
///
/// Per stress-test correction C6, `duration_ms` is **unsigned**: a
/// duration cannot be negative, and forcing `u64` removes the need for
/// the client to defend against non-negative invariants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Status {
    pub state: String,
    pub active_profile: String,
    pub duration_ms: u64,
}

/// One entry in the response of `Profiles1.List`.
///
/// Wire signature: `(ssu)` — name, description, `schema_version` (u32).
///
/// Per stress-test correction C12, `schema_version` is the schema
/// version **after** any auto-migration the daemon performed while
/// loading the profile. For a successfully-loaded profile this always
/// equals `zwhisper_core::profile::CURRENT_SCHEMA_VERSION`. M4-shaped
/// property notifications stay deferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ProfileEntry {
    pub name: String,
    pub description: String,
    pub schema_version: u32,
}

/// One entry in the response of `Profiles1.list_v2` (M5).
///
/// Wire signature: `(ssus)` — name, description, `schema_version`,
/// **backend** (e.g., `"whisper-cpp"`, `"deepgram"`). Added so the
/// tray can render a cloud marker (`☁`) next to cloud-backed profiles
/// without a per-profile follow-up RPC. The legacy
/// [`ProfileEntry`] / `list` surface stays untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ProfileEntryV2 {
    pub name: String,
    pub description: String,
    pub schema_version: u32,
    pub backend: String,
}

/// One entry in the response of `Jobs1.ListJobs` (RFC-daemon-role
/// Feature 1).
///
/// Wire signature: `(ssst)` — `job_id`, `state`, `label`,
/// `submitted_ms`. `state` is one of `queued | running | done | failed
/// | cancelled`. `label` is a human hint (a file stem for
/// `TranscribeFile`, `auto:<session>` for a post-record auto-transcribe).
/// `submitted_ms` is a Unix-epoch millisecond timestamp; **unsigned**
/// for the same reason `Status.duration_ms` is (C6 — a timestamp is
/// never negative).
///
/// This is a NEW interface, so the shape is chosen freely; there is no
/// frozen struct to preserve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct JobInfo {
    pub job_id: String,
    pub state: String,
    pub label: String,
    pub submitted_ms: u64,
}

/// One entry in the response of `History1.ListSessions` /
/// `History1.GetSession` (RFC-daemon-role Feature 2).
///
/// Wire signature: `(stssssssss)` — `session_id`, `created_at_ms`
/// (Unix-epoch ms, unsigned per C6), `profile`, `audio_path`,
/// `backend`, `model`, `lang`, `status`, `transcript_path` (`""` when
/// none), `last_error` (`""` when none). `status` ∈ `recorded |
/// transcribing | interrupted | done | failed`.
///
/// This is the wire projection of the daemon's richer on-disk
/// `HistoryEntry`; fields the CLI never renders (codec, native_rate,
/// channels, whisper_pid) are intentionally dropped from the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct HistorySession {
    pub session_id: String,
    pub created_at_ms: u64,
    pub profile: String,
    pub audio_path: String,
    pub backend: String,
    pub model: String,
    pub lang: String,
    pub status: String,
    pub transcript_path: String,
    pub last_error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_to_dbus_signature_sst() {
        // C6 freezes `Status` at `(sst)`. If this test fails, the wire
        // format drifted — fix the struct, not the assertion.
        assert_eq!(Status::SIGNATURE.to_string(), "(sst)");
    }

    #[test]
    fn profile_entry_serializes_to_dbus_signature_ssu() {
        // M3 lock-in § 3: `ProfileEntry` is `(ssu)`.
        assert_eq!(ProfileEntry::SIGNATURE.to_string(), "(ssu)");
    }

    #[test]
    fn profile_entry_v2_serializes_to_dbus_signature_ssus() {
        // M5 § "Profiles1 D-Bus contract decision": list_v2 returns
        // `a(ssus)`. Drift here means the tray will silently drop or
        // mis-render the backend column.
        assert_eq!(ProfileEntryV2::SIGNATURE.to_string(), "(ssus)");
    }

    #[test]
    fn job_info_serializes_to_dbus_signature_ssst() {
        // RFC-daemon-role Jobs1.ListJobs returns a(ssst). Drift here
        // silently mis-renders the `jobs` CLI table.
        assert_eq!(JobInfo::SIGNATURE.to_string(), "(ssst)");
    }

    #[test]
    fn history_session_serializes_to_dbus_signature() {
        // RFC-daemon-role History1.ListSessions/GetSession returns
        // (stssssssss): s + t + eight s.
        assert_eq!(HistorySession::SIGNATURE.to_string(), "(stssssssss)");
    }
}
