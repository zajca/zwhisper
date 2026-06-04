use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use tracing::debug;

use super::devices::DeviceSelection;
use super::error::RecordingError;

/// Element name of the ASR fan-out appsink (RFC Phase 4).
pub(crate) const ASR_SINK_NAME: &str = "asr_sink";

/// Parameters for [`build`]: the native capture rate the FLAC artifact
/// is written at, the ASR rate the fan-out branch normalizes to, whether
/// to add the ASR fan-out branch at all, and the optional software input
/// trim applied to the mic branch.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PipelineParams {
    /// Native FLAC rate (16 kHz / 44.1 kHz / 48 kHz).
    pub native_rate_hz: u32,
    /// ASR-normalized rate of the fan-out branch (typically 16 kHz).
    pub asr_rate_hz: u32,
    /// When true, a `tee` feeds both the FLAC writer (native rate) and
    /// an `appsink` (ASR rate, mono `f32`) for live PCM capture.
    pub capture_pcm: bool,
    /// Optional zwhisper-owned software input trim in decibels
    /// (RFC-mic-setup Phase 3). When `Some(db)` and the linear factor
    /// differs from unity by more than [`GAIN_UNITY_EPSILON`], a
    /// `volume` element is inserted on the **mic** branch (never the
    /// monitor) right after `audioresample`. The factor is
    /// [`crate::gain::db_to_linear`] clamped to the shared
    /// `gain::MIN_INPUT_GAIN_DB`..=`gain::MAX_INPUT_GAIN_DB` linear
    /// bounds, so a profile that slipped past validation still cannot
    /// drive the element out of range.
    pub input_gain_db: Option<f32>,
}

/// Build the capture pipeline. With `params.capture_pcm`, the mixed
/// mono stream fans out through a `tee`: branch 1 encodes FLAC at the
/// native rate (the durable, full-fidelity artifact); branch 2
/// resamples to the ASR rate as mono `f32` into an `appsink` the
/// recorder drains for live PCM. The FLAC branch is independent of the
/// ASR branch, so a stalled/failed ASR sink never corrupts the FLAC.
///
/// Returns the built pipeline, a `BuiltOutput` ownership token (so a
/// later failure removes only files *we* created), and the ASR
/// `AppSink` when PCM capture is enabled.
pub(crate) fn build(
    selection: &DeviceSelection,
    output_path: &Path,
    params: PipelineParams,
) -> Result<(gst::Pipeline, BuiltOutput, Option<gst_app::AppSink>), RecordingError> {
    let owned = precreate_output(output_path)?;
    let token = BuiltOutput {
        path: output_path.to_owned(),
        owned,
    };

    match build_inner(selection, output_path, params) {
        Ok((pipeline, sink)) => Ok((pipeline, token, sink)),
        Err(e) => {
            // Pipeline build failed; remove the file *only* if we
            // created it (`owned == true`). If the user's file was
            // already there `precreate_output` would have returned
            // `EEXIST` before this point, so this branch only ever
            // sees files we precreated.
            token.cleanup_on_failure();
            Err(e)
        }
    }
}

/// Receipt issued by `pipeline::build` so the caller can clean up the
/// output file on a *later* failure (e.g., `set_state(Playing)`)
/// without risking a user-owned file. The token does not implement
/// `Drop`-time cleanup intentionally — the recorder controls when the
/// file is removed (success → keep, failure → delete).
#[derive(Debug)]
pub(crate) struct BuiltOutput {
    pub(crate) path: std::path::PathBuf,
    /// `true` exactly when this process created the file via
    /// `OpenOptions::create_new`. `false` is reserved for future
    /// "append to existing" modes and is never produced today.
    pub(crate) owned: bool,
}

impl BuiltOutput {
    /// Remove the file iff we created it. Best-effort — failures are
    /// logged at debug, never propagated, because we are already on
    /// an error return path.
    pub(crate) fn cleanup_on_failure(&self) {
        if !self.owned {
            return;
        }
        if let Err(rm_err) = std::fs::remove_file(&self.path) {
            debug!(?rm_err, path = %self.path.display(),
                   "could not remove precreated output file after failure");
        }
    }
}

fn build_inner(
    selection: &DeviceSelection,
    output_path: &Path,
    params: PipelineParams,
) -> Result<(gst::Pipeline, Option<gst_app::AppSink>), RecordingError> {
    let path_str = output_path
        .to_str()
        .ok_or_else(|| RecordingError::OutputPath {
            path: output_path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "output path is not valid UTF-8",
            ),
        })?;
    let escaped_output = escape_for_parse_launch(path_str);
    let escaped_mic = escape_for_parse_launch(&selection.mic_node);
    // Mic-only (`monitor_node == None`) builds a single-source graph;
    // `Some` adds the monitor branch + `audiomixer`.
    let escaped_monitor = selection
        .monitor_node
        .as_deref()
        .map(escape_for_parse_launch);

    let description = pipeline_description(
        &escaped_mic,
        escaped_monitor.as_deref(),
        &escaped_output,
        params,
    );
    debug!(%description, "constructed gstreamer pipeline description");

    let element = gst::parse::launch(&description).map_err(|e| RecordingError::PipelineFailed {
        stage: "parse_launch".into(),
        source: Box::new(e),
    })?;

    let pipeline =
        element
            .downcast::<gst::Pipeline>()
            .map_err(|_| RecordingError::PipelineFailed {
                stage: "downcast_pipeline".into(),
                source: "parse::launch did not return a gst::Pipeline".into(),
            })?;

    let sink = if params.capture_pcm {
        let by_name =
            pipeline
                .by_name(ASR_SINK_NAME)
                .ok_or_else(|| RecordingError::PipelineFailed {
                    stage: "find_asr_sink".into(),
                    source: "pipeline is missing the asr_sink appsink".into(),
                })?;
        let app_sink =
            by_name
                .downcast::<gst_app::AppSink>()
                .map_err(|_| RecordingError::PipelineFailed {
                    stage: "downcast_asr_sink".into(),
                    source: "asr_sink is not an AppSink".into(),
                })?;
        Some(app_sink)
    } else {
        None
    };

    Ok((pipeline, sink))
}

/// Largest deviation of the linear gain factor from unity (1.0) that is
/// still treated as "no trim". `input_gain_db = 0.0` maps to exactly
/// `1.0`, but a value rounded to `{:.6}` (the element's formatted
/// precision) can land a hair off; below this epsilon the `volume`
/// element is omitted entirely so a no-op trim never alters the graph.
const GAIN_UNITY_EPSILON: f32 = 1e-4;

/// The `volume volume={linear} ! ` element for the mic branch, or an
/// empty string when no audible trim applies. `None` gain, a non-finite
/// value, or a factor within [`GAIN_UNITY_EPSILON`] of unity all yield
/// no element (the common, untrimmed case stays byte-for-byte as before).
///
/// The factor is [`crate::gain::db_to_linear`] clamped to the shared
/// `gain` range's linear bounds — defence in depth on top of
/// `Profile::validate`, so a profile that somehow carried an
/// out-of-range dB still cannot push the element past the sane window.
fn mic_volume_element(input_gain_db: Option<f32>) -> String {
    let Some(db) = input_gain_db else {
        return String::new();
    };
    if !db.is_finite() {
        return String::new();
    }
    let min_linear = crate::gain::db_to_linear(crate::gain::MIN_INPUT_GAIN_DB);
    let max_linear = crate::gain::db_to_linear(crate::gain::MAX_INPUT_GAIN_DB);
    let linear = crate::gain::db_to_linear(db).clamp(min_linear, max_linear);
    if (linear - 1.0).abs() <= GAIN_UNITY_EPSILON {
        return String::new();
    }
    format!("volume volume={linear:.6} ! ")
}

/// Build the `gst::parse::launch` description. Pure (no GStreamer init)
/// so the shape is unit-testable.
///
/// `gst::parse::launch` tokenises by whitespace and uses backslash for
/// escaping; the `target-object` values are also covered by the strict
/// `[A-Za-z0-9._:-]+` validation in `audio::devices`, so the
/// double-defence keeps any future caller from injecting elements.
///
/// Two shapes depending on `escaped_monitor`:
/// - `Some(monitor)` — mic + sink-monitor **mono mix** through an
///   `audiomixer`.
/// - `None` — **mic-only** (RFC-mic-setup Phase 5): a single
///   `pipewiresrc` with no `audiomixer`.
///
/// In both shapes the optional mic `volume` trim sits right after the
/// mic's `audioresample`. The resulting mono stream is produced at the
/// native rate; with PCM capture it fans out through a `tee` to (1) a
/// FLAC writer at the native rate and (2) an `appsink` resampled to the
/// ASR rate as mono `f32`. The `tee`/ASR fan-out is identical in both
/// shapes.
fn pipeline_description(
    escaped_mic: &str,
    escaped_monitor: Option<&str>,
    escaped_output: &str,
    params: PipelineParams,
) -> String {
    let native = params.native_rate_hz;
    let mic_volume = mic_volume_element(params.input_gain_db);
    let sources = match escaped_monitor {
        Some(monitor) => {
            // Downmix each source to mono *before* the `audiomixer`.
            // Feeding the mixer two stereo (multi-channel) inputs and
            // downmixing to mono only at the mixer output measured ~22 dB
            // quieter than the same mic captured mono — loud enough on a
            // meeting-volume monitor but so far below the noise floor for
            // a single quiet mic that whisper.cpp and Parakeet
            // transcribed silence. Forcing `channels=1` on each pad mixes
            // mono+mono and restores the mic to its true level (verified
            // against a direct `pw-record` reference). The trailing mono
            // caps on the mixer output stay as a belt-and-braces
            // guarantee for flacenc. The optional mic `volume` trim sits
            // on the mic pad only — the monitor is never trimmed.
            format!(
                "pipewiresrc target-object=\"{escaped_mic}\" ! audioconvert ! audioresample ! \
                 {mic_volume}audio/x-raw,channels=1 ! mix. \
                 pipewiresrc target-object=\"{monitor}\" ! audioconvert ! audioresample ! \
                 audio/x-raw,channels=1 ! mix. \
                 audiomixer name=mix ! audioconvert ! audioresample ! \
                 audio/x-raw,format=S16LE,rate={native},channels=1"
            )
        }
        None => {
            // Mic-only (RFC-mic-setup Phase 5): a single source, no
            // `audiomixer`. The optional `volume` trim sits right after
            // `audioresample`; the mono/native-rate caps match the
            // mixer-output caps of the two-source shape so the downstream
            // `tee`/FLAC/ASR fan-out is byte-for-byte identical.
            format!(
                "pipewiresrc target-object=\"{escaped_mic}\" ! audioconvert ! audioresample ! \
                 {mic_volume}audio/x-raw,format=S16LE,rate={native},channels=1"
            )
        }
    };

    if params.capture_pcm {
        let asr = params.asr_rate_hz;
        // Branch 1 (FLAC, native rate) and branch 2 (ASR appsink, mono
        // f32 at the ASR rate) hang off a `tee`. `queue` decouples the
        // two so a slow ASR consumer cannot backpressure the FLAC
        // writer. `sync=false` lets the appsink pull as fast as the
        // recorder drains; the recorder bounds memory itself.
        format!(
            "{sources} ! tee name=asr_tee \
             asr_tee. ! queue ! flacenc ! filesink location=\"{escaped_output}\" \
             asr_tee. ! queue leaky=no ! audioconvert ! audioresample ! \
             audio/x-raw,format=F32LE,rate={asr},channels=1 ! \
             appsink name={ASR_SINK_NAME} sync=false max-buffers=0 drop=false"
        )
    } else {
        format!("{sources} ! flacenc ! filesink location=\"{escaped_output}\"")
    }
}

/// Returns `true` to indicate this process created the file. Today
/// the only success path is `create_new`, so a successful return
/// always means we own the file. If `create_new` fails (most
/// commonly `EEXIST` because the user already has a file at that
/// path) we propagate the underlying `io::Error` and never advertise
/// ownership.
fn precreate_output(path: &Path) -> Result<bool, RecordingError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| RecordingError::OutputPath {
                path: path.to_owned(),
                source,
            })?;
        }
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| RecordingError::OutputPath {
            path: path.to_owned(),
            source,
        })?;
    Ok(true)
}

/// `gst::parse::launch` treats `\` as escape and breaks on quotes, so
/// any embedded `"` or `\` in a node name has to be doubled.
/// `PipeWire` node names never contain these in practice, but
/// defending against it is cheap and prevents a subtle injection if a
/// future caller passes a hostile string.
fn escape_for_parse_launch(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', r#"\""#)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn params(native: u32, capture: bool) -> PipelineParams {
        PipelineParams {
            native_rate_hz: native,
            asr_rate_hz: 16_000,
            capture_pcm: capture,
            input_gain_db: None,
        }
    }

    fn params_gain(native: u32, capture: bool, input_gain_db: Option<f32>) -> PipelineParams {
        PipelineParams {
            native_rate_hz: native,
            asr_rate_hz: 16_000,
            capture_pcm: capture,
            input_gain_db,
        }
    }

    #[test]
    fn description_without_capture_is_single_flac_branch() {
        let d = pipeline_description("mic", Some("mon"), "/out.flac", params(48_000, false));
        assert!(d.contains("rate=48000"), "{d}");
        assert!(
            d.contains("flacenc ! filesink location=\"/out.flac\""),
            "{d}"
        );
        assert!(!d.contains("tee"), "no tee without capture: {d}");
        assert!(!d.contains("appsink"), "{d}");
    }

    #[test]
    fn each_source_is_downmixed_to_mono_before_the_mixer() {
        // Regression: feeding the audiomixer stereo inputs and
        // downmixing only at the output dropped the mic ~22 dB and
        // made transcription fail. Both source branches must carry
        // an explicit `channels=1` cap *before* the `mix.` link.
        let d = pipeline_description("mic", Some("mon"), "/out.flac", params(16_000, true));
        assert_eq!(
            d.matches("audio/x-raw,channels=1 ! mix.").count(),
            2,
            "both source pads must be mono before the mixer: {d}"
        );
    }

    #[test]
    fn description_with_capture_has_tee_flac_and_asr_appsink() {
        let d = pipeline_description("mic", Some("mon"), "/out.flac", params(44_100, true));
        // FLAC branch at native rate.
        assert!(d.contains("rate=44100"), "native flac rate: {d}");
        assert!(d.contains("flacenc ! filesink"), "{d}");
        // Fan-out tee + ASR appsink at 16 kHz mono f32.
        assert!(d.contains("tee name=asr_tee"), "{d}");
        assert!(d.contains("appsink name=asr_sink"), "{d}");
        assert!(d.contains("format=F32LE,rate=16000,channels=1"), "{d}");
        assert!(d.matches("asr_tee.").count() >= 2, "two tee branches: {d}");
    }

    #[test]
    fn mic_only_description_has_no_audiomixer_and_single_source() {
        // RFC-mic-setup Phase 5: `monitor == None` builds a single-source
        // graph with no `audiomixer`/`mix.` and exactly one
        // `pipewiresrc`. The FLAC branch must still be present.
        let d = pipeline_description("mic", None, "/out.flac", params(48_000, false));
        assert!(!d.contains("audiomixer"), "no mixer for mic-only: {d}");
        assert!(!d.contains("mix."), "no mixer pads for mic-only: {d}");
        assert_eq!(
            d.matches("pipewiresrc").count(),
            1,
            "mic-only must have a single source: {d}"
        );
        assert!(d.contains("target-object=\"mic\""), "{d}");
        assert!(
            d.contains("format=S16LE,rate=48000,channels=1"),
            "native mono caps: {d}"
        );
        assert!(
            d.contains("flacenc ! filesink location=\"/out.flac\""),
            "FLAC branch present: {d}"
        );
    }

    #[test]
    fn mic_only_description_keeps_identical_tee_fanout() {
        // The `tee`/ASR-appsink fan-out must be identical to the
        // mono-mix shape so capture_pcm works the same in both modes.
        let d = pipeline_description("mic", None, "/out.flac", params(44_100, true));
        assert!(!d.contains("audiomixer"), "no mixer for mic-only: {d}");
        assert!(d.contains("tee name=asr_tee"), "{d}");
        assert!(d.contains("appsink name=asr_sink"), "{d}");
        assert!(d.contains("format=F32LE,rate=16000,channels=1"), "{d}");
        assert!(d.matches("asr_tee.").count() >= 2, "two tee branches: {d}");
        assert!(d.contains("rate=44100"), "native flac rate: {d}");
    }

    #[test]
    fn volume_element_present_with_correct_factor_when_gain_set() {
        // -6 dB ≈ 0.501187 linear; formatted to 6 decimals.
        let d = pipeline_description(
            "mic",
            Some("mon"),
            "/out.flac",
            params_gain(48_000, false, Some(-6.0)),
        );
        let expected = format!("volume volume={:.6} !", crate::gain::db_to_linear(-6.0));
        assert!(d.contains(&expected), "expected `{expected}` in: {d}");
        // The trim sits on the mic branch before its mono cap, not on
        // the monitor branch. `str::split` always yields at least one
        // element, so the first segment (everything up to the first
        // `mix.`) is the mic pad.
        let mic_seg = d.split("mix.").next().unwrap_or_default();
        assert!(
            mic_seg.contains("volume volume="),
            "volume must be on the mic branch: {mic_seg}"
        );
        // Exactly one volume element (mic only, never the monitor).
        assert_eq!(
            d.matches("volume volume=").count(),
            1,
            "exactly one volume element on the mic branch: {d}"
        );
    }

    #[test]
    fn volume_element_present_in_mic_only_mode() {
        let d = pipeline_description(
            "mic",
            None,
            "/out.flac",
            params_gain(16_000, true, Some(6.0)),
        );
        let expected = format!("volume volume={:.6} !", crate::gain::db_to_linear(6.0));
        assert!(d.contains(&expected), "expected `{expected}` in: {d}");
        assert_eq!(d.matches("volume volume=").count(), 1, "{d}");
    }

    #[test]
    fn volume_element_absent_when_gain_is_none() {
        let d = pipeline_description(
            "mic",
            Some("mon"),
            "/out.flac",
            params_gain(48_000, false, None),
        );
        assert!(!d.contains("volume volume="), "no trim when None: {d}");
    }

    #[test]
    fn volume_element_absent_when_gain_is_zero_db() {
        // 0 dB is exactly unity (factor 1.0); the element is omitted so
        // an untrimmed profile produces a byte-for-byte unchanged graph.
        let d = pipeline_description(
            "mic",
            Some("mon"),
            "/out.flac",
            params_gain(48_000, false, Some(0.0)),
        );
        assert!(!d.contains("volume volume="), "no trim at 0 dB: {d}");
        // And the mono-mix downmix regression still holds with gain set
        // to a no-op.
        assert_eq!(d.matches("audio/x-raw,channels=1 ! mix.").count(), 2, "{d}");
    }

    #[test]
    fn volume_element_absent_for_non_finite_gain() {
        // Defence in depth: a NaN/inf that slipped past validation must
        // not emit a `volume=NaN` element (gst would reject it).
        for db in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let d = pipeline_description(
                "mic",
                Some("mon"),
                "/out.flac",
                params_gain(48_000, false, Some(db)),
            );
            assert!(!d.contains("volume volume="), "non-finite {db}: {d}");
        }
    }

    #[test]
    fn volume_factor_is_clamped_to_gain_range() {
        // A dB beyond the shared range (which validation would reject,
        // but the pipeline clamps defensively) must not exceed the
        // max-linear factor.
        let max_linear = crate::gain::db_to_linear(crate::gain::MAX_INPUT_GAIN_DB);
        let d = pipeline_description(
            "mic",
            Some("mon"),
            "/out.flac",
            params_gain(48_000, false, Some(1000.0)),
        );
        let expected = format!("volume volume={max_linear:.6} !");
        assert!(
            d.contains(&expected),
            "clamped to max: expected `{expected}` in {d}"
        );
    }

    #[test]
    fn escape_is_identity_for_plain_node_names() {
        assert_eq!(
            escape_for_parse_launch("alsa_input.usb-Generic_PHL-00.analog-stereo"),
            "alsa_input.usb-Generic_PHL-00.analog-stereo"
        );
    }

    #[test]
    fn escape_handles_quotes_and_backslash() {
        assert_eq!(escape_for_parse_launch(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn precreate_output_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phase3.flac");
        let owned = precreate_output(&path).unwrap();
        assert!(
            owned,
            "precreate must report ownership for the file it just created"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn precreate_output_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phase3.flac");
        precreate_output(&path).unwrap();
        let err = precreate_output(&path).unwrap_err();
        match err {
            RecordingError::OutputPath { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists);
            }
            other => panic!("expected OutputPath, got {other:?}"),
        }
    }
}
