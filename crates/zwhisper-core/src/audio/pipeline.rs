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
/// is written at, the ASR rate the fan-out branch normalizes to, and
/// whether to add the ASR fan-out branch at all.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PipelineParams {
    /// Native FLAC rate (16 kHz / 44.1 kHz / 48 kHz).
    pub native_rate_hz: u32,
    /// ASR-normalized rate of the fan-out branch (typically 16 kHz).
    pub asr_rate_hz: u32,
    /// When true, a `tee` feeds both the FLAC writer (native rate) and
    /// an `appsink` (ASR rate, mono `f32`) for live PCM capture.
    pub capture_pcm: bool,
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
    let escaped_monitor = escape_for_parse_launch(&selection.monitor_node);

    let description = pipeline_description(&escaped_mic, &escaped_monitor, &escaped_output, params);
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

/// Build the `gst::parse::launch` description. Pure (no GStreamer init)
/// so the shape is unit-testable.
///
/// `gst::parse::launch` tokenises by whitespace and uses backslash for
/// escaping; the `target-object` values are also covered by the strict
/// `[A-Za-z0-9._:-]+` validation in `audio::devices`, so the
/// double-defence keeps any future caller from injecting elements.
///
/// Mic + sink-monitor mono mix only. The mixed mono stream is produced
/// at the native rate; with PCM capture it fans out through a `tee` to
/// (1) a FLAC writer at the native rate and (2) an `appsink` resampled
/// to the ASR rate as mono `f32`.
fn pipeline_description(
    escaped_mic: &str,
    escaped_monitor: &str,
    escaped_output: &str,
    params: PipelineParams,
) -> String {
    let native = params.native_rate_hz;
    let sources = format!(
        "pipewiresrc target-object=\"{escaped_mic}\" ! audioconvert ! audioresample ! mix. \
         pipewiresrc target-object=\"{escaped_monitor}\" ! audioconvert ! audioresample ! mix. \
         audiomixer name=mix ! audioconvert ! audioresample ! \
         audio/x-raw,format=S16LE,rate={native},channels=1"
    );

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
        }
    }

    #[test]
    fn description_without_capture_is_single_flac_branch() {
        let d = pipeline_description("mic", "mon", "/out.flac", params(48_000, false));
        assert!(d.contains("rate=48000"), "{d}");
        assert!(
            d.contains("flacenc ! filesink location=\"/out.flac\""),
            "{d}"
        );
        assert!(!d.contains("tee"), "no tee without capture: {d}");
        assert!(!d.contains("appsink"), "{d}");
    }

    #[test]
    fn description_with_capture_has_tee_flac_and_asr_appsink() {
        let d = pipeline_description("mic", "mon", "/out.flac", params(44_100, true));
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
