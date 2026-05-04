//! Hotkey configuration (debounce, cooldown, auto-bind, etc.).
//!
//! This module owns the single struct that the tray and the CLI
//! consult before deciding whether a toggle attempt should be
//! accepted, debounced, or rejected because we are still in the
//! post-stop cooldown window. Per `docs/M6-plan.md` § "Architectural
//! decisions" D1, the cooldown defaults to 1500 ms — this is the
//! window during which the daemon is still draining the recording
//! pipeline (gst EOS + transcribe drain) and a fresh
//! `StartRecording` would race the previous session's terminal
//! emit and either clobber the wave file or surface as
//! `RpcError::SessionInUse`.
//!
//! ## Defaults — sourced from documented constants
//!
//! Per CLAUDE.md "no silent defaults": every default is a `pub
//! const` at the top of this file rather than a magic number
//! buried in `impl Default`. Anyone reading the constants knows
//! the wire-level meaning without spelunking through code.
//!
//! ## Corrupt config policy (risk D2)
//!
//! `from_path` swallows two failure modes — file missing and TOML
//! parse error — and returns `Self::default()` in both. Hotkey
//! config is **optional feature config**, not required runtime
//! state, so a syntax error must not prevent the tray from
//! starting. The parse failure path emits a single
//! `tracing::warn!` so the operator notices in the journal.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Default debounce window — collapse two presses fired within
/// this interval down to a single accepted toggle. Mitigates the
/// "key bounces" artefact a few keyboards exhibit at the
/// hardware level, plus IME duplicate forwards.
pub const DEFAULT_DEBOUNCE_MS: u64 = 250;

/// Default post-stop cooldown — once a `StopRecording` is sent,
/// reject any further toggle attempts inside this window. See D1
/// rationale: `GetStatus` flips back to `"idle"` after the
/// lifecycle task drains the pipeline, but the daemon may still
/// be inside the transcribe step. Without the cooldown, a quick
/// second press would either kick off a competing recording or
/// trip `RpcError::SessionInUse`.
pub const DEFAULT_COOLDOWN_MS: u64 = 1500;

/// Default upper bound on how long the portal `bind` step is
/// allowed to take before the listener task gives up and reports
/// "bind timeout". Matches `DoD #12`.
pub const DEFAULT_BIND_TIMEOUT_SECS: u64 = 30;

/// Default for whether the tray attempts to bind the global
/// shortcut on startup (vs. waiting for the user to open the
/// settings dialog).
pub const DEFAULT_AUTO_BIND_ON_STARTUP: bool = true;

/// Default for whether `StartRecording` triggered through the
/// hotkey emits a transient notification (`DoD #18`).
pub const DEFAULT_NOTIFY_ON_START: bool = true;

/// Hotkey-related configuration knobs.
///
/// All fields are `pub` so the CLI's `zwhisper hotkey` subcommand
/// (read-only inspection) can render the current values without
/// going through accessors. The struct lives in
/// `~/.config/zwhisper/hotkey.toml`; see `DoD #16` for the path
/// resolution rules and the precedence over `$XDG_CONFIG_HOME`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotkeyConfig {
    /// Collapse repeated presses inside this window into one accept.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    /// Reject toggle attempts inside this post-stop window.
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
    /// Upper bound on `bind` portal calls.
    #[serde(default = "default_bind_timeout_secs")]
    pub bind_timeout_secs: u64,
    /// Whether to bind the shortcut on startup automatically.
    #[serde(default = "default_auto_bind_on_startup")]
    pub auto_bind_on_startup: bool,
    /// Whether to fire `notify-send` when toggle starts a recording.
    #[serde(default = "default_notify_on_start")]
    pub notify_on_start: bool,
}

const fn default_debounce_ms() -> u64 {
    DEFAULT_DEBOUNCE_MS
}
const fn default_cooldown_ms() -> u64 {
    DEFAULT_COOLDOWN_MS
}
const fn default_bind_timeout_secs() -> u64 {
    DEFAULT_BIND_TIMEOUT_SECS
}
const fn default_auto_bind_on_startup() -> bool {
    DEFAULT_AUTO_BIND_ON_STARTUP
}
const fn default_notify_on_start() -> bool {
    DEFAULT_NOTIFY_ON_START
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            cooldown_ms: DEFAULT_COOLDOWN_MS,
            bind_timeout_secs: DEFAULT_BIND_TIMEOUT_SECS,
            auto_bind_on_startup: DEFAULT_AUTO_BIND_ON_STARTUP,
            notify_on_start: DEFAULT_NOTIFY_ON_START,
        }
    }
}

impl HotkeyConfig {
    /// Read the config from `path`. Missing file or parse failure
    /// both fall back to [`Default::default`]; parse failures
    /// additionally emit a `tracing::warn!` so the operator can
    /// notice the silent-fallback in the journal.
    ///
    /// Per D2 (risk register): hotkey config is optional feature
    /// config. We must NOT panic or block tray startup on a
    /// syntax error.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(e) => {
                warn!(
                    error = %e,
                    path = %path.display(),
                    "could not read hotkey config; falling back to defaults",
                );
                return Self::default();
            }
        };
        match Self::from_str(&raw) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(
                    error = %e,
                    path = %path.display(),
                    "could not parse hotkey config; falling back to defaults",
                );
                Self::default()
            }
        }
    }

    /// Parse a config from a TOML source. Used both by
    /// [`Self::from_path`] and by unit tests that prefer to feed
    /// the source directly without round-tripping through the
    /// filesystem.
    ///
    /// Named `from_str` (and not implementing the standard
    /// [`std::str::FromStr`] trait) because the M6 spec freezes
    /// this exact public signature and `FromStr::from_str`'s
    /// associated `Err` type must be `Sized + 'static`, which
    /// `toml::de::Error` satisfies — but we still want the
    /// inherent method form so callers can write
    /// `HotkeyConfig::from_str(&raw)` without an explicit
    /// `<HotkeyConfig as FromStr>::from_str` import.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(toml_src: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_src)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn default_matches_documented_constants() {
        let cfg = HotkeyConfig::default();
        assert_eq!(cfg.debounce_ms, DEFAULT_DEBOUNCE_MS);
        assert_eq!(cfg.cooldown_ms, DEFAULT_COOLDOWN_MS);
        assert_eq!(cfg.bind_timeout_secs, DEFAULT_BIND_TIMEOUT_SECS);
        assert_eq!(cfg.auto_bind_on_startup, DEFAULT_AUTO_BIND_ON_STARTUP);
        assert_eq!(cfg.notify_on_start, DEFAULT_NOTIFY_ON_START);
    }

    #[test]
    fn round_trip_serializes_and_parses_back() {
        let original = HotkeyConfig {
            debounce_ms: 300,
            cooldown_ms: 1750,
            bind_timeout_secs: 45,
            auto_bind_on_startup: false,
            notify_on_start: false,
        };
        let serialised = toml::to_string(&original).expect("serialize");
        let parsed = HotkeyConfig::from_str(&serialised).expect("parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn from_str_accepts_partial_config_with_defaults_for_missing_keys() {
        let toml_src = "debounce_ms = 100\n";
        let cfg = HotkeyConfig::from_str(toml_src).expect("parse");
        assert_eq!(cfg.debounce_ms, 100);
        assert_eq!(cfg.cooldown_ms, DEFAULT_COOLDOWN_MS);
        assert_eq!(cfg.bind_timeout_secs, DEFAULT_BIND_TIMEOUT_SECS);
    }

    #[test]
    fn from_str_rejects_unknown_field() {
        // `deny_unknown_fields` keeps typos from being silently
        // accepted as defaults — this is the layer the user
        // edits by hand, so we surface unknown keys as parse
        // errors and trip the `from_path` warn fallback.
        let toml_src = "debounce_ms = 100\nmysterious = true\n";
        let err = HotkeyConfig::from_str(toml_src).expect_err("should fail");
        assert!(
            err.to_string().contains("mysterious"),
            "error message {err} should mention the unknown field",
        );
    }

    #[test]
    fn from_path_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-exists.toml");
        let cfg = HotkeyConfig::from_path(&path);
        assert_eq!(cfg, HotkeyConfig::default());
    }

    #[test]
    fn from_path_corrupt_toml_returns_default_and_does_not_panic() {
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "this is = = = not valid TOML <<<").unwrap();
        tmp.flush().unwrap();
        let cfg = HotkeyConfig::from_path(tmp.path());
        assert_eq!(cfg, HotkeyConfig::default());
    }

    #[test]
    fn from_path_valid_file_round_trips_through_disk() {
        let original = HotkeyConfig {
            debounce_ms: 333,
            cooldown_ms: 2000,
            bind_timeout_secs: 60,
            auto_bind_on_startup: false,
            notify_on_start: true,
        };
        let serialised = toml::to_string(&original).unwrap();
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(serialised.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let parsed = HotkeyConfig::from_path(tmp.path());
        assert_eq!(parsed, original);
    }
}
