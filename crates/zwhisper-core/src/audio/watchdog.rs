//! Bus message classifier.
//!
//! Translates `gst::Message`s into a domain `Classification` enum so
//! the recorder/watchdog logic can stay free of `GStreamer` types.
//! The recorder owns the EOS finalisation; the classifier is
//! read-only.
//!
//! `pipewiresrc`-specific signals follow what the upstream plugin
//! emits today:
//!
//! - underrun → `Warning` whose source path contains `pipewiresrc`
//! - device gone → `Error` from a `pipewiresrc` whose payload
//!   contains `target not found`, `Stream error`, or
//!   `Connection lost`; **or** an `Element` message whose
//!   structure name is `node-removed`.
//!
//! These are runtime-locked-in heuristics — adjust if a future
//! `gst-plugin-pipewire` release renames them.

use gstreamer as gst;
use gstreamer::prelude::*;

use super::state::StopReason;

const PIPEWIRESRC_NEEDLE: &str = "pipewiresrc";
/// Substrings that a `pipewiresrc` Error payload carries when the
/// underlying `PipeWire` node is gone (USB unplug, profile switch,
/// session manager change). All entries are lowercase so the matcher
/// can compare against a `to_lowercase()`d combined message; this
/// keeps the check robust against capitalisation drift between
/// `gst-plugin-pipewire` releases.
const DEVICE_LOST_NEEDLES: &[&str] = &[
    "target not found",
    "stream error",
    "connection lost",
    "stream disconnected",
];
/// Substrings that mark a `pipewiresrc` warning as an actual buffer
/// underrun rather than a benign diagnostic (format negotiation
/// fallback, clock drift notice, etc.). If none match we fall through
/// to the generic `Warning` branch so the underrun counter does not
/// pick up unrelated noise. Lowercase for the same reason as
/// `DEVICE_LOST_NEEDLES`.
const UNDERRUN_NEEDLES: &[&str] = &["underrun", "xrun", "buffer underflow"];

/// Outcome of classifying a single bus message.
#[derive(Debug, Clone)]
pub(crate) enum Classification {
    /// `pipewiresrc` reported an underrun — increment the counter; not
    /// stop-worthy on its own.
    Underrun { source: String },
    /// Stop the recording immediately with the given reason.
    Stop(StopReason),
    /// Diagnostic-only — caller should log at warn level.
    Warning { source: String, message: String },
    /// Nothing to do.
    Ignore,
}

pub(crate) fn classify(message: &gst::Message) -> Classification {
    use gst::MessageView;

    let source = message
        .src()
        .map_or_else(|| "<unknown>".to_owned(), |s| s.path_string().to_string());

    match message.view() {
        MessageView::Eos(_) => Classification::Stop(StopReason::EosObserved),

        MessageView::Error(err) => {
            let payload = err.error().to_string();
            let debug = err.debug().map(|s| s.to_string()).unwrap_or_default();
            let combined = format!("{payload} {debug}").to_lowercase();

            if is_pipewiresrc(&source) && contains_any(&combined, DEVICE_LOST_NEEDLES) {
                return Classification::Stop(StopReason::DeviceLost {
                    node: extract_node_hint(&source).unwrap_or_else(|| source.clone()),
                });
            }

            Classification::Stop(StopReason::BusError {
                stage: source,
                message: payload,
            })
        }

        MessageView::Warning(warn) => {
            let payload = warn.error().to_string();
            let debug = warn.debug().map(|s| s.to_string()).unwrap_or_default();
            let combined = format!("{payload} {debug}").to_lowercase();
            if is_pipewiresrc(&source) && contains_any(&combined, UNDERRUN_NEEDLES) {
                Classification::Underrun { source }
            } else {
                Classification::Warning {
                    source,
                    message: payload,
                }
            }
        }

        MessageView::Element(el) => {
            // `pipewiresrc` emits a custom `node-removed` Element
            // message when the upstream PipeWire node disappears.
            // Treat any structure with that exact name as a hot-swap
            // signal regardless of which element produced it.
            if let Some(structure) = el.structure() {
                if structure.name() == "node-removed" {
                    let node = structure
                        .get::<&str>("node-name")
                        .ok()
                        .map(str::to_owned)
                        .or_else(|| extract_node_hint(&source))
                        .unwrap_or_else(|| source.clone());
                    return Classification::Stop(StopReason::DeviceLost { node });
                }
            }
            Classification::Ignore
        }

        // `StateChanged` to Null while we are still recording could
        // also signal a lost source, but the canonical path is
        // Error/Element above. Keep this branch silent until Phase 4
        // soak shows it is needed in practice.
        _ => Classification::Ignore,
    }
}

fn is_pipewiresrc(source: &str) -> bool {
    source.contains(PIPEWIRESRC_NEEDLE)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Extract a node-ish hint from an element path string like
/// `/GstPipeline:pipeline0/GstPipeWireSrc:pipewiresrc1`. We use the
/// last `/`-segment so the user gets *something* legible even when no
/// `node-name` is attached.
fn extract_node_hint(source: &str) -> Option<String> {
    source.rsplit('/').next().map(str::to_owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn ensure_init() {
        let _ = gst::init();
    }

    #[test]
    fn eos_classifies_as_stop_eos_observed() {
        ensure_init();
        let msg = gst::message::Eos::new();
        match classify(&msg) {
            Classification::Stop(StopReason::EosObserved) => {}
            other => panic!("unexpected classification: {other:?}"),
        }
    }

    #[test]
    fn error_classifies_as_stop_bus_error() {
        ensure_init();
        let msg = gst::message::Error::builder(gst::CoreError::Failed, "synthetic")
            .debug("test")
            .build();
        match classify(&msg) {
            Classification::Stop(StopReason::BusError { message, .. }) => {
                assert!(message.contains("synthetic") || !message.is_empty());
            }
            other => panic!("unexpected classification: {other:?}"),
        }
    }

    #[test]
    fn warning_classifies_as_warning() {
        ensure_init();
        let msg = gst::message::Warning::builder(gst::CoreError::Failed, "synthetic-warn").build();
        match classify(&msg) {
            Classification::Warning { .. } => {}
            other => panic!("unexpected classification: {other:?}"),
        }
    }

    #[test]
    fn unrelated_message_is_ignored() {
        ensure_init();
        let msg = gst::message::StreamStart::builder().build();
        match classify(&msg) {
            Classification::Ignore => {}
            other => panic!("unexpected classification: {other:?}"),
        }
    }

    #[test]
    fn helper_recognises_pipewiresrc_in_path() {
        assert!(is_pipewiresrc(
            "/GstPipeline:pipeline0/GstPipeWireSrc:pipewiresrc0"
        ));
        assert!(!is_pipewiresrc("/GstPipeline:pipeline0/GstAudioMixer:mix"));
    }

    #[test]
    fn extract_node_hint_returns_last_segment() {
        let hint = extract_node_hint("/GstPipeline:pipeline0/GstPipeWireSrc:pipewiresrc0");
        assert_eq!(hint.as_deref(), Some("GstPipeWireSrc:pipewiresrc0"));
    }

    #[test]
    fn contains_any_matches_first_needle() {
        assert!(contains_any(
            "stream error: target not found",
            DEVICE_LOST_NEEDLES
        ));
        assert!(!contains_any("all good", DEVICE_LOST_NEEDLES));
    }

    #[test]
    fn device_lost_needle_match_is_case_insensitive() {
        // Mirrors the `to_lowercase()` step in `classify` for the
        // Error branch — verifies no needle is accidentally written
        // with mixed case.
        let combined = "STREAM ERROR: TARGET NOT FOUND".to_lowercase();
        assert!(contains_any(&combined, DEVICE_LOST_NEEDLES));
    }

    #[test]
    fn element_message_with_node_removed_classifies_as_device_lost() {
        ensure_init();
        let bin = gst::Bin::builder().name("test-bin").build();
        let structure = gst::Structure::builder("node-removed")
            .field("node-name", "alsa_input.usb-Foo-00.analog-stereo")
            .build();
        let msg = gst::message::Element::builder(structure).src(&bin).build();
        match classify(&msg) {
            Classification::Stop(StopReason::DeviceLost { node }) => {
                assert_eq!(node, "alsa_input.usb-Foo-00.analog-stereo");
            }
            other => panic!("unexpected classification: {other:?}"),
        }
    }

    #[test]
    fn element_message_with_other_structure_is_ignored() {
        ensure_init();
        let bin = gst::Bin::builder().name("test-bin").build();
        let structure = gst::Structure::builder("some-other").build();
        let msg = gst::message::Element::builder(structure).src(&bin).build();
        match classify(&msg) {
            Classification::Ignore => {}
            other => panic!("unexpected classification: {other:?}"),
        }
    }
}
