//! Pre-capture checks (RFC-actionable-errors § F6).
//!
//! Three things are worth knowing before a single sample is captured: a
//! backend that is not compiled in, a model that is not installed, and a
//! muted microphone. All three were previously discovered *after* the
//! user had finished speaking — the first two at transcribe time, the
//! third never.
//!
//! [`run`] checks them at `Recorder1.StartRecording`, in ascending order
//! of cost.
//!
//! ## Only a muted microphone refuses the recording
//!
//! The outcome separates a **refusal** from an **advisory**, and the
//! split is deliberate:
//!
//! - A muted microphone makes the capture worthless by construction:
//!   there is nothing to keep, so refusing costs the user nothing and
//!   saves them a wasted sentence.
//! - A missing model or an uncompiled backend only breaks the
//!   *transcription*. The audio is still real, irreplaceable, and
//!   transcribable later with `zwhisper transcribe <file>` once the model
//!   is installed. Refusing would throw away the one thing that cannot
//!   be recreated, so the problem is reported and the recording proceeds.
//!
//! Both checks are skipped entirely when `transcription.auto` is off: a
//! record-only profile promises no transcript, so a missing model is not
//! a problem it has.
//!
//! ## The probe must never cost a recording either
//!
//! The mute probe shells out to `pw-dump` and `wpctl`, and it is the one
//! place in this module that can refuse. It therefore refuses **only** on
//! a definitive `muted == true`. A missing tool, an unparseable dump, a
//! node that cannot be resolved, or a probe that overruns its budget are
//! all logged at debug and the recording proceeds.

use std::time::Duration;

use tracing::{debug, warn};
use zwhisper_core::diagnostics::{DiagnosticsConfig, FailureReason};
use zwhisper_core::profile::Profile;
use zwhisper_core::setup::{AudioDevice, PipewireControl, SystemPipewire, build_devices};
use zwhisper_core::transcribe::{BackendSettings, TranscribeOpts};

/// The failure-detection config for a profile: its `[diagnostics]`
/// table, or the conservative defaults when it has none.
pub(crate) fn diagnostics_config(profile: &Profile) -> DiagnosticsConfig {
    profile
        .diagnostics
        .as_ref()
        .map_or_else(DiagnosticsConfig::default, |d| d.to_config())
}

/// What the pre-capture checks found.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    /// A reason to refuse the recording outright — only ever a muted
    /// microphone, whose capture would be worthless.
    pub(crate) refusal: Option<FailureReason>,
    /// A reason the *transcription* will fail. Reported so the user can
    /// fix it now (possibly even mid-recording), but never a reason to
    /// discard the audio.
    pub(crate) advisory: Option<FailureReason>,
}

/// Run every pre-capture check for `profile`.
pub(crate) async fn run(profile: &Profile, cfg: &DiagnosticsConfig) -> Outcome {
    let mut outcome = Outcome::default();

    // 1 + 2: backend compiled in, model resolvable. Pure and cheap, and
    // they share the coordinator's own resolution path so a check that
    // passes here cannot fail differently at transcribe time. Only
    // meaningful when a transcript was actually promised.
    if profile.transcription.auto {
        let opts = TranscribeOpts {
            backend: profile.transcription.backend,
            model: profile.transcription.model.clone(),
            language: profile.transcription.language.clone(),
            settings: BackendSettings {
                whisper_cpp: profile.transcription.whisper_cpp.clone(),
                deepgram: profile.transcription.deepgram.clone(),
            },
        };
        if let Err(e) = zwhisper_core::transcribe::preflight(&opts) {
            outcome.advisory = Some(FailureReason::from(&e));
        }
    }

    // 3: the mute flag. Opt-out per profile.
    if cfg.mute_probe {
        outcome.refusal = probe_mute(profile.sources.mic.clone(), cfg).await;
    }

    outcome
}

/// Probe the target source's mute flag.
///
/// Returns `Some(reason)` **only** for a definitive mute. Every other
/// outcome — including every error — is `None`, because an inconclusive
/// probe must not block a recording.
///
/// The `pw-dump` / `wpctl` calls are blocking `Command`s, so they run on
/// the blocking pool under a hard timeout; a hung child cannot stall the
/// D-Bus method call.
async fn probe_mute(mic: String, cfg: &DiagnosticsConfig) -> Option<FailureReason> {
    let budget = Duration::from_millis(cfg.mute_probe_timeout_ms);
    let probe =
        tokio::task::spawn_blocking(move || probe_mute_blocking(&SystemPipewire::default(), &mic));

    match tokio::time::timeout(budget, probe).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(join_err)) => {
            warn!(error = %join_err, "mute probe panicked; starting the recording anyway");
            None
        }
        Err(_elapsed) => {
            // The blocking task keeps running and finishes on its own;
            // we simply stop waiting for it. Its result is discarded.
            debug!(
                timeout_ms = cfg.mute_probe_timeout_ms,
                "mute probe timed out; starting the recording anyway",
            );
            None
        }
    }
}

/// The synchronous body of the mute probe, split out so it can be tested
/// against a mock [`PipewireControl`] with no PipeWire running.
pub(crate) fn probe_mute_blocking(pw: &impl PipewireControl, mic: &str) -> Option<FailureReason> {
    let nodes = match pw.dump_nodes() {
        Ok(n) => n,
        Err(e) => {
            debug!(error = %e, "mute probe: could not enumerate PipeWire nodes");
            return None;
        }
    };
    // A missing default-source name is not fatal: it only costs the
    // `is_default` flag, which matters solely for a `mic = "default"`
    // profile — and that case is handled by returning `None` below.
    let default_name = pw.default_source_name().unwrap_or_default();
    let devices = build_devices(&nodes, &default_name);

    let Some(target) = select_target(&devices, mic) else {
        debug!(%mic, "mute probe: target source not found among PipeWire nodes");
        return None;
    };

    match pw.get_volume(target.id) {
        Ok(volume) if volume.muted => Some(FailureReason::mic_muted(
            &target.description,
            &target.node_name,
            target.id,
        )),
        Ok(_) => None,
        Err(e) => {
            debug!(error = %e, node = %target.node_name, "mute probe: could not read volume");
            None
        }
    }
}

/// Resolve a profile's `sources.mic` to a concrete device.
///
/// `"default"` picks the current default source; anything else is an
/// exact `node.name` match, the same string the capture pipeline hands
/// `pipewiresrc target-object=`. Monitor sources are never a mic, so
/// they are excluded — matching one would report the mute state of a
/// sink.
fn select_target<'a>(devices: &'a [AudioDevice], mic: &str) -> Option<&'a AudioDevice> {
    if mic == "default" {
        return devices
            .iter()
            .find(|d| d.is_source && !d.is_monitor && d.is_default);
    }
    devices
        .iter()
        .find(|d| d.is_source && !d.is_monitor && d.node_name == mic)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use zwhisper_core::setup::{RawNode, SetupError, Volume};

    /// Canned [`PipewireControl`] — no PipeWire, no subprocesses.
    #[derive(Debug)]
    struct MockPw {
        nodes: Result<Vec<RawNode>, &'static str>,
        default_source: Result<String, &'static str>,
        volume: Result<Volume, &'static str>,
    }

    impl Default for MockPw {
        fn default() -> Self {
            Self {
                nodes: Ok(vec![
                    raw_node(
                        52,
                        "alsa_input.pci-0000_00_1f.3",
                        "Built-in Mic",
                        "Audio/Source",
                    ),
                    raw_node(
                        60,
                        "alsa_output.pci-0000_00_1f.3.monitor",
                        "Speakers Monitor",
                        "Audio/Source",
                    ),
                ]),
                default_source: Ok("alsa_input.pci-0000_00_1f.3".to_owned()),
                volume: Ok(Volume {
                    linear: 0.3,
                    muted: false,
                }),
            }
        }
    }

    fn raw_node(id: u32, node_name: &str, description: &str, media_class: &str) -> RawNode {
        RawNode {
            id,
            node_name: node_name.to_owned(),
            description: description.to_owned(),
            media_class: media_class.to_owned(),
            serial: None,
        }
    }

    fn err(message: &'static str) -> SetupError {
        SetupError::CommandFailed {
            tool: "wpctl",
            message: message.to_owned(),
        }
    }

    impl PipewireControl for MockPw {
        fn dump_nodes(&self) -> Result<Vec<RawNode>, SetupError> {
            self.nodes.clone().map_err(err)
        }
        fn default_source_name(&self) -> Result<String, SetupError> {
            self.default_source.clone().map_err(err)
        }
        fn get_volume(&self, _id: u32) -> Result<Volume, SetupError> {
            self.volume.map_err(err)
        }
        fn set_volume(&self, _id: u32, _linear: f32) -> Result<(), SetupError> {
            unreachable!("the probe never mutates volume")
        }
        fn set_default(&self, _id: u32) -> Result<(), SetupError> {
            unreachable!("the probe never changes the default device")
        }
    }

    fn muted() -> MockPw {
        MockPw {
            volume: Ok(Volume {
                linear: 0.3,
                muted: true,
            }),
            ..MockPw::default()
        }
    }

    #[test]
    fn a_muted_explicit_node_is_reported_with_its_unmute_command() {
        let reason = probe_mute_blocking(&muted(), "alsa_input.pci-0000_00_1f.3").unwrap();
        assert_eq!(
            reason.code,
            zwhisper_core::diagnostics::FailureCode::MicMuted
        );
        assert!(reason.message.contains("Built-in Mic"));
        assert!(reason.message.contains("alsa_input.pci-0000_00_1f.3"));
        assert!(reason.action.contains("wpctl set-mute 52 0"));
    }

    #[test]
    fn a_muted_default_source_is_resolved_and_reported() {
        let reason = probe_mute_blocking(&muted(), "default").unwrap();
        assert!(reason.action.contains("wpctl set-mute 52 0"));
    }

    #[test]
    fn an_unmuted_source_does_not_block() {
        assert!(probe_mute_blocking(&MockPw::default(), "default").is_none());
    }

    // ---- every inconclusive outcome must let the recording through ----

    #[test]
    fn a_failing_dump_does_not_block() {
        let pw = MockPw {
            nodes: Err("pw-dump not found"),
            ..muted()
        };
        assert!(probe_mute_blocking(&pw, "default").is_none());
    }

    #[test]
    fn a_failing_volume_read_does_not_block() {
        let pw = MockPw {
            volume: Err("wpctl exited 1"),
            ..MockPw::default()
        };
        assert!(probe_mute_blocking(&pw, "default").is_none());
    }

    #[test]
    fn an_unresolvable_node_does_not_block() {
        assert!(probe_mute_blocking(&muted(), "alsa_input.does-not-exist").is_none());
    }

    #[test]
    fn a_missing_default_source_name_does_not_block() {
        // Without the default name nothing carries `is_default`, so a
        // `mic = "default"` profile cannot be resolved — and an
        // unresolvable target never blocks.
        let pw = MockPw {
            default_source: Err("wpctl inspect failed"),
            ..muted()
        };
        assert!(probe_mute_blocking(&pw, "default").is_none());
    }

    /// A profile whose only interesting property here is
    /// `transcription.auto` and a model id that cannot resolve.
    fn profile_with(auto: bool, model: &str) -> Profile {
        use zwhisper_core::profile::schema::{
            Backend, Codec, Mode, Recording, Sources, Transcription,
        };
        Profile {
            schema_version: 1,
            name: "t".to_owned(),
            description: String::new(),
            sources: Sources {
                mic: "default".to_owned(),
                system_output: String::new(),
                mode: Mode::MonoMix,
                input_gain_db: None,
            },
            recording: Recording {
                codec: Codec::Flac,
                sample_rate: 16_000,
                max_duration_minutes: 60,
            },
            transcription: Transcription {
                backend: Backend::WhisperCpp,
                model: model.to_owned(),
                language: "auto".to_owned(),
                auto,
                deepgram: None,
                whisper_cpp: None,
            },
            outputs: Vec::new(),
            hotkey: zwhisper_core::profile::schema::Hotkey::default(),
            diagnostics: None,
        }
    }

    /// The mute probe shells out to `pw-dump`/`wpctl`; switch it off so
    /// these tests exercise only the backend/model half of `run`.
    fn no_probe() -> DiagnosticsConfig {
        DiagnosticsConfig {
            mute_probe: false,
            ..DiagnosticsConfig::default()
        }
    }

    #[tokio::test]
    async fn a_missing_model_is_an_advisory_not_a_refusal() {
        // The audio is irreplaceable and still transcribable later with
        // `zwhisper transcribe <file>`; refusing would discard the one
        // thing that cannot be recreated.
        let outcome = run(
            &profile_with(true, "definitely-not-an-installed-model"),
            &no_probe(),
        )
        .await;
        assert!(
            outcome.refusal.is_none(),
            "a missing model must never block a recording",
        );
        let advisory = outcome.advisory.expect("advisory expected");
        assert_eq!(
            advisory.code,
            zwhisper_core::diagnostics::FailureCode::ModelMissing
        );
        assert!(
            advisory.action.contains("zwhisper model install"),
            "action was: {}",
            advisory.action,
        );
    }

    #[tokio::test]
    async fn a_record_only_profile_is_not_advised_about_models() {
        // `auto = false` promises no transcript, so a missing model is
        // not a problem this profile has.
        let outcome = run(
            &profile_with(false, "definitely-not-an-installed-model"),
            &no_probe(),
        )
        .await;
        assert!(outcome.refusal.is_none());
        assert!(
            outcome.advisory.is_none(),
            "a record-only profile must not be warned about transcription",
        );
    }

    #[test]
    fn a_monitor_source_is_never_treated_as_the_microphone() {
        // Matching a `.monitor` node would report a *sink's* mute state
        // and refuse a perfectly good recording.
        let pw = muted();
        assert!(
            probe_mute_blocking(&pw, "alsa_output.pci-0000_00_1f.3.monitor").is_none(),
            "a monitor must not resolve as the mic",
        );
    }
}
