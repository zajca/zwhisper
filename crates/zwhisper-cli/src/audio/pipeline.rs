use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::debug;

use super::devices::DeviceSelection;
use super::error::RecordingError;

/// Build the M0 mono-mix pipeline using `gst::parse::launch`. The
/// shape mirrors IDEA.md § 3 verbatim. Returns the built pipeline and
/// a `BuiltOutput` token — the token tells the recorder that *we* are
/// the ones who created the file, so it is safe to remove on a later
/// failure. Without that signal a retry against an existing user file
/// would happily delete the user's data.
pub(crate) fn build(
    selection: &DeviceSelection,
    output_path: &Path,
) -> Result<(gst::Pipeline, BuiltOutput), RecordingError> {
    let owned = precreate_output(output_path)?;
    let token = BuiltOutput { path: output_path.to_owned(), owned };

    match build_inner(selection, output_path) {
        Ok(pipeline) => Ok((pipeline, token)),
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
) -> Result<gst::Pipeline, RecordingError> {
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

    // gst::parse::launch tokenises by whitespace and uses backslash for
    // escaping. The `target-object` value is also covered by the
    // strict `[A-Za-z0-9._:-]+` validation in `audio::devices`, so the
    // double-defence keeps any future caller from injecting elements.
    //
    // M2 ships mic + sink monitor mono mix only. Mic-only is
    // rejected upstream (`devices::resolve` returns
    // `InvalidArgument` for empty `monitor_arg`; profile validation
    // rejects empty `system_output`); the M3 pipeline split adds
    // a real mic-only branch alongside the rate parameterisation.
    let description = format!(
        "pipewiresrc target-object=\"{escaped_mic}\" ! audioconvert ! audioresample ! mix. \
         pipewiresrc target-object=\"{escaped_monitor}\" ! audioconvert ! audioresample ! mix. \
         audiomixer name=mix ! audioconvert ! audioresample ! \
         audio/x-raw,format=S16LE,rate=16000,channels=1 ! \
         flacenc ! filesink location=\"{escaped_output}\""
    );
    debug!(%description, "constructed gstreamer pipeline description");

    let element = gst::parse::launch(&description).map_err(|e| RecordingError::PipelineFailed {
        stage: "parse_launch".into(),
        source: Box::new(e),
    })?;

    element
        .downcast::<gst::Pipeline>()
        .map_err(|_| RecordingError::PipelineFailed {
            stage: "downcast_pipeline".into(),
            source: "parse::launch did not return a gst::Pipeline".into(),
        })
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

    #[test]
    fn escape_is_identity_for_plain_node_names() {
        assert_eq!(
            escape_for_parse_launch("alsa_input.usb-Generic_PHL-00.analog-stereo"),
            "alsa_input.usb-Generic_PHL-00.analog-stereo"
        );
    }

    #[test]
    fn escape_handles_quotes_and_backslash() {
        assert_eq!(
            escape_for_parse_launch(r#"a"b\c"#),
            r#"a\"b\\c"#
        );
    }

    #[test]
    fn precreate_output_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phase3.flac");
        let owned = precreate_output(&path).unwrap();
        assert!(owned, "precreate must report ownership for the file it just created");
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
