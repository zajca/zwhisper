//! Actionable failure diagnosis (RFC-actionable-errors).
//!
//! zwhisper's typed errors already say precisely what went wrong. What
//! they do not say is **what to do about it**, and nothing downstream
//! can tell one failure from another without parsing a `Display` string.
//! This module closes both gaps:
//!
//! - [`crate::diagnostics::FailureCode`] is a stable, machine-readable
//!   vocabulary. Its wire
//!   strings are the contract; a consumer branches on them.
//! - [`crate::diagnostics::FailureReason`] pairs a code with a human
//!   message naming the
//!   concrete device / model / backend involved, and an **action** —
//!   a command the user can run or a setting they can change.
//!
//! The `From` impls over [`crate::audio::error::RecordingError`] and
//! [`crate::transcribe::TranscribeError`] are exhaustive matches, so a
//! new error variant fails to compile here rather than quietly falling
//! into a catch-all and losing its action.
//!
//! ## The action rule
//!
//! An action is never advice. "Check your microphone" is not an action;
//! `` `wpctl set-mute 52 0` `` is. Every constructor in this module is
//! covered by a test asserting the action names a runnable command, a
//! concrete path, or a settings key.

pub mod config;
pub mod levels;

pub use config::DiagnosticsConfig;
pub use levels::{LevelSummary, diagnose_levels};

use std::fmt;

/// Stable, machine-readable failure vocabulary.
///
/// The wire string produced by [`Self::as_str`] is the part consumers
/// may depend on; [`FailureReason::message`] and
/// [`FailureReason::action`] are human text and may be reworded at any
/// time. `from_wire` is the exact inverse of `as_str`, and the pairing is
/// covered by a round-trip test over [`FailureCode::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FailureCode {
    /// The capture source was muted when the recording was requested.
    MicMuted,
    /// The input saturated for a meaningful part of the recording.
    MicClipping,
    /// The recording contained no audible signal at all.
    MicSilent,
    /// The capture device vanished mid-recording.
    DeviceLost,
    /// The backend produced no text although the levels looked healthy.
    EmptyTranscript,
    /// The configured model is not installed (or is incomplete).
    ModelMissing,
    /// The backend exists but its engine was not compiled into this
    /// build.
    BackendNotCompiled,
    /// The backend is a recognised id with no implementation.
    BackendUnsupported,
    /// A local backend's executable could not be found or run.
    BackendUnavailable,
    /// A cloud backend rejected the API key.
    CloudAuth,
    /// No API key could be resolved for a cloud backend.
    CloudKeyMissing,
    /// A cloud backend reported quota exhaustion or rate limiting.
    CloudQuota,
    /// A cloud backend was unreachable, or the request timed out.
    CloudNetwork,
    /// Any other capture-side failure.
    RecordingFailed,
    /// Any other transcription-side failure.
    TranscribeFailed,
    /// The job was cancelled by the user.
    JobCancelled,
    /// The daemon stopped while the transcription was running.
    Interrupted,
}

impl FailureCode {
    /// Every variant, in declaration order. The round-trip and
    /// action-quality tests iterate this, so a variant added without a
    /// wire string or a summary fails the test run.
    pub const ALL: &'static [Self] = &[
        Self::MicMuted,
        Self::MicClipping,
        Self::MicSilent,
        Self::DeviceLost,
        Self::EmptyTranscript,
        Self::ModelMissing,
        Self::BackendNotCompiled,
        Self::BackendUnsupported,
        Self::BackendUnavailable,
        Self::CloudAuth,
        Self::CloudKeyMissing,
        Self::CloudQuota,
        Self::CloudNetwork,
        Self::RecordingFailed,
        Self::TranscribeFailed,
        Self::JobCancelled,
        Self::Interrupted,
    ];

    /// The wire string. **This is the stable contract** — changing one
    /// of these breaks every consumer branching on it, including user
    /// Waybar CSS keyed on the class name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MicMuted => "mic_muted",
            Self::MicClipping => "mic_clipping",
            Self::MicSilent => "mic_silent",
            Self::DeviceLost => "device_lost",
            Self::EmptyTranscript => "empty_transcript",
            Self::ModelMissing => "model_missing",
            Self::BackendNotCompiled => "backend_not_compiled",
            Self::BackendUnsupported => "backend_unsupported",
            Self::BackendUnavailable => "backend_unavailable",
            Self::CloudAuth => "cloud_auth",
            Self::CloudKeyMissing => "cloud_key_missing",
            Self::CloudQuota => "cloud_quota",
            Self::CloudNetwork => "cloud_network",
            Self::RecordingFailed => "recording_failed",
            Self::TranscribeFailed => "transcribe_failed",
            Self::JobCancelled => "job_cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    /// Inverse of [`Self::as_str`]. Returns `None` for an unknown code
    /// so a consumer reading a newer daemon's vocabulary degrades to
    /// "some failure" instead of guessing a wrong one.
    #[must_use]
    pub fn from_wire(code: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == code)
    }

    /// Short title for a desktop notification. Distinct per failure
    /// class so the user can tell a muted mic from a missing model
    /// without reading the body.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::MicMuted => "Microphone is muted",
            Self::MicClipping => "Input is clipping",
            Self::MicSilent => "Nothing was recorded",
            Self::DeviceLost => "Recording device disappeared",
            Self::EmptyTranscript => "No speech recognised",
            Self::ModelMissing => "Model not installed",
            Self::BackendNotCompiled | Self::BackendUnsupported | Self::BackendUnavailable => {
                "Backend unavailable"
            }
            Self::CloudAuth | Self::CloudKeyMissing => "Cloud authentication failed",
            Self::CloudQuota => "Cloud quota exhausted",
            Self::CloudNetwork => "Cloud backend unreachable",
            Self::RecordingFailed => "Recording failed",
            Self::TranscribeFailed => "Transcription failed",
            Self::JobCancelled => "Transcription cancelled",
            Self::Interrupted => "Transcription interrupted",
        }
    }

    /// Whether the failure needs the user to act before the next
    /// recording will work. Drives notification urgency: a muted mic or
    /// a missing model will fail again identically until it is fixed,
    /// whereas a cancelled job or a network blip will not.
    #[must_use]
    pub const fn is_actionable_now(self) -> bool {
        match self {
            Self::MicMuted
            | Self::MicClipping
            | Self::MicSilent
            | Self::ModelMissing
            | Self::BackendNotCompiled
            | Self::BackendUnsupported
            | Self::BackendUnavailable
            | Self::CloudAuth
            | Self::CloudKeyMissing => true,
            Self::DeviceLost
            | Self::EmptyTranscript
            | Self::CloudQuota
            | Self::CloudNetwork
            | Self::RecordingFailed
            | Self::TranscribeFailed
            | Self::JobCancelled
            | Self::Interrupted => false,
        }
    }
}

impl fmt::Display for FailureCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failure, as the user should hear about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureReason {
    /// The stable code consumers branch on.
    pub code: FailureCode,
    /// One sentence naming the concrete device, model, or backend
    /// involved. Lower-case, no trailing full stop — callers compose it
    /// into their own sentence (`"recording failed: {message}"`).
    pub message: String,
    /// A command the user can run or a setting they can change.
    pub action: String,
}

impl FailureReason {
    /// Build a reason from its three parts.
    #[must_use]
    pub fn new(code: FailureCode, message: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            action: action.into(),
        }
    }

    /// Reconstruct from the wire triple. An unrecognised code degrades
    /// to the matching catch-all rather than being dropped: the message
    /// and action are still worth showing, and a client talking to a
    /// newer daemon should not lose them.
    #[must_use]
    pub fn from_wire(code: &str, message: &str, action: &str) -> Self {
        Self::new(
            FailureCode::from_wire(code).unwrap_or(FailureCode::TranscribeFailed),
            message,
            action,
        )
    }

    /// Two lines for a terminal or a notification body: the message,
    /// then the action behind an arrow.
    #[must_use]
    pub fn render(&self) -> String {
        format!("{}\n  → {}", self.message, self.action)
    }

    /// The muted-microphone reason, built by the pre-capture probe.
    ///
    /// `description` is the human device name (`node.description`);
    /// `node` is the `PipeWire` `node.name`; `id` is the numeric node id
    /// `wpctl` takes. Naming all three is deliberate — the description
    /// is what the user recognises, the node name is what their profile
    /// contains, and the id is what the fix command needs.
    #[must_use]
    pub fn mic_muted(description: &str, node: &str, id: u32) -> Self {
        Self::new(
            FailureCode::MicMuted,
            format!("microphone `{description}` (`{node}`) is muted"),
            format!("unmute it: `wpctl set-mute {id} 0`"),
        )
    }

    /// The empty-transcript reason: the audio was fine, the recogniser
    /// simply returned nothing.
    #[must_use]
    pub fn empty_transcript(backend: &str, model: &str, language: &str) -> Self {
        Self::new(
            FailureCode::EmptyTranscript,
            format!(
                "`{backend}` produced no text from model `{model}` at language `{language}`, \
                 although the recorded level looked healthy"
            ),
            format!(
                "check `transcription.language` in the profile (currently `{language}`), \
                 or try a larger model from `zwhisper model list`"
            ),
        )
    }

    /// A job the user cancelled. Not really a failure, but it shares the
    /// terminal-state plumbing and the user still benefits from knowing
    /// the audio survived.
    #[must_use]
    pub fn job_cancelled(session_id: &str) -> Self {
        Self::new(
            FailureCode::JobCancelled,
            "transcription was cancelled; the recorded audio is kept".to_owned(),
            format!("re-run it with `zwhisper retry {session_id}`"),
        )
    }

    /// A transcription the daemon was running when it stopped. Startup
    /// recovery marks these `interrupted` and deliberately does not
    /// auto-retry (RFC-daemon-role F2.3).
    #[must_use]
    pub fn interrupted(session_id: &str) -> Self {
        Self::new(
            FailureCode::Interrupted,
            "the daemon stopped while this transcription was running; the audio is kept".to_owned(),
            format!("re-run it with `zwhisper retry {session_id}`"),
        )
    }
}

impl fmt::Display for FailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// Where a failure should point a user who has no better idea: the
/// daemon's own log. Used only by the catch-all arms, which by
/// definition have nothing more specific to offer.
const JOURNAL_ACTION: &str = "inspect the daemon log: `journalctl --user -u zwhisperd -n 200`";

#[cfg(feature = "audio")]
impl From<&crate::audio::error::RecordingError> for FailureReason {
    fn from(err: &crate::audio::error::RecordingError) -> Self {
        use crate::audio::error::RecordingError as E;
        match err {
            E::DeviceDisappeared { node } => Self::new(
                FailureCode::DeviceLost,
                format!("capture device `{node}` disappeared while recording"),
                "audio up to that point was kept; pick a stable device with \
                 `zwhisper audio devices`"
                    .to_owned(),
            ),
            E::DeviceDiscovery(source) => Self::new(
                FailureCode::RecordingFailed,
                format!("could not resolve the capture device: {source}"),
                "list what PipeWire actually offers with `zwhisper audio devices`".to_owned(),
            ),
            E::OutputPath { path, source } => Self::new(
                FailureCode::RecordingFailed,
                format!("could not open `{}` for writing: {source}", path.display()),
                format!(
                    "make sure `{}` exists and is writable",
                    path.parent().unwrap_or(path).display()
                ),
            ),
            E::EncoderFailed(_) | E::EosTimeout { .. } | E::PipelineFailed { .. } => Self::new(
                FailureCode::RecordingFailed,
                format!("{err}"),
                JOURNAL_ACTION.to_owned(),
            ),
        }
    }
}

#[cfg(feature = "transcribe")]
impl From<&crate::transcribe::TranscribeError> for FailureReason {
    #[allow(clippy::too_many_lines)]
    fn from(err: &crate::transcribe::TranscribeError) -> Self {
        use crate::transcribe::TranscribeError as E;
        match err {
            // ----- model resolution -----
            E::ModelNotFound { name, expected } => Self::new(
                FailureCode::ModelMissing,
                format!(
                    "model `{name}` is not installed at `{}`",
                    expected.display()
                ),
                format!("install it: `zwhisper model install {name}`"),
            ),
            E::ModelBundleIncomplete { id, dir, missing } => Self::new(
                FailureCode::ModelMissing,
                format!(
                    "model bundle `{id}` at `{}` is missing {} file(s): {}",
                    dir.display(),
                    missing.len(),
                    missing.join(", "),
                ),
                format!("re-install it: `zwhisper model install {id}`"),
            ),
            E::InvalidModelName { name, reason } => Self::new(
                FailureCode::ModelMissing,
                format!("model name `{name}` is not usable: {reason}"),
                "pick one from `zwhisper model list` and set it as \
                 `transcription.model` in the profile"
                    .to_owned(),
            ),
            E::InvalidModelDir {
                env_var,
                path,
                reason,
            } => Self::new(
                FailureCode::ModelMissing,
                format!(
                    "`{env_var}` points at `{}`, which is not usable: {reason}",
                    path.display()
                ),
                format!("set `{env_var}` to an absolute directory, or unset it to use the default"),
            ),
            E::InvalidModelSpec { id, reason } => Self::new(
                FailureCode::ModelMissing,
                format!("model spec `{id}` is invalid: {reason}"),
                "pick a model from `zwhisper model list`".to_owned(),
            ),
            E::ModelResolution(detail) => Self::new(
                FailureCode::ModelMissing,
                format!("could not resolve the models directory: {detail}"),
                "set `ZWHISPER_MODELS_DIR` to an absolute directory".to_owned(),
            ),

            // ----- backend availability -----
            E::BackendNotCompiled { backend, feature } => Self::new(
                FailureCode::BackendNotCompiled,
                format!("the `{backend}` backend is not compiled into this build"),
                format!(
                    "rebuild with it: \
                     `cargo build --release --features {feature} -p zwhisper-cli -p zwhisperd`, \
                     or pick another backend from `zwhisper backend list`"
                ),
            ),
            E::BackendUnsupported { backend } => Self::new(
                FailureCode::BackendUnsupported,
                format!("the `{backend}` backend has no implementation in this build"),
                "pick a backend from `zwhisper backend list`".to_owned(),
            ),
            E::BackendUnavailable { searched } => Self::new(
                FailureCode::BackendUnavailable,
                format!(
                    "no `whisper-cli` binary found; looked in {}",
                    render_paths(searched)
                ),
                "install whisper.cpp (AUR `whisper.cpp` on Arch), or point \
                 `ZWHISPER_WHISPER_CLI` at the binary"
                    .to_owned(),
            ),
            E::BackendSpawn { tool, source } => Self::new(
                FailureCode::BackendUnavailable,
                format!("could not run `{}`: {source}", tool.display()),
                format!(
                    "check that `{}` exists and is executable, or point \
                     `ZWHISPER_WHISPER_CLI` at a working binary",
                    tool.display()
                ),
            ),
            E::BackendUnknown { name, supported } => Self::new(
                FailureCode::BackendUnsupported,
                format!(
                    "`{name}` is not a known backend; supported: {}",
                    supported.join(", ")
                ),
                "set `transcription.backend` in the profile to one of those".to_owned(),
            ),

            // ----- cloud -----
            E::BackendKeyMissing { backend, source } => Self::new(
                FailureCode::CloudKeyMissing,
                format!("no API key for `{backend}`: {source}"),
                format!(
                    "check `zwhisper backend health --backend {backend}` for where the key \
                         is looked up"
                ),
            ),
            E::BackendAuth {
                backend,
                status,
                key_source,
            } => Self::new(
                FailureCode::CloudAuth,
                format!("`{backend}` rejected the API key from {key_source} (HTTP {status})"),
                format!(
                    "rotate the key, then verify with \
                     `zwhisper backend health --backend {backend}`"
                ),
            ),
            E::BackendQuota {
                backend,
                status,
                retry_after_s,
                ..
            } => Self::new(
                FailureCode::CloudQuota,
                match retry_after_s {
                    Some(s) => format!(
                        "`{backend}` is rate-limited or out of credit (HTTP {status}); \
                         it asked to retry in {s}s"
                    ),
                    None => format!("`{backend}` is rate-limited or out of credit (HTTP {status})"),
                },
                "the audio is kept — re-run it later with `zwhisper retry <session>`".to_owned(),
            ),
            E::BackendNetwork { backend, .. } | E::BackendTimeout { backend, .. } => Self::new(
                FailureCode::CloudNetwork,
                format!("`{backend}` was unreachable: {err}"),
                format!(
                    "check connectivity, then `zwhisper backend health --backend {backend}`; \
                     the audio is kept for `zwhisper retry <session>`"
                ),
            ),
            E::BackendConfig { backend, message } => Self::new(
                FailureCode::TranscribeFailed,
                format!("`{backend}` is misconfigured: {message}"),
                format!(
                    "fix `[transcription.{}]` in the profile",
                    backend.replace('-', "_")
                ),
            ),

            // ----- everything that is genuinely "look at the log" -----
            E::AsrRateMismatch { .. }
            | E::ArtifactWrite { .. }
            | E::AudioDecode { .. }
            | E::BackendBadResponse { .. }
            | E::BackendExitedNonZero { .. }
            | E::BackendJsonShape { .. }
            | E::InputAudio { .. }
            | E::InvalidBackendOption { .. }
            | E::JsonShape { .. }
            | E::OutputMissing { .. }
            | E::OutputUnreadable { .. }
            | E::PcmSource(_)
            | E::UnsupportedAudioInput { .. }
            | E::UnsupportedModelKind { .. } => Self::new(
                FailureCode::TranscribeFailed,
                format!("{err}"),
                JOURNAL_ACTION.to_owned(),
            ),
        }
    }
}

/// Render a search-path list the way an error message reads best.
#[cfg(feature = "transcribe")]
fn render_paths(paths: &[std::path::PathBuf]) -> String {
    if paths.is_empty() {
        return "no candidate locations".to_owned();
    }
    paths
        .iter()
        .map(|p| format!("`{}`", p.display()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn every_code_round_trips_through_its_wire_string() {
        for &code in FailureCode::ALL {
            let wire = code.as_str();
            assert_eq!(
                FailureCode::from_wire(wire),
                Some(code),
                "`{wire}` did not round-trip",
            );
        }
    }

    #[test]
    fn wire_strings_are_unique_and_snake_case() {
        let mut seen = std::collections::HashSet::new();
        for &code in FailureCode::ALL {
            let wire = code.as_str();
            assert!(seen.insert(wire), "duplicate wire string `{wire}`");
            assert!(
                wire.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                "`{wire}` is not snake_case — it is also a Waybar CSS class",
            );
        }
    }

    #[test]
    fn all_slice_covers_every_variant() {
        // `ALL` is hand-maintained; this is the guard that a variant
        // added to the enum was added here too. The literal is the
        // deliberate tripwire — bump it in the same commit as the
        // variant.
        assert_eq!(FailureCode::ALL.len(), 17);
    }

    #[test]
    fn unknown_wire_string_is_rejected_rather_than_guessed() {
        assert_eq!(FailureCode::from_wire("mic_on_fire"), None);
        assert_eq!(FailureCode::from_wire(""), None);
        assert_eq!(FailureCode::from_wire("MIC_MUTED"), None);
    }

    #[test]
    fn every_code_has_a_distinct_enough_summary() {
        for &code in FailureCode::ALL {
            let summary = code.summary();
            assert!(!summary.is_empty(), "{code} has no summary");
            assert!(
                summary.len() <= 40,
                "{code} summary is too long for a notification title: {summary}",
            );
        }
    }

    /// An action must name something the user can *do*: a backtick-quoted
    /// command, a path, or a settings key. This is the rule the whole
    /// module exists to enforce, so it is asserted over every
    /// constructor rather than spot-checked.
    fn assert_actionable(reason: &FailureReason) {
        assert!(
            !reason.action.is_empty(),
            "{} has an empty action",
            reason.code
        );
        assert!(
            reason.action.contains('`') || reason.action.contains('/'),
            "{} action names no command, path or key: {}",
            reason.code,
            reason.action,
        );
        assert!(
            !reason.message.is_empty(),
            "{} has an empty message",
            reason.code
        );
    }

    #[test]
    fn hand_written_constructors_are_actionable() {
        assert_actionable(&FailureReason::mic_muted(
            "Built-in Audio",
            "alsa_input.pci",
            52,
        ));
        assert_actionable(&FailureReason::empty_transcript("parakeet", "v3", "auto"));
        assert_actionable(&FailureReason::job_cancelled("abc-123"));
        assert_actionable(&FailureReason::interrupted("abc-123"));
    }

    #[test]
    fn mic_muted_names_the_device_and_the_exact_unmute_command() {
        let r = FailureReason::mic_muted("Built-in Audio Analog Stereo", "alsa_input.pci-x", 52);
        assert_eq!(r.code, FailureCode::MicMuted);
        assert!(r.message.contains("Built-in Audio Analog Stereo"));
        assert!(r.message.contains("alsa_input.pci-x"));
        assert!(r.action.contains("wpctl set-mute 52 0"));
    }

    #[test]
    fn render_puts_the_action_on_its_own_line() {
        let r = FailureReason::new(
            FailureCode::MicMuted,
            "mic is muted",
            "`wpctl set-mute 1 0`",
        );
        assert_eq!(r.render(), "mic is muted\n  → `wpctl set-mute 1 0`");
    }

    #[test]
    fn from_wire_preserves_text_for_an_unknown_code() {
        // A client talking to a newer daemon must not throw away a
        // perfectly good message just because the code is new.
        let r = FailureReason::from_wire("mic_on_fire", "the mic is on fire", "`use water`");
        assert_eq!(r.code, FailureCode::TranscribeFailed);
        assert_eq!(r.message, "the mic is on fire");
        assert_eq!(r.action, "`use water`");
    }

    #[test]
    fn from_wire_round_trips_a_known_code() {
        for &code in FailureCode::ALL {
            let r = FailureReason::from_wire(code.as_str(), "m", "a");
            assert_eq!(r.code, code);
        }
    }

    #[test]
    fn only_failures_the_user_can_fix_now_are_flagged_actionable() {
        assert!(FailureCode::MicMuted.is_actionable_now());
        assert!(FailureCode::ModelMissing.is_actionable_now());
        assert!(!FailureCode::JobCancelled.is_actionable_now());
        assert!(!FailureCode::CloudNetwork.is_actionable_now());
    }

    #[cfg(feature = "audio")]
    mod recording {
        use super::*;
        use crate::audio::error::RecordingError;

        #[test]
        fn device_disappeared_maps_to_device_lost_and_names_the_node() {
            let err = RecordingError::DeviceDisappeared {
                node: "alsa_input.usb-Blue_Yeti".to_owned(),
            };
            let r = FailureReason::from(&err);
            assert_eq!(r.code, FailureCode::DeviceLost);
            assert!(r.message.contains("alsa_input.usb-Blue_Yeti"));
            assert_actionable(&r);
        }

        #[test]
        fn output_path_failure_points_at_the_parent_directory() {
            let err = RecordingError::OutputPath {
                path: std::path::PathBuf::from("/nope/deeper/out.flac"),
                source: std::io::Error::other("boom"),
            };
            let r = FailureReason::from(&err);
            assert_eq!(r.code, FailureCode::RecordingFailed);
            assert!(r.action.contains("/nope/deeper"));
            assert_actionable(&r);
        }

        #[test]
        fn encoder_failure_falls_back_to_the_log_without_losing_the_detail() {
            let err = RecordingError::EncoderFailed("flacenc exploded".to_owned());
            let r = FailureReason::from(&err);
            assert_eq!(r.code, FailureCode::RecordingFailed);
            assert!(r.message.contains("flacenc exploded"));
            assert_actionable(&r);
        }
    }

    #[cfg(feature = "transcribe")]
    mod transcribe {
        use super::*;
        use crate::transcribe::TranscribeError;
        use std::path::PathBuf;

        #[test]
        fn missing_single_file_model_prints_the_exact_install_command() {
            let err = TranscribeError::ModelNotFound {
                name: "large-v3-turbo-q5_0".to_owned(),
                expected: PathBuf::from("/home/u/.local/share/zwhisper/models/ggml-x.bin"),
            };
            let r = FailureReason::from(&err);
            assert_eq!(r.code, FailureCode::ModelMissing);
            assert!(
                r.action
                    .contains("zwhisper model install large-v3-turbo-q5_0"),
                "action was: {}",
                r.action,
            );
            assert_actionable(&r);
        }

        #[test]
        fn incomplete_bundle_prints_the_exact_install_command() {
            let err = TranscribeError::ModelBundleIncomplete {
                id: "parakeet-tdt-0.6b-v3-int8".to_owned(),
                dir: PathBuf::from("/models/parakeet"),
                missing: vec!["encoder.onnx".to_owned(), "vocab.txt".to_owned()],
            };
            let r = FailureReason::from(&err);
            assert_eq!(r.code, FailureCode::ModelMissing);
            assert!(r.message.contains("encoder.onnx"));
            assert!(
                r.action
                    .contains("zwhisper model install parakeet-tdt-0.6b-v3-int8")
            );
            assert_actionable(&r);
        }

        #[test]
        fn not_compiled_backend_names_the_cargo_feature() {
            let err = TranscribeError::BackendNotCompiled {
                backend: "parakeet",
                feature: "parakeet",
            };
            let r = FailureReason::from(&err);
            assert_eq!(r.code, FailureCode::BackendNotCompiled);
            assert!(r.action.contains("--features parakeet"));
            assert_actionable(&r);
        }

        #[test]
        fn cloud_auth_names_where_the_key_came_from_and_never_the_key() {
            let err = TranscribeError::BackendAuth {
                backend: "deepgram",
                status: 401,
                key_source: "`ZWHISPER_DEEPGRAM_API_KEY`".to_owned(),
            };
            let r = FailureReason::from(&err);
            assert_eq!(r.code, FailureCode::CloudAuth);
            assert!(
                r.message.contains("ZWHISPER_DEEPGRAM_API_KEY"),
                "message must name the source: {}",
                r.message,
            );
            assert!(r.message.contains("401"));
            assert_actionable(&r);
        }

        #[test]
        fn whisper_cli_missing_lists_where_it_looked() {
            let err = TranscribeError::BackendUnavailable {
                searched: vec![PathBuf::from("/usr/bin/whisper-cli")],
            };
            let r = FailureReason::from(&err);
            assert_eq!(r.code, FailureCode::BackendUnavailable);
            assert!(r.message.contains("/usr/bin/whisper-cli"));
            assert!(r.action.contains("ZWHISPER_WHISPER_CLI"));
            assert_actionable(&r);
        }

        #[test]
        fn quota_without_a_retry_hint_still_reads_cleanly() {
            let err = TranscribeError::BackendQuota {
                backend: "deepgram",
                status: 429,
                retry_after_s: None,
                message: "slow down".to_owned(),
            };
            let r = FailureReason::from(&err);
            assert_eq!(r.code, FailureCode::CloudQuota);
            assert!(!r.message.contains("retry in"));
            assert_actionable(&r);
        }

        #[test]
        fn catch_all_keeps_the_original_display_text() {
            let err = TranscribeError::OutputMissing {
                path: PathBuf::from("/tmp/x.txt"),
            };
            let r = FailureReason::from(&err);
            assert_eq!(r.code, FailureCode::TranscribeFailed);
            assert!(r.message.contains("/tmp/x.txt"));
            assert_actionable(&r);
        }
    }
}
