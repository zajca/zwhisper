//! Tunables for the guided microphone setup / calibration
//! (RFC-mic-setup, "Calibration Algorithm" + "External Tools &
//! Security"). Every threshold, window length, timeout, and size cap
//! lives here as a named constant with a [`SetupConfig`] field, so the
//! analysis and CLI carry **zero hardcoded magic numbers** (CLAUDE.md:
//! zero hardcoded values, no silent defaults). [`SetupConfig::validate`]
//! fails fast on a structurally impossible config rather than letting a
//! zero rate or a `> 1.0` volume cap cause surprises deep in the
//! calibration loop.

/// dBFS floor reported for silence / an empty capture. Well below any
/// real noise floor, so an all-zero buffer maps to a clean sentinel
/// instead of `-inf` arithmetic leaking into clamps and comparisons.
/// Mirrors the `linear_to_db(0)` guard but as a finite, comparable
/// value the recommendation math can subtract against.
pub const SILENCE_FLOOR_DB: f32 = -120.0;

/// Target speech **peak** in dBFS. Mid of the RFC window (−9…−6 dBFS):
/// loud enough to use the converter's range, with headroom before
/// 0 dBFS clipping.
pub const DEFAULT_TARGET_PEAK_DB: f32 = -7.5;

/// Acceptable distance from [`DEFAULT_TARGET_PEAK_DB`] before the
/// calibration loop stops adjusting. ±1.5 dB keeps it inside the
/// −9…−6 window.
pub const DEFAULT_TARGET_PEAK_TOLERANCE_DB: f32 = 1.5;

/// Maximum acceptable **idle noise floor** in dBFS (RFC). A floor that
/// stays above this after lowering volume is the ALC1220 broadband-noise
/// signature the wizard warns about.
pub const DEFAULT_IDLE_FLOOR_MAX_DB: f32 = -45.0;

/// Seconds of silence sampled for the noise-floor window (before the
/// speak prompt).
pub const DEFAULT_NOISE_FLOOR_SECONDS: f32 = 0.5;

/// Seconds of speech sampled per calibration iteration.
pub const DEFAULT_SPEECH_SECONDS: f32 = 3.0;

/// Maximum apply→re-measure→adjust iterations. Hardware gain stages are
/// not perfectly linear, so calibration iterates a few times rather than
/// computing the volume once; this caps the loop.
pub const DEFAULT_MAX_ITERATIONS: u32 = 3;

/// Lowest linear volume the recommender will set.
pub const DEFAULT_MIN_VOLUME: f32 = 0.0;

/// Highest linear volume the recommender will set (the saturation cap).
/// The wizard may suggest a lower value when the noise floor is high.
pub const DEFAULT_MAX_VOLUME: f32 = 1.0;

/// Hard timeout (seconds) for a `pw-cat` metering child. Kills a stuck
/// capture rather than hanging the wizard.
pub const DEFAULT_PW_CAT_TIMEOUT_SECS: u64 = 10;

/// Size cap (bytes) on `pw-dump` stdout. Real dumps are tens of KiB;
/// 8 MiB rejects an absurd / hostile dump before it is buffered.
pub const DEFAULT_MAX_PW_DUMP_BYTES: usize = 8 * 1024 * 1024;

/// Capture/metering sample rate (Hz). 16 kHz mono matches the ASR rate,
/// so the metered signal is the one the recogniser actually sees.
pub const DEFAULT_METER_RATE_HZ: u32 = 16_000;

/// Read-chunk size (bytes) for the live VU loop's `pw-cat` stdout. Small
/// enough for a responsive bar refresh, large enough to avoid syscall
/// churn.
pub const DEFAULT_METER_STDOUT_CHUNK_BYTES: usize = 4096;

/// Size cap (bytes) on a single fixed-duration capture's stdout. Bounds
/// the buffer for the noise-floor / speech windows so a misbehaving
/// `pw-cat` cannot stream unbounded PCM into memory.
pub const DEFAULT_MEASURE_STDOUT_CAP_BYTES: usize = 8 * 1024 * 1024;

/// All knobs for the setup / calibration flow. `Default` yields the
/// RFC-tuned values above; the CLI overrides individual fields from
/// flags (`--target-peak-db`, `--seconds`, `--max-volume`).
#[derive(Debug, Clone)]
pub struct SetupConfig {
    /// Target speech peak in dBFS (mid of the RFC −9…−6 window).
    pub target_peak_db: f32,
    /// Half-width of the acceptable peak band around `target_peak_db`.
    pub target_peak_tolerance_db: f32,
    /// Maximum acceptable idle noise floor in dBFS.
    pub idle_floor_max_db: f32,
    /// Seconds of silence sampled for the noise floor.
    pub noise_floor_seconds: f32,
    /// Seconds of speech sampled per iteration.
    pub speech_seconds: f32,
    /// Maximum apply→re-measure iterations.
    pub max_iterations: u32,
    /// Lowest linear volume the recommender will set.
    pub min_volume: f32,
    /// Highest linear volume the recommender will set (saturation cap).
    pub max_volume: f32,
    /// Hard timeout (seconds) for a `pw-cat` metering child.
    pub pw_cat_timeout_secs: u64,
    /// Size cap (bytes) on `pw-dump` stdout.
    pub max_pw_dump_bytes: usize,
    /// Capture/metering sample rate (Hz).
    pub meter_rate_hz: u32,
    /// Read-chunk size (bytes) for the live VU loop.
    pub meter_stdout_chunk_bytes: usize,
    /// Size cap (bytes) for a single fixed-duration capture.
    pub measure_stdout_cap_bytes: usize,
}

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            target_peak_db: DEFAULT_TARGET_PEAK_DB,
            target_peak_tolerance_db: DEFAULT_TARGET_PEAK_TOLERANCE_DB,
            idle_floor_max_db: DEFAULT_IDLE_FLOOR_MAX_DB,
            noise_floor_seconds: DEFAULT_NOISE_FLOOR_SECONDS,
            speech_seconds: DEFAULT_SPEECH_SECONDS,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            min_volume: DEFAULT_MIN_VOLUME,
            max_volume: DEFAULT_MAX_VOLUME,
            pw_cat_timeout_secs: DEFAULT_PW_CAT_TIMEOUT_SECS,
            max_pw_dump_bytes: DEFAULT_MAX_PW_DUMP_BYTES,
            meter_rate_hz: DEFAULT_METER_RATE_HZ,
            meter_stdout_chunk_bytes: DEFAULT_METER_STDOUT_CHUNK_BYTES,
            measure_stdout_cap_bytes: DEFAULT_MEASURE_STDOUT_CAP_BYTES,
        }
    }
}

impl SetupConfig {
    /// Fail fast on a structurally invalid config. Returns a
    /// human-readable message (mirrors
    /// [`crate::transcribe::config::AudioConfig::validate`]'s
    /// `Result<(), String>` shape) so the CLI can surface it directly.
    ///
    /// Checks: positive rates and window lengths, a non-zero iteration
    /// cap, a `max_volume` in `(0.0, 1.0]` with `min_volume` below it,
    /// a positive tolerance, and non-zero read/size caps so the metering
    /// loop can make progress and the recommendation has a usable band.
    pub fn validate(&self) -> Result<(), String> {
        if self.meter_rate_hz == 0 {
            return Err("setup.meter_rate_hz must be > 0".to_owned());
        }
        if !(self.noise_floor_seconds.is_finite() && self.noise_floor_seconds > 0.0) {
            return Err("setup.noise_floor_seconds must be finite and > 0".to_owned());
        }
        if !(self.speech_seconds.is_finite() && self.speech_seconds > 0.0) {
            return Err("setup.speech_seconds must be finite and > 0".to_owned());
        }
        if self.max_iterations == 0 {
            return Err("setup.max_iterations must be > 0".to_owned());
        }
        if !self.target_peak_db.is_finite() {
            return Err("setup.target_peak_db must be finite".to_owned());
        }
        if !(self.target_peak_tolerance_db.is_finite() && self.target_peak_tolerance_db > 0.0) {
            return Err("setup.target_peak_tolerance_db must be finite and > 0".to_owned());
        }
        if !self.idle_floor_max_db.is_finite() {
            return Err("setup.idle_floor_max_db must be finite".to_owned());
        }
        if !(self.min_volume.is_finite() && self.min_volume >= 0.0) {
            return Err("setup.min_volume must be finite and >= 0".to_owned());
        }
        if !(self.max_volume.is_finite() && self.max_volume > 0.0 && self.max_volume <= 1.0) {
            return Err("setup.max_volume must be finite and within (0.0, 1.0]".to_owned());
        }
        if self.min_volume > self.max_volume {
            return Err("setup.min_volume must not exceed setup.max_volume".to_owned());
        }
        if self.pw_cat_timeout_secs == 0 {
            return Err("setup.pw_cat_timeout_secs must be > 0".to_owned());
        }
        if self.max_pw_dump_bytes == 0 {
            return Err("setup.max_pw_dump_bytes must be > 0".to_owned());
        }
        if self.meter_stdout_chunk_bytes == 0 {
            return Err("setup.meter_stdout_chunk_bytes must be > 0".to_owned());
        }
        if self.measure_stdout_cap_bytes == 0 {
            return Err("setup.measure_stdout_cap_bytes must be > 0".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates() {
        SetupConfig::default().validate().unwrap();
    }

    #[test]
    fn default_target_is_inside_rfc_window() {
        let cfg = SetupConfig::default();
        // RFC speech peak window: −9 … −6 dBFS.
        assert!(cfg.target_peak_db <= -6.0 && cfg.target_peak_db >= -9.0);
        // The whole tolerance band must stay inside the window too.
        assert!(cfg.target_peak_db + cfg.target_peak_tolerance_db <= -6.0);
        assert!(cfg.target_peak_db - cfg.target_peak_tolerance_db >= -9.0);
    }

    #[test]
    fn zero_meter_rate_rejected() {
        let cfg = SetupConfig {
            meter_rate_hz: 0,
            ..SetupConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zero_iterations_rejected() {
        let cfg = SetupConfig {
            max_iterations: 0,
            ..SetupConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn non_positive_seconds_rejected() {
        for cfg in [
            SetupConfig {
                noise_floor_seconds: 0.0,
                ..SetupConfig::default()
            },
            SetupConfig {
                speech_seconds: -1.0,
                ..SetupConfig::default()
            },
            SetupConfig {
                speech_seconds: f32::NAN,
                ..SetupConfig::default()
            },
        ] {
            assert!(cfg.validate().is_err());
        }
    }

    #[test]
    fn max_volume_outside_unit_interval_rejected() {
        for max in [0.0_f32, -0.5, 1.5, f32::NAN, f32::INFINITY] {
            let cfg = SetupConfig {
                max_volume: max,
                ..SetupConfig::default()
            };
            assert!(cfg.validate().is_err(), "max_volume {max} must reject");
        }
        // Exactly 1.0 is the documented upper bound and is accepted.
        let cfg = SetupConfig {
            max_volume: 1.0,
            ..SetupConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn min_above_max_rejected() {
        let cfg = SetupConfig {
            min_volume: 0.9,
            max_volume: 0.5,
            ..SetupConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn non_positive_tolerance_rejected() {
        for tol in [0.0_f32, -1.0, f32::NAN] {
            let cfg = SetupConfig {
                target_peak_tolerance_db: tol,
                ..SetupConfig::default()
            };
            assert!(cfg.validate().is_err());
        }
    }

    #[test]
    fn zero_size_caps_rejected() {
        for cfg in [
            SetupConfig {
                max_pw_dump_bytes: 0,
                ..SetupConfig::default()
            },
            SetupConfig {
                meter_stdout_chunk_bytes: 0,
                ..SetupConfig::default()
            },
            SetupConfig {
                measure_stdout_cap_bytes: 0,
                ..SetupConfig::default()
            },
            SetupConfig {
                pw_cat_timeout_secs: 0,
                ..SetupConfig::default()
            },
        ] {
            assert!(cfg.validate().is_err());
        }
    }
}
