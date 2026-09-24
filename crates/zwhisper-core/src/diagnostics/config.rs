//! Tunables for the actionable-diagnostics layer (RFC-actionable-errors
//! § F9).
//!
//! Every threshold, interval, and timeout lives here as a named constant
//! with a [`DiagnosticsConfig`] field, so the detection code carries zero
//! inline magic numbers (CLAUDE.md: zero hardcoded values, no silent
//! defaults). [`DiagnosticsConfig::validate`] fails fast on a
//! structurally impossible config rather than letting a zero interval or
//! an out-of-range ratio produce a nonsense verdict deep in the analysis.
//!
//! The defaults are deliberately conservative: a verdict is a claim made
//! to the user about their hardware, and a false "your mic is clipping"
//! is worse than staying quiet.

/// dBFS floor reported for silence. Defined in `crate::gain` and
/// re-exported here (and, identically, as
/// [`crate::setup::config::SILENCE_FLOOR_DB`]) so each module publishes
/// the floor under its own name while there is only one definition.
pub const SILENCE_FLOOR_DB: f32 = crate::gain::SILENCE_FLOOR_DB;

/// Interval (ms) between `level` element messages during capture. Also
/// the width of one analysis window. 100 ms is the GStreamer default and
/// gives 600 windows a minute — enough resolution for a ratio test
/// without flooding the bus.
pub const DEFAULT_LEVEL_INTERVAL_MS: u64 = 100;

/// Peak level (dBFS) at or above which an analysis window counts as
/// clipped. `-0.1` rather than `0.0`: a 16-bit sample at full scale
/// reports marginally below 0 dBFS, and a converter that is pinned to
/// its rail is already distorting.
pub const DEFAULT_CLIP_PEAK_DB: f32 = -0.1;

/// Fraction of analysis windows that must clip before the recording is
/// called clipped. A single transient (a door, a desk bump) is not a
/// gain problem; 2 % — roughly one window in fifty — is.
pub const DEFAULT_CLIP_WINDOW_RATIO: f32 = 0.02;

/// Whole-session RMS (dBFS) below which the recording is called silent.
/// 15 dB below the RFC-mic-setup idle-floor ceiling
/// (`DEFAULT_IDLE_FLOOR_MAX_DB = -45`), which is itself well below any
/// speech: a session whose *aggregate* RMS is under this contains no
/// speech on any microphone.
pub const DEFAULT_SILENT_RMS_DB: f32 = -60.0;

/// Minimum recording length (ms) before a level verdict is offered at
/// all. Below this there are too few windows for a ratio to mean
/// anything, so the analysis declines to make a claim.
pub const DEFAULT_MIN_ANALYSIS_MS: u64 = 500;

/// Whether to probe the target source's mute flag before starting a
/// recording. On by default: speaking a whole sentence into a muted
/// microphone is the failure this layer exists to prevent.
pub const DEFAULT_MUTE_PROBE: bool = true;

/// Hard timeout (ms) for the whole mute probe (`pw-dump` + `wpctl`).
/// Past it the probe is abandoned and the recording proceeds — an
/// inconclusive probe must never cost the user their dictation.
pub const DEFAULT_MUTE_PROBE_TIMEOUT_MS: u64 = 1_500;

/// All knobs for failure detection. `Default` yields the conservative
/// values above; a profile's `[diagnostics]` table overrides individual
/// fields.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticsConfig {
    /// Interval (ms) between `level` messages / width of one window.
    pub level_interval_ms: u64,
    /// Peak (dBFS) at or above which a window counts as clipped.
    pub clip_peak_db: f32,
    /// Fraction of windows that must clip before the verdict fires.
    pub clip_window_ratio: f32,
    /// Whole-session RMS (dBFS) below which the recording is silent.
    pub silent_rms_db: f32,
    /// Minimum recording length (ms) before any level verdict is made.
    pub min_analysis_ms: u64,
    /// Whether to probe the source's mute flag before capture.
    pub mute_probe: bool,
    /// Hard timeout (ms) for the mute probe.
    pub mute_probe_timeout_ms: u64,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            level_interval_ms: DEFAULT_LEVEL_INTERVAL_MS,
            clip_peak_db: DEFAULT_CLIP_PEAK_DB,
            clip_window_ratio: DEFAULT_CLIP_WINDOW_RATIO,
            silent_rms_db: DEFAULT_SILENT_RMS_DB,
            min_analysis_ms: DEFAULT_MIN_ANALYSIS_MS,
            mute_probe: DEFAULT_MUTE_PROBE,
            mute_probe_timeout_ms: DEFAULT_MUTE_PROBE_TIMEOUT_MS,
        }
    }
}

impl DiagnosticsConfig {
    /// Fail fast on a structurally invalid config. Returns a
    /// human-readable message (mirroring
    /// [`crate::setup::SetupConfig::validate`]'s `Result<(), String>`
    /// shape) so the profile validator can surface it directly.
    ///
    /// Checks: a non-zero level interval and probe timeout, finite dB
    /// thresholds, a clip ratio inside `(0.0, 1.0]`, and a silence floor
    /// strictly above [`SILENCE_FLOOR_DB`] — a threshold *at* the
    /// sentinel would call every recording silent, including the ones
    /// that merely produced no level messages.
    pub fn validate(&self) -> Result<(), String> {
        if self.level_interval_ms == 0 {
            return Err("diagnostics.level_interval_ms must be > 0".to_owned());
        }
        if !self.clip_peak_db.is_finite() {
            return Err("diagnostics.clip_peak_db must be finite".to_owned());
        }
        if !(self.clip_window_ratio.is_finite()
            && self.clip_window_ratio > 0.0
            && self.clip_window_ratio <= 1.0)
        {
            return Err(
                "diagnostics.clip_window_ratio must be finite and within (0.0, 1.0]".to_owned(),
            );
        }
        if !self.silent_rms_db.is_finite() {
            return Err("diagnostics.silent_rms_db must be finite".to_owned());
        }
        if self.silent_rms_db <= SILENCE_FLOOR_DB {
            return Err(format!(
                "diagnostics.silent_rms_db must be above the silence floor {SILENCE_FLOOR_DB}"
            ));
        }
        if self.mute_probe_timeout_ms == 0 {
            return Err("diagnostics.mute_probe_timeout_ms must be > 0".to_owned());
        }
        Ok(())
    }

    /// The level-message interval in nanoseconds, the unit the
    /// `GStreamer` `level` element's `interval` property takes.
    /// Saturates rather than wrapping on an absurd configured value;
    /// [`Self::validate`] has already rejected zero.
    #[must_use]
    pub fn level_interval_ns(&self) -> u64 {
        self.level_interval_ms.saturating_mul(1_000_000)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates() {
        DiagnosticsConfig::default().validate().unwrap();
    }

    #[test]
    fn silence_threshold_sits_well_below_the_mic_setup_idle_floor() {
        // The idle-floor ceiling is what `zwhisper audio` warns about;
        // the silence verdict must be a much stronger statement than
        // "your noise floor is high", or it would fire on a merely
        // quiet room.
        let cfg = DiagnosticsConfig::default();
        // `setup::config::DEFAULT_IDLE_FLOOR_MAX_DB` is -45.0; keep the
        // comparison literal so this test does not need the `setup`
        // feature to compile.
        assert!(cfg.silent_rms_db < -45.0 - 10.0);
        assert!(cfg.silent_rms_db > SILENCE_FLOOR_DB);
    }

    #[test]
    fn zero_level_interval_rejected() {
        let cfg = DiagnosticsConfig {
            level_interval_ms: 0,
            ..DiagnosticsConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn clip_ratio_outside_unit_interval_rejected() {
        for ratio in [0.0_f32, -0.1, 1.5, f32::NAN, f32::INFINITY] {
            let cfg = DiagnosticsConfig {
                clip_window_ratio: ratio,
                ..DiagnosticsConfig::default()
            };
            assert!(cfg.validate().is_err(), "ratio {ratio} must reject");
        }
        // Exactly 1.0 means "every window must clip" — strict, but a
        // coherent request.
        let cfg = DiagnosticsConfig {
            clip_window_ratio: 1.0,
            ..DiagnosticsConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn non_finite_db_thresholds_rejected() {
        for db in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                DiagnosticsConfig {
                    clip_peak_db: db,
                    ..DiagnosticsConfig::default()
                }
                .validate()
                .is_err(),
                "clip_peak_db {db} must reject"
            );
            assert!(
                DiagnosticsConfig {
                    silent_rms_db: db,
                    ..DiagnosticsConfig::default()
                }
                .validate()
                .is_err(),
                "silent_rms_db {db} must reject"
            );
        }
    }

    #[test]
    fn silence_threshold_at_or_below_the_floor_rejected() {
        for db in [SILENCE_FLOOR_DB, SILENCE_FLOOR_DB - 1.0] {
            let cfg = DiagnosticsConfig {
                silent_rms_db: db,
                ..DiagnosticsConfig::default()
            };
            assert!(cfg.validate().is_err(), "silent_rms_db {db} must reject");
        }
    }

    #[test]
    fn zero_probe_timeout_rejected() {
        let cfg = DiagnosticsConfig {
            mute_probe_timeout_ms: 0,
            ..DiagnosticsConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn interval_converts_to_nanoseconds() {
        let cfg = DiagnosticsConfig::default();
        assert_eq!(cfg.level_interval_ns(), 100_000_000);
    }
}
