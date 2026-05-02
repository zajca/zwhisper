//! Runtime tuning knobs for `zwhisper-tray`.
//!
//! Per CLAUDE.md, all configuration values live in a dedicated
//! module rather than scattered as `const` items across the source
//! tree. Constants here are grouped by concern and carry a doc
//! comment that states the rationale for the chosen default — a
//! future tuning pass can revisit each value with the design context
//! intact.
//!
//! ## Override mechanisms
//!
//! Most knobs are compile-time constants (no runtime override) —
//! they are tuning parameters of the tray's internal architecture
//! that have no obvious operator-facing use case. The two values
//! the operator may legitimately want to override at runtime
//! (clipboard size guard and the SNI assume-available toggle for
//! sandboxed environments) read env vars; see [`Config::from_env`]
//! and the `*_ENV` constants below.
//!
//! ## Why not TOML
//!
//! The tray has no per-user persisted configuration: it consumes
//! the daemon's profiles and otherwise has nothing to remember
//! across runs. Env-var overrides plus compile-time constants
//! cover every tuning need without introducing a config file the
//! user would have to manage.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Channels — bounded mpsc capacities
// ---------------------------------------------------------------------------

/// Capacity of the menu → RPC dispatcher mpsc.
///
/// **Rationale.** Menu callbacks `try_send` `PendingCmd` values onto
/// this channel; if the dispatcher cannot keep up, the buffer
/// absorbs short bursts. 8 is enough headroom for the most
/// pathological case (user spamming Start/Stop while RPCs are
/// in-flight) without growing memory unboundedly. A full buffer
/// makes `try_send` fail, which is logged and dropped — a benign
/// outcome (the optimistic action lock prevents ambiguous state).
pub const COMMAND_CHANNEL_CAPACITY: usize = 8;

/// Capacity of the pump → sink-dispatcher mpsc.
///
/// **Rationale.** `TranscriptComplete` only fires once per session,
/// so the channel is almost always empty. 8 is a conservative
/// upper bound covering the (currently theoretical) case where the
/// daemon completes multiple sessions back-to-back faster than the
/// dispatcher can flush them.
pub const SINK_CHANNEL_CAPACITY: usize = 8;

// ---------------------------------------------------------------------------
// Pump — D-Bus connection lifetime
// ---------------------------------------------------------------------------

/// Period between defensive `Profiles1.List` re-fetches.
///
/// **Rationale.** The M3 contract has no `ProfilesChanged` signal;
/// `Profiles1.Reload` is a no-op stub. The tray therefore polls the
/// list periodically so an out-of-band TOML edit eventually shows
/// up in the menu without requiring the user to restart the tray.
/// 60 s is a deliberate compromise: short enough that the staleness
/// window is invisible to most users, long enough that the cost
/// (one synchronous filesystem scan) is negligible. Will be removed
/// when M5+ introduces `Profiles2.ProfilesChanged`.
pub const PROFILE_REFRESH_PERIOD: Duration = Duration::from_secs(60);

/// Reconnect-backoff schedule, in milliseconds.
///
/// **Rationale.** Exponential-with-cap pattern. After every failed
/// reconnect attempt, the pump waits for the next entry, capped at
/// the last value. Total worst-case delay before the cap kicks in:
/// 250 + 500 + 1000 + 2000 = 3.75 s, which fits comfortably under
/// systemd's `RestartSec=2` for the daemon (so the tray reconnects
/// almost as soon as the daemon comes back). The 5 s cap balances
/// "do not hammer a permanently dead daemon" against "do not leave
/// the user staring at an offline icon forever".
pub const BACKOFF_SCHEDULE_MS: &[u64] = &[250, 500, 1000, 2000, 5000];

// ---------------------------------------------------------------------------
// Sinks — clipboard size guard
// ---------------------------------------------------------------------------

/// Default upper bound on transcript size for clipboard delivery,
/// in bytes. Transcripts larger than this skip the clipboard sink
/// and surface the size in the notification body instead.
///
/// **Rationale.** A 4-hour meeting at 12 KB/min ≈ 3 MB; a 30-min
/// dictation at the same rate ≈ 360 KB. 512 KB covers the typical
/// dictation case without choking on long meetings, where pasting a
/// novel into the clipboard is rarely what the user wants. The
/// runtime override [`CLIPBOARD_MAX_BYTES_ENV`] lets operators
/// raise (or lower) the threshold without recompiling.
pub const DEFAULT_CLIPBOARD_MAX_BYTES: u64 = 512 * 1024;

/// Environment variable consulted by [`Config::from_env`] to
/// override [`DEFAULT_CLIPBOARD_MAX_BYTES`]. Set to a u64 in bytes;
/// invalid or unparseable values fall back to the default with a
/// `tracing::warn!`.
pub const CLIPBOARD_MAX_BYTES_ENV: &str = "ZWHISPER_TRAY_CLIPBOARD_MAX_BYTES";

// ---------------------------------------------------------------------------
// Runtime config
// ---------------------------------------------------------------------------

/// Resolved runtime configuration for the tray.
///
/// Built once at startup via [`Config::from_env`] and threaded
/// through the spawned tasks. Compile-time constants (the
/// `*_CAPACITY` channel sizes, [`PROFILE_REFRESH_PERIOD`],
/// [`BACKOFF_SCHEDULE_MS`]) stay as `pub const` because they have
/// no operator-facing override; only operator-tunable values land
/// in this struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Resolved clipboard size guard (env or default).
    pub clipboard_max_bytes: u64,
}

impl Config {
    /// Build a [`Config`] from environment variables, falling back
    /// to documented defaults. Logs a `tracing::warn!` for any
    /// override that fails to parse — the run continues with the
    /// default rather than aborting startup.
    pub fn from_env() -> Self {
        let clipboard_max_bytes = match std::env::var(CLIPBOARD_MAX_BYTES_ENV) {
            Ok(raw) => parse_clipboard_max_bytes(&raw).unwrap_or_else(|reason| {
                tracing::warn!(
                    env = CLIPBOARD_MAX_BYTES_ENV,
                    raw = %raw,
                    reason = %reason,
                    "ignoring invalid override; falling back to default",
                );
                DEFAULT_CLIPBOARD_MAX_BYTES
            }),
            Err(_) => DEFAULT_CLIPBOARD_MAX_BYTES,
        };
        Self {
            clipboard_max_bytes,
        }
    }
}

impl Default for Config {
    /// Default config used by tests that do not need to exercise
    /// env-driven overrides.
    fn default() -> Self {
        Self {
            clipboard_max_bytes: DEFAULT_CLIPBOARD_MAX_BYTES,
        }
    }
}

/// Pure parser for the clipboard size override. Splits the parse
/// from `Config::from_env` so tests can exercise every branch
/// without env mutation (the 2024 edition makes `set_var` unsafe;
/// workspace lints deny `unsafe_code`).
pub fn parse_clipboard_max_bytes(raw: &str) -> Result<u64, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty");
    }
    trimmed.parse::<u64>().map_err(|_| "not a u64")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn channel_capacities_are_positive() {
        // const block: clippy's `assertions_on_constants` (warn at
        // workspace level) wants assertions on const inputs to be
        // compile-time. The const block fails the build itself if
        // someone bumps the constant to 0.
        const _: () = {
            assert!(COMMAND_CHANNEL_CAPACITY > 0);
            assert!(SINK_CHANNEL_CAPACITY > 0);
        };
    }

    #[test]
    fn backoff_schedule_is_strictly_non_decreasing() {
        // Exponential-with-cap pattern: every step should be at
        // least as long as the previous step. Catches a future
        // edit that accidentally mis-orders the schedule.
        for window in BACKOFF_SCHEDULE_MS.windows(2) {
            assert!(
                window[0] <= window[1],
                "backoff step regressed: {} > {}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn backoff_schedule_caps_within_a_reasonable_envelope() {
        // 5 s cap is the documented intent; the test guards against
        // a future edit that pushes the cap past 60 s and surprises
        // operators with a tray that stays offline for a minute.
        let cap = *BACKOFF_SCHEDULE_MS.last().unwrap();
        assert!(cap <= 30_000, "backoff cap exceeds 30 s: {cap} ms");
    }

    #[test]
    fn profile_refresh_period_is_at_least_one_second() {
        // Catch a future edit that drops the period to milliseconds
        // and DOSes the daemon's `Profiles1.List` handler.
        assert!(PROFILE_REFRESH_PERIOD >= Duration::from_secs(1));
    }

    #[test]
    fn parse_clipboard_max_bytes_accepts_valid_u64() {
        assert_eq!(parse_clipboard_max_bytes("1024"), Ok(1024));
        assert_eq!(parse_clipboard_max_bytes("0"), Ok(0));
    }

    #[test]
    fn parse_clipboard_max_bytes_trims_whitespace() {
        assert_eq!(parse_clipboard_max_bytes("  2048  "), Ok(2048));
    }

    #[test]
    fn parse_clipboard_max_bytes_rejects_empty() {
        assert_eq!(parse_clipboard_max_bytes(""), Err("empty"));
        assert_eq!(parse_clipboard_max_bytes("   "), Err("empty"));
    }

    #[test]
    fn parse_clipboard_max_bytes_rejects_non_numeric() {
        assert_eq!(parse_clipboard_max_bytes("twelve"), Err("not a u64"));
        assert_eq!(parse_clipboard_max_bytes("-1"), Err("not a u64"));
        assert_eq!(parse_clipboard_max_bytes("1.5"), Err("not a u64"));
    }

    #[test]
    fn config_default_uses_documented_default() {
        assert_eq!(
            Config::default(),
            Config {
                clipboard_max_bytes: DEFAULT_CLIPBOARD_MAX_BYTES,
            }
        );
    }

    #[test]
    fn clipboard_max_bytes_env_name_is_namespaced() {
        // Sanity: env vars must be vendor-prefixed so the operator
        // can grep for the project's footprint.
        assert!(CLIPBOARD_MAX_BYTES_ENV.starts_with("ZWHISPER_"));
    }
}
