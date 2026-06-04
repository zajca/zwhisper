//! Pure level-analysis and volume-recommendation math
//! (RFC-mic-setup, "Calibration Algorithm").
//!
//! These functions take decoded mono `f32` PCM (the CLI feeds them the
//! `pw-cat --format=f32` stdout) and never touch a process or the
//! filesystem, so the calibration logic is fully unit-testable with
//! hand-computed vectors. All dB conversion goes through the crate-shared
//! `gain::linear_to_db` (one silence guard, no `NaN`), and the
//! recommendation is clamped and finiteness-checked against
//! [`SetupConfig`] so it can never hand `wpctl` a bogus volume.

use super::config::{SILENCE_FLOOR_DB, SetupConfig};
use crate::gain::linear_to_db;

/// Loudness statistics for one capture window. dBFS with `0 dBFS` =
/// full scale (`|s| = 1.0`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelStats {
    /// Peak level: `20·log10(max|s|)`, or [`SILENCE_FLOOR_DB`] for an
    /// empty / all-zero buffer.
    pub peak_db: f32,
    /// RMS level: `20·log10(sqrt(mean(s²)))`, or [`SILENCE_FLOOR_DB`]
    /// for an empty / all-zero buffer.
    pub rms_db: f32,
    /// Number of samples analysed.
    pub frames: usize,
}

/// Compute peak/RMS dBFS for a mono `f32` buffer.
///
/// An empty buffer, or one whose samples are all exactly zero, reports
/// `peak_db = rms_db = `[`SILENCE_FLOOR_DB`] — a finite floor sentinel,
/// never `NaN` or a raw `-inf` that would poison later `clamp`/compare
/// logic. Non-finite samples (should not occur from `pw-cat`, but a
/// hostile/garbled stream might contain them) are ignored for the peak
/// and treated as `0.0` for the RMS sum, so they cannot produce a `NaN`
/// result either.
pub fn analyze(samples: &[f32]) -> LevelStats {
    let frames = samples.len();
    if frames == 0 {
        return LevelStats {
            peak_db: SILENCE_FLOOR_DB,
            rms_db: SILENCE_FLOOR_DB,
            frames: 0,
        };
    }

    let mut peak: f32 = 0.0;
    let mut sum_sq: f64 = 0.0;
    for &s in samples {
        if !s.is_finite() {
            // Garbage sample — do not let it set the peak or NaN the
            // RMS. Counts as silence for this position.
            continue;
        }
        let mag = s.abs();
        if mag > peak {
            peak = mag;
        }
        sum_sq += f64::from(s) * f64::from(s);
    }

    // mean(s²) in f64 to keep precision over long windows, then back to
    // f32 for the dB conversion. `frames > 0` here, so no divide-by-zero.
    let mean_sq = sum_sq / frames as f64;
    let rms = mean_sq.sqrt() as f32;

    let peak_db = floor(linear_to_db(peak));
    let rms_db = floor(linear_to_db(rms));

    LevelStats {
        peak_db,
        rms_db,
        frames,
    }
}

/// Clamp a raw dB value up to the finite silence floor so callers never
/// see `-inf` (which `linear_to_db(0.0)` returns for true silence).
fn floor(db: f32) -> f32 {
    if db < SILENCE_FLOOR_DB {
        SILENCE_FLOOR_DB
    } else {
        db
    }
}

/// Recommend a new linear volume to move the measured speech peak toward
/// the configured target.
///
/// Applies the RFC formula
/// `new = current · 10^((target_peak_db − measured_peak_db)/20)`, then
/// clamps to `[cfg.min_volume, cfg.max_volume]`. The result is always
/// finite and within the cap: if the inputs are degenerate (a non-finite
/// current volume or a silence-floor measurement that yields a huge
/// ratio), the clamp still bounds the output, and a non-finite product
/// collapses to `cfg.max_volume` (treat "louder than representable" as
/// "go to the cap") rather than escaping as `inf`/`NaN`.
pub fn recommend_volume(current_linear: f32, measured_peak_db: f32, cfg: &SetupConfig) -> f32 {
    // A non-finite or negative current volume is meaningless; start from
    // the cap so the clamp below produces a usable value instead of
    // propagating garbage.
    let current = if current_linear.is_finite() && current_linear >= 0.0 {
        current_linear
    } else {
        cfg.max_volume
    };

    let delta_db = cfg.target_peak_db - measured_peak_db;
    let factor = crate::gain::db_to_linear(delta_db);
    let proposed = current * factor;

    if !proposed.is_finite() {
        // Ratio blew up (e.g. measured at the silence floor): the mic is
        // effectively silent, so push to the cap and let the caller's
        // too-quiet check fire if even that is not enough.
        return cfg.max_volume;
    }

    proposed.clamp(cfg.min_volume, cfg.max_volume)
}

/// Whether a measured peak is within `tolerance_db` of the target.
/// `|peak_db − target_db| <= tolerance_db`. A non-finite peak (it should
/// be floored by [`analyze`], but be defensive) is never "within
/// tolerance".
pub fn within_tolerance(peak_db: f32, target_db: f32, tolerance_db: f32) -> bool {
    if !peak_db.is_finite() {
        return false;
    }
    (peak_db - target_db).abs() <= tolerance_db
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const EPS: f32 = 0.05;

    /// `analyze` returns [`SILENCE_FLOOR_DB`] verbatim for silence, so an
    /// exact compare is semantically correct; use a tiny epsilon to keep
    /// clippy's `float_cmp` happy without weakening the assertion.
    fn is_floor(db: f32) -> bool {
        (db - SILENCE_FLOOR_DB).abs() < f32::EPSILON
    }

    #[test]
    fn empty_buffer_reports_silence_floor_without_nan() {
        let stats = analyze(&[]);
        assert_eq!(stats.frames, 0);
        assert!(is_floor(stats.peak_db));
        assert!(is_floor(stats.rms_db));
        assert!(!stats.peak_db.is_nan() && !stats.rms_db.is_nan());
    }

    #[test]
    fn all_zero_buffer_reports_silence_floor() {
        let stats = analyze(&[0.0_f32; 256]);
        assert_eq!(stats.frames, 256);
        assert!(is_floor(stats.peak_db));
        assert!(is_floor(stats.rms_db));
    }

    #[test]
    fn full_scale_constant_is_about_zero_dbfs() {
        // A constant ±1.0 signal: peak = 1.0 → 0 dBFS, rms = 1.0 → 0 dBFS.
        let stats = analyze(&[1.0_f32; 1000]);
        assert!(stats.peak_db.abs() < EPS, "peak {}", stats.peak_db);
        assert!(stats.rms_db.abs() < EPS, "rms {}", stats.rms_db);
    }

    #[test]
    fn half_scale_constant_is_about_minus_six_dbfs() {
        let stats = analyze(&[0.5_f32; 1000]);
        assert!(
            (stats.peak_db - (-6.0206)).abs() < EPS,
            "peak {}",
            stats.peak_db
        );
        assert!(
            (stats.rms_db - (-6.0206)).abs() < EPS,
            "rms {}",
            stats.rms_db
        );
    }

    #[test]
    fn known_vector_has_hand_computed_peak_and_rms() {
        // samples: [1, -1, 1, -1] → peak |s| = 1 → 0 dBFS;
        // mean(s²) = 1 → rms = 1 → 0 dBFS.
        let stats = analyze(&[1.0, -1.0, 1.0, -1.0]);
        assert!(stats.peak_db.abs() < EPS);
        assert!(stats.rms_db.abs() < EPS);

        // samples: [0.5, -0.5] → peak 0.5 → −6.02 dB; rms 0.5 → −6.02 dB.
        let stats = analyze(&[0.5, -0.5]);
        assert!((stats.peak_db - (-6.0206)).abs() < EPS);
        assert!((stats.rms_db - (-6.0206)).abs() < EPS);
    }

    #[test]
    fn peak_exceeds_or_equals_rms_for_dynamic_signal() {
        // A spike among quiet samples: peak >> rms.
        let mut buf = vec![0.01_f32; 999];
        buf.push(1.0);
        let stats = analyze(&buf);
        assert!(stats.peak_db > stats.rms_db, "{stats:?}");
        assert!(
            stats.peak_db.abs() < EPS,
            "peak should be ~0 dBFS: {stats:?}"
        );
    }

    #[test]
    fn non_finite_samples_are_ignored_no_nan() {
        let buf = [0.5_f32, f32::NAN, f32::INFINITY, -0.5];
        let stats = analyze(&buf);
        assert!(!stats.peak_db.is_nan(), "{stats:?}");
        assert!(!stats.rms_db.is_nan(), "{stats:?}");
        // Peak comes from the finite ±0.5 → −6.02 dB.
        assert!((stats.peak_db - (-6.0206)).abs() < EPS, "{stats:?}");
    }

    #[test]
    fn recommend_raises_volume_when_too_quiet() {
        let cfg = SetupConfig::default();
        // Measured well below target → recommend a higher volume.
        let current = 0.25;
        let new = recommend_volume(current, cfg.target_peak_db - 12.0, &cfg);
        assert!(new > current, "{new} should exceed {current}");
        assert!(new <= cfg.max_volume);
    }

    #[test]
    fn recommend_lowers_volume_when_too_loud() {
        let cfg = SetupConfig::default();
        let current = 0.8;
        // Measured 12 dB above target → recommend a lower volume.
        let new = recommend_volume(current, cfg.target_peak_db + 12.0, &cfg);
        assert!(new < current, "{new} should be below {current}");
        assert!(new >= cfg.min_volume);
    }

    #[test]
    fn recommend_clamps_at_max_volume() {
        let cfg = SetupConfig {
            max_volume: 0.5,
            ..SetupConfig::default()
        };
        // Hugely too quiet at already-high volume → would exceed cap.
        let new = recommend_volume(0.4, SILENCE_FLOOR_DB, &cfg);
        assert!((new - 0.5).abs() < 1e-6, "{new} should clamp to 0.5");
    }

    #[test]
    fn recommend_clamps_at_min_volume() {
        let cfg = SetupConfig {
            min_volume: 0.1,
            ..SetupConfig::default()
        };
        // Hugely too loud → would go below min.
        let new = recommend_volume(0.5, cfg.target_peak_db + 60.0, &cfg);
        assert!((new - 0.1).abs() < 1e-6, "{new} should clamp to 0.1");
    }

    #[test]
    fn recommend_is_finite_for_degenerate_inputs() {
        let cfg = SetupConfig::default();
        for (cur, meas) in [
            (f32::NAN, -7.5),
            (f32::INFINITY, -7.5),
            (-1.0, -7.5),
            (0.25, f32::NEG_INFINITY),
            (0.25, SILENCE_FLOOR_DB),
        ] {
            let new = recommend_volume(cur, meas, &cfg);
            assert!(new.is_finite(), "({cur}, {meas}) -> {new}");
            assert!(
                (cfg.min_volume..=cfg.max_volume).contains(&new),
                "({cur}, {meas}) -> {new}"
            );
        }
    }

    #[test]
    fn recommend_at_target_keeps_volume_roughly_stable() {
        let cfg = SetupConfig::default();
        let current = 0.45;
        let new = recommend_volume(current, cfg.target_peak_db, &cfg);
        // At target, factor ≈ 1.0, so the volume should barely move.
        assert!((new - current).abs() < 1e-4, "{new} vs {current}");
    }

    #[test]
    fn under_responsive_mock_converges_within_iteration_cap() {
        // Model an under-responsive hardware gain stage: doubling the
        // linear volume only raises the measured peak by ~3 dB instead
        // of the ideal 6 dB. Drive the same apply→measure loop the CLI
        // will run and assert it reaches tolerance (or the cap) within
        // the configured iteration count — i.e. it terminates.
        let cfg = SetupConfig::default();

        // measured_peak_db as a function of linear volume for this mock.
        // At volume 1.0 the mock peaks at −3 dBFS; halving volume drops
        // it by 3 dB (the under-response), so volume `v` ->
        // −3 + 3*log2(v) dB. db = -3 + 3*log2(v).
        let measure = |v: f32| -> f32 {
            if v <= 0.0 {
                SILENCE_FLOOR_DB
            } else {
                -3.0 + 3.0 * v.log2()
            }
        };

        let mut volume = 0.05_f32; // start very quiet
        let mut converged = false;
        for _ in 0..cfg.max_iterations {
            let peak = measure(volume);
            if within_tolerance(peak, cfg.target_peak_db, cfg.target_peak_tolerance_db) {
                converged = true;
                break;
            }
            let next = recommend_volume(volume, peak, &cfg);
            // The loop must always make progress or hit the cap; assert
            // it never produces a non-finite / out-of-range volume.
            assert!(next.is_finite());
            assert!((cfg.min_volume..=cfg.max_volume).contains(&next));
            volume = next;
        }
        // Either it converged, or it parked at the cap (a legitimately
        // too-quiet outcome the CLI would report) — the point is it
        // terminated without looping forever.
        let final_peak = measure(volume);
        assert!(
            converged
                || (volume - cfg.max_volume).abs() < 1e-6
                || within_tolerance(final_peak, cfg.target_peak_db, cfg.target_peak_tolerance_db),
            "did not terminate cleanly: volume={volume} peak={final_peak}"
        );
    }

    #[test]
    fn within_tolerance_basic() {
        assert!(within_tolerance(-7.0, -7.5, 1.5));
        assert!(within_tolerance(-9.0, -7.5, 1.5));
        assert!(within_tolerance(-6.0, -7.5, 1.5));
        assert!(!within_tolerance(-3.0, -7.5, 1.5));
        assert!(!within_tolerance(-12.0, -7.5, 1.5));
    }

    #[test]
    fn within_tolerance_rejects_non_finite_peak() {
        assert!(!within_tolerance(f32::NEG_INFINITY, -7.5, 1.5));
        assert!(!within_tolerance(f32::NAN, -7.5, 1.5));
    }
}
