//! Whole-session level statistics and the clipping / silence verdicts
//! (RFC-actionable-errors § F4, § F5).
//!
//! The capture pipeline carries a `GStreamer` `level` element that posts
//! one element message per interval with peak and RMS **already in
//! dBFS**. The recorder's bus thread folds each message into a
//! [`LevelSummary`] — five scalars, so a four-hour recording costs
//! exactly what a four-second one does and no PCM is retained.
//!
//! [`diagnose_levels`] turns that summary into a verdict. It is
//! deliberately reluctant: it declines on a recording too short to
//! measure, on non-finite input, and on anything that is merely
//! unusual rather than broken. A verdict is a claim made to the user
//! about their hardware, and the caller only ever consults it for a
//! recording that already produced no text.

use super::config::{DiagnosticsConfig, SILENCE_FLOOR_DB};
use super::{FailureCode, FailureReason};
use crate::gain::linear_to_db;

/// Running level statistics for one recording, folded from the `level`
/// element's per-interval messages.
///
/// The energy accumulator is kept **linear** (a duration-weighted mean
/// square) rather than as an average of per-window dB values: dB is
/// logarithmic, so averaging it would under-weight loud windows and
/// over-weight quiet ones, and windows of unequal length would not
/// combine correctly. The conversion to dBFS happens once, in
/// [`Self::rms_db`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelSummary {
    /// Loudest peak seen in any window, dBFS. [`SILENCE_FLOOR_DB`] when
    /// no window has been folded in yet.
    pub max_peak_db: f32,
    /// Duration-weighted sum of each window's mean square, in
    /// `linear² · ms`. Divided by [`Self::total_duration_ms`] to get the
    /// session mean square.
    pub energy_ms: f64,
    /// Total duration covered by the folded windows, ms.
    pub total_duration_ms: u64,
    /// Number of windows folded in.
    pub windows: u32,
    /// Windows whose peak reached the configured clip threshold.
    pub clipped_windows: u32,
}

impl Default for LevelSummary {
    fn default() -> Self {
        Self {
            max_peak_db: SILENCE_FLOOR_DB,
            energy_ms: 0.0,
            total_duration_ms: 0,
            windows: 0,
            clipped_windows: 0,
        }
    }
}

impl LevelSummary {
    /// Fold one `level` message into the summary.
    ///
    /// `peak_db` and `rms_db` are the element's per-window values in
    /// dBFS; `duration_ms` is the window length. A non-finite dB value
    /// or a zero-length window is ignored entirely rather than
    /// poisoning the accumulator — the `level` element should never
    /// produce either, but a single bad message must not be able to
    /// turn a healthy recording into a silence verdict.
    ///
    /// `clip_peak_db` is passed in rather than read from a config held
    /// by the summary so this stays a plain value type the bus thread
    /// can own behind a mutex.
    pub fn fold(&mut self, peak_db: f32, rms_db: f32, duration_ms: u64, clip_peak_db: f32) {
        if duration_ms == 0 || !peak_db.is_finite() || !rms_db.is_finite() {
            return;
        }

        if peak_db > self.max_peak_db {
            self.max_peak_db = peak_db;
        }
        if peak_db >= clip_peak_db {
            self.clipped_windows = self.clipped_windows.saturating_add(1);
        }

        // dB -> linear amplitude -> mean square, weighted by the window
        // length so unequal windows combine correctly.
        let amplitude = f64::from(crate::gain::db_to_linear(rms_db));
        self.energy_ms += amplitude * amplitude * duration_ms as f64;
        self.total_duration_ms = self.total_duration_ms.saturating_add(duration_ms);
        self.windows = self.windows.saturating_add(1);
    }

    /// Whether any window has been folded in. A summary that never saw
    /// a `level` message says nothing about the recording — the
    /// pipeline may simply have failed before `Playing`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows == 0
    }

    /// Session RMS in dBFS, or [`SILENCE_FLOOR_DB`] for an empty
    /// summary or one whose energy is exactly zero.
    #[must_use]
    pub fn rms_db(&self) -> f32 {
        if self.total_duration_ms == 0 || self.energy_ms <= 0.0 {
            return SILENCE_FLOOR_DB;
        }
        let mean_square = self.energy_ms / self.total_duration_ms as f64;
        let db = linear_to_db(mean_square.sqrt() as f32);
        if db < SILENCE_FLOOR_DB {
            SILENCE_FLOOR_DB
        } else {
            db
        }
    }

    /// Fraction of windows that clipped, in `[0.0, 1.0]`. Zero for an
    /// empty summary.
    #[must_use]
    pub fn clipped_ratio(&self) -> f32 {
        if self.windows == 0 {
            return 0.0;
        }
        self.clipped_windows as f32 / self.windows as f32
    }
}

/// Diagnose a recording from its level summary.
///
/// Returns `None` when no confident claim can be made — an empty
/// summary, a recording shorter than `cfg.min_analysis_ms`, or levels
/// that look healthy. The caller consults this **only** for a recording
/// that produced no transcript text; a real transcript is never
/// downgraded to a failure on the strength of a level reading.
///
/// Silence takes precedence over clipping: "nothing was heard" is the
/// stronger, less ambiguous statement, and a summary can satisfy both
/// only through a pathological mix of windows.
#[must_use]
pub fn diagnose_levels(
    summary: &LevelSummary,
    mic_node: &str,
    cfg: &DiagnosticsConfig,
) -> Option<FailureReason> {
    if summary.is_empty() || summary.total_duration_ms < cfg.min_analysis_ms {
        return None;
    }

    let rms_db = summary.rms_db();
    let device = if mic_node.is_empty() {
        "the default input".to_owned()
    } else {
        format!("`{mic_node}`")
    };

    if rms_db < cfg.silent_rms_db {
        return Some(FailureReason::new(
            FailureCode::MicSilent,
            format!(
                "nothing was heard on {device}: the whole {} of audio averaged {rms_db:.1} dBFS, \
                 below the {:.1} dBFS silence threshold",
                format_duration_ms(summary.total_duration_ms),
                cfg.silent_rms_db,
            ),
            format!(
                "check the device and its level with `zwhisper audio meter{}`",
                {
                    if mic_node.is_empty() {
                        String::new()
                    } else {
                        format!(" --source {mic_node}")
                    }
                }
            ),
        ));
    }

    let ratio = summary.clipped_ratio();
    if ratio >= cfg.clip_window_ratio {
        return Some(FailureReason::new(
            FailureCode::MicClipping,
            format!(
                "the input clipped in {:.0}% of the recording (peak {:.1} dBFS on {device}) — \
                 a saturated signal carries no recognisable speech",
                f64::from(ratio) * 100.0,
                summary.max_peak_db,
            ),
            "lower the input level with `zwhisper audio calibrate --apply`".to_owned(),
        ));
    }

    None
}

/// Render a millisecond count the way the verdict messages read best:
/// whole seconds for anything under a minute, `Nm Ss` above it.
fn format_duration_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    format!("{}m {:02}s", secs / 60, secs % 60)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const CLIP_DB: f32 = -0.1;

    /// Build a summary from `(peak_db, rms_db)` windows of equal length.
    fn summary_of(windows: &[(f32, f32)], window_ms: u64) -> LevelSummary {
        let mut s = LevelSummary::default();
        for &(peak, rms) in windows {
            s.fold(peak, rms, window_ms, CLIP_DB);
        }
        s
    }

    /// `n` identical windows — the common shape for the verdict tests.
    fn uniform(n: usize, peak_db: f32, rms_db: f32) -> LevelSummary {
        summary_of(&vec![(peak_db, rms_db); n], 100)
    }

    #[test]
    fn empty_summary_reports_floor_and_no_verdict() {
        let s = LevelSummary::default();
        assert!(s.is_empty());
        assert!((s.rms_db() - SILENCE_FLOOR_DB).abs() < f32::EPSILON);
        assert!((s.clipped_ratio() - 0.0).abs() < f32::EPSILON);
        assert!(diagnose_levels(&s, "mic", &DiagnosticsConfig::default()).is_none());
    }

    #[test]
    fn uniform_windows_average_to_their_own_rms() {
        // Ten identical -20 dBFS windows must average to -20 dBFS, not
        // to something the linear/dB conversion smeared.
        let s = uniform(10, -6.0, -20.0);
        assert!((s.rms_db() - (-20.0)).abs() < 0.05, "got {}", s.rms_db());
    }

    #[test]
    fn unequal_windows_are_duration_weighted() {
        // 900 ms at -40 dBFS then 100 ms at -20 dBFS. A naive mean of
        // the dB values would give -38; the correct duration-weighted
        // power average is ~-29.6.
        let mut s = LevelSummary::default();
        s.fold(-30.0, -40.0, 900, CLIP_DB);
        s.fold(-10.0, -20.0, 100, CLIP_DB);
        let db = s.rms_db();
        assert!((db - (-29.6)).abs() < 0.2, "got {db}");
        assert_eq!(s.total_duration_ms, 1000);
        assert_eq!(s.windows, 2);
    }

    #[test]
    fn max_peak_tracks_the_loudest_window() {
        let s = summary_of(&[(-30.0, -40.0), (-3.0, -12.0), (-25.0, -35.0)], 100);
        assert!((s.max_peak_db - (-3.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn non_finite_and_zero_length_windows_are_ignored() {
        let mut s = LevelSummary::default();
        s.fold(f32::NAN, -20.0, 100, CLIP_DB);
        s.fold(-6.0, f32::INFINITY, 100, CLIP_DB);
        s.fold(-6.0, -20.0, 0, CLIP_DB);
        assert!(s.is_empty(), "no window should have been folded in");

        // A good window after the bad ones still lands cleanly.
        s.fold(-6.0, -20.0, 100, CLIP_DB);
        assert_eq!(s.windows, 1);
        assert!(s.rms_db().is_finite());
    }

    #[test]
    fn silent_recording_is_diagnosed_and_names_the_device() {
        let cfg = DiagnosticsConfig::default();
        // 30 windows x 100 ms = 3 s, well past min_analysis_ms.
        let s = uniform(30, -85.0, -90.0);
        let reason = diagnose_levels(&s, "alsa_input.pci-0000_00_1f.3", &cfg).unwrap();
        assert_eq!(reason.code, FailureCode::MicSilent);
        assert!(
            reason.message.contains("alsa_input.pci-0000_00_1f.3"),
            "message must name the device: {}",
            reason.message
        );
        assert!(reason.action.contains("zwhisper audio meter"));
    }

    #[test]
    fn silent_recording_without_a_named_node_still_diagnoses() {
        let cfg = DiagnosticsConfig::default();
        let reason = diagnose_levels(&uniform(30, -85.0, -90.0), "", &cfg).unwrap();
        assert_eq!(reason.code, FailureCode::MicSilent);
        assert!(reason.message.contains("the default input"));
        // No `--source` argument can be suggested without a node name.
        assert!(!reason.action.contains("--source"));
    }

    #[test]
    fn clipping_fires_exactly_at_the_configured_ratio() {
        let cfg = DiagnosticsConfig::default(); // ratio 0.02
        // 100 windows, 2 of them clipped == exactly 2 %.
        let mut windows = vec![(-12.0_f32, -24.0_f32); 98];
        windows.extend([(0.0_f32, -6.0_f32); 2]);
        let s = summary_of(&windows, 100);
        let reason = diagnose_levels(&s, "mic", &cfg).unwrap();
        assert_eq!(reason.code, FailureCode::MicClipping);
        assert!(reason.action.contains("zwhisper audio calibrate"));
    }

    #[test]
    fn a_single_transient_below_the_ratio_is_not_clipping() {
        let cfg = DiagnosticsConfig::default();
        // 100 windows, 1 clipped == 1 %, under the 2 % threshold.
        let mut windows = vec![(-12.0_f32, -24.0_f32); 99];
        windows.push((0.0, -6.0));
        assert!(diagnose_levels(&summary_of(&windows, 100), "mic", &cfg).is_none());
    }

    #[test]
    fn healthy_recording_gets_no_verdict() {
        let cfg = DiagnosticsConfig::default();
        let s = uniform(30, -7.0, -22.0);
        assert!(diagnose_levels(&s, "mic", &cfg).is_none());
    }

    #[test]
    fn recording_shorter_than_the_analysis_window_gets_no_verdict() {
        let cfg = DiagnosticsConfig::default(); // min 500 ms
        // 4 x 100 ms = 400 ms of dead silence — clearly silent, but too
        // short to be worth a claim.
        let s = uniform(4, -90.0, -95.0);
        assert!(s.rms_db() < cfg.silent_rms_db);
        assert!(diagnose_levels(&s, "mic", &cfg).is_none());
    }

    #[test]
    fn silence_wins_over_clipping() {
        let cfg = DiagnosticsConfig::default();
        // Pathological: a few full-scale windows among a long stretch of
        // digital silence. Both predicates could hold; silence is the
        // stronger statement.
        let mut windows = vec![(SILENCE_FLOOR_DB, SILENCE_FLOOR_DB); 200];
        windows.extend([(0.0_f32, -100.0_f32); 10]);
        let s = summary_of(&windows, 100);
        assert!(s.clipped_ratio() >= cfg.clip_window_ratio);
        let reason = diagnose_levels(&s, "mic", &cfg).unwrap();
        assert_eq!(reason.code, FailureCode::MicSilent);
    }

    #[test]
    fn thresholds_are_honoured_from_the_config() {
        // A recording that is healthy under the defaults must flip to a
        // verdict under a stricter configured threshold, proving the
        // analysis reads the config rather than inlined constants.
        let s = uniform(30, -7.0, -22.0);
        assert!(diagnose_levels(&s, "mic", &DiagnosticsConfig::default()).is_none());
        let strict = DiagnosticsConfig {
            silent_rms_db: -15.0,
            ..DiagnosticsConfig::default()
        };
        assert_eq!(
            diagnose_levels(&s, "mic", &strict).unwrap().code,
            FailureCode::MicSilent
        );
    }

    #[test]
    fn duration_rendering() {
        assert_eq!(format_duration_ms(3_400), "3s");
        assert_eq!(format_duration_ms(59_999), "59s");
        assert_eq!(format_duration_ms(60_000), "1m 00s");
        assert_eq!(format_duration_ms(125_000), "2m 05s");
    }
}
