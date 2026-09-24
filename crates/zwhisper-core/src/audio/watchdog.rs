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

/// Structure name of the `level` element's periodic analysis message.
const LEVEL_STRUCTURE: &str = "level";

/// Structure name `pipewiresrc` uses to announce that its upstream node
/// went away.
const NODE_REMOVED_STRUCTURE: &str = "node-removed";

/// Outcome of classifying a single bus message.
#[derive(Debug, Clone)]
pub(crate) enum Classification {
    /// `pipewiresrc` reported an underrun — increment the counter; not
    /// stop-worthy on its own.
    Underrun { source: String },
    /// Stop the recording immediately with the given reason.
    Stop(StopReason),
    /// A `level` element reported one analysis window. `peak_db` and
    /// `rms_db` are dBFS (the element computes them; 0 dBFS is full
    /// scale) and `duration_ms` is the window length. Folded into the
    /// recording's `LevelSummary` — never stop-worthy.
    Level {
        peak_db: f32,
        rms_db: f32,
        duration_ms: u64,
    },
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
                if structure.name() == NODE_REMOVED_STRUCTURE {
                    let node = structure
                        .get::<&str>("node-name")
                        .ok()
                        .map(str::to_owned)
                        .or_else(|| extract_node_hint(&source))
                        .unwrap_or_else(|| source.clone());
                    return Classification::Stop(StopReason::DeviceLost { node });
                }
                if structure.name() == LEVEL_STRUCTURE {
                    if let Some(level) = parse_level(structure) {
                        return level;
                    }
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

/// Read one `level` element message into a [`Classification::Level`].
///
/// The element reports `peak` and `rms` as `GValueArray`s of `f64`
/// **already in dBFS**, one entry per channel, plus the window length in
/// nanoseconds.
///
/// Extraction is kept separate from the arithmetic in
/// [`level_from_parts`] because `glib::ValueArray` is not `Send`, so
/// gstreamer-rs refuses to build a `Structure` containing one — a
/// `level` structure can be *read* from Rust but not synthesised, and
/// the decision logic would otherwise be untestable without a live
/// pipeline.
fn parse_level(structure: &gst::StructureRef) -> Option<Classification> {
    let duration_ns = structure.get::<u64>("duration").ok()?;
    level_from_parts(
        &channel_values(structure, "peak"),
        &channel_values(structure, "rms"),
        duration_ns,
    )
}

/// Collect a `level` message's per-channel `GValueArray` into plain
/// `f64`s. A missing or non-array field yields an empty vec, which
/// [`level_from_parts`] rejects.
fn channel_values(structure: &gst::StructureRef, field: &str) -> Vec<f64> {
    structure
        .get::<gst::glib::ValueArray>(field)
        .map(|array| array.iter().filter_map(|v| v.get::<f64>().ok()).collect())
        .unwrap_or_default()
}

/// Turn one `level` window's per-channel dBFS readings into a
/// classification.
///
/// The capture graph is mono by the time it reaches the element, but the
/// **loudest** channel is taken so a future multi-channel graph degrades
/// to "the worst channel" rather than silently reporting only channel 0.
///
/// Returns `None` when the window is zero-length or no channel carries a
/// finite reading. A malformed message is dropped rather than folded in
/// as a zero, which would drag the session RMS toward silence and could
/// manufacture a false "nothing was heard" verdict.
fn level_from_parts(peaks: &[f64], rms: &[f64], duration_ns: u64) -> Option<Classification> {
    let duration_ms = duration_ns / 1_000_000;
    if duration_ms == 0 {
        return None;
    }
    Some(Classification::Level {
        peak_db: max_finite_db(peaks)?,
        rms_db: max_finite_db(rms)?,
        duration_ms,
    })
}

/// Largest finite reading in a per-channel dBFS slice.
fn max_finite_db(values: &[f64]) -> Option<f32> {
    values
        .iter()
        .map(|&db| db as f32)
        .filter(|db| db.is_finite())
        .fold(None, |acc: Option<f32>, db| {
            Some(acc.map_or(db, |best| if db > best { db } else { best }))
        })
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
    fn level_window_classifies_with_db_values_and_window_length() {
        match level_from_parts(&[-6.0211], &[-9.0315], 100_000_000) {
            Some(Classification::Level {
                peak_db,
                rms_db,
                duration_ms,
            }) => {
                assert!((peak_db - (-6.0211)).abs() < 1e-3, "{peak_db}");
                assert!((rms_db - (-9.0315)).abs() < 1e-3, "{rms_db}");
                assert_eq!(duration_ms, 100);
            }
            other => panic!("unexpected classification: {other:?}"),
        }
    }

    #[test]
    fn multi_channel_level_window_reports_the_loudest_channel() {
        // The capture graph is mono today, but a future multi-channel
        // shape must degrade to "the worst channel", not "channel 0".
        match level_from_parts(&[-30.0, -2.0], &[-40.0, -11.0], 100_000_000) {
            Some(Classification::Level {
                peak_db, rms_db, ..
            }) => {
                assert!((peak_db - (-2.0)).abs() < 1e-3, "{peak_db}");
                assert!((rms_db - (-11.0)).abs() < 1e-3, "{rms_db}");
            }
            other => panic!("unexpected classification: {other:?}"),
        }
    }

    #[test]
    fn malformed_level_window_is_dropped_rather_than_folded_as_zero() {
        // No channels at all.
        assert!(level_from_parts(&[], &[], 100_000_000).is_none());
        // Peak present, RMS missing.
        assert!(level_from_parts(&[-6.0], &[], 100_000_000).is_none());
        // Sub-millisecond window: nothing to weight it by.
        assert!(level_from_parts(&[-6.0], &[-9.0], 0).is_none());
        assert!(level_from_parts(&[-6.0], &[-9.0], 999_999).is_none());
    }

    #[test]
    fn non_finite_level_channels_are_skipped() {
        match level_from_parts(&[f64::NEG_INFINITY, -12.0], &[f64::NAN, -20.0], 100_000_000) {
            Some(Classification::Level {
                peak_db, rms_db, ..
            }) => {
                assert!((peak_db - (-12.0)).abs() < 1e-3, "{peak_db}");
                assert!((rms_db - (-20.0)).abs() < 1e-3, "{rms_db}");
            }
            other => panic!("unexpected classification: {other:?}"),
        }
    }

    #[test]
    fn a_window_of_only_non_finite_channels_is_dropped() {
        assert!(level_from_parts(&[f64::NAN], &[f64::NAN], 100_000_000).is_none());
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
