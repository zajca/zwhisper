use std::path::PathBuf;

use clap::{Args, Subcommand};
use color_eyre::eyre::{bail, eyre};
use tracing::{info, warn};

use crate::audio::recorder::{RecordOptions, record_blocking};
use crate::profile::{self, OutputDest, Profile, commands as profile_commands};
use crate::transcribe::{self, TranscribeOpts};

#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("source-mode")
        .required(true)
        .multiple(false)
        .args(["profile", "output"])
))]
pub(crate) struct RecordArgs {
    /// Profile name to drive the entire record + transcribe pipeline.
    /// Mutually exclusive with the raw flags below.
    #[arg(
        long,
        conflicts_with_all = [
            "mic", "monitor", "output", "duration", "max_duration_minutes",
            "transcribe", "model", "lang"
        ]
    )]
    pub(crate) profile: Option<String>,

    /// `PipeWire` mic source (node name or `default`).
    #[arg(long, default_value = "default")]
    pub(crate) mic: String,

    /// `PipeWire` sink monitor (node name or `default`).
    #[arg(long, default_value = "default")]
    pub(crate) monitor: String,

    /// Output FLAC path (required unless `--profile` is set).
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,

    /// Recording duration in seconds (0 = run until Ctrl+C, but still
    /// capped by `--max-duration-minutes`).
    #[arg(long, default_value_t = 0)]
    pub(crate) duration: u64,

    /// Hard upper bound on a single recording, in minutes. Acts as the
    /// `max_duration_minutes` safeguard from IDEA.md against runaway
    /// captures. Pass `0` to opt out explicitly.
    #[arg(long, default_value_t = 240)]
    pub(crate) max_duration_minutes: u64,

    /// Backend for post-record transcription. Omit to skip transcribing.
    /// Only `whisper-cpp` is supported in M2; M5 widens this.
    #[arg(long)]
    pub(crate) transcribe: Option<String>,

    /// Model name for the post-record transcribe step. Required when
    /// `--transcribe` is set; M2 dropped the previously-hardcoded
    /// `small` default to honour the no-hidden-defaults rule
    /// (CLAUDE.md). Use `--profile default` for the previous behaviour.
    #[arg(long, requires = "transcribe", required_if_eq("transcribe", "whisper-cpp"))]
    pub(crate) model: Option<String>,

    /// Language: ISO 639-1 code (e.g. `cs`, `en`) or `auto` for
    /// autodetect. Required when `--transcribe` is set; M2 dropped the
    /// previously-hardcoded `auto` default for the same reason as
    /// `--model`.
    #[arg(long, requires = "transcribe", required_if_eq("transcribe", "whisper-cpp"))]
    pub(crate) lang: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct TranscribeArgs {
    /// Input audio file.
    pub(crate) input: PathBuf,

    /// Profile name; pulls `[transcription]` settings from the profile.
    /// Mutually exclusive with the raw `--backend / --model / --language` flags.
    #[arg(long, conflicts_with_all = ["backend", "model", "language"])]
    pub(crate) profile: Option<String>,

    /// Backend identifier (e.g. `whisper-cpp`).
    #[arg(long, default_value = "whisper-cpp")]
    pub(crate) backend: String,

    /// Model name (backend-specific).
    #[arg(long, default_value = "small")]
    pub(crate) model: String,

    /// Source language (ISO 639-1, e.g. `cs`, `en`).
    #[arg(long, default_value = "auto")]
    pub(crate) language: String,
}

/// `zwhisper profile` subcommands — pure config-plane helpers; do not
/// touch `GStreamer`.
#[derive(Debug, Subcommand)]
pub(crate) enum ProfileCmd {
    /// List all profiles (user override > shipped > embedded).
    List,
    /// Show the resolved TOML for a named profile.
    Show {
        /// Profile name (`[A-Za-z0-9._-]+`).
        name: String,
    },
    /// Copy a profile into the user override dir under a new name.
    Clone {
        /// Source profile name (any source).
        src: String,
        /// Destination user override name; refuses to overwrite.
        dst: String,
    },
    /// Force the migration chain on a user override profile. No-op
    /// when already at `CURRENT_SCHEMA_VERSION`.
    Migrate {
        /// User override profile name.
        name: String,
    },
}

pub(crate) fn run_record(args: &RecordArgs) -> color_eyre::Result<()> {
    if let Some(name) = &args.profile {
        let profile = profile::load(name).map_err(|e| eyre!("{e}"))?;
        run_record_with_profile(&profile)
    } else {
        run_record_with_flags(args)
    }
}

fn run_record_with_profile(profile: &Profile) -> color_eyre::Result<()> {
    info!(profile = %profile.name, "record requested via profile");

    let output = profile.primary_output_path().ok_or_else(|| {
        eyre!(
            "profile {:?} has no `[[output]]` of type \"file\"; \
             add one with a path like \"~/Recordings/zwhisper/{{profile}}/{{timestamp}}.flac\"",
            profile.name
        )
    })?;

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| eyre!("could not create {}: {e}", parent.display()))?;
    }

    for out in &profile.outputs {
        match out {
            OutputDest::File { .. } => {}
            OutputDest::Clipboard => warn!(
                profile = %profile.name,
                "profile requests clipboard output; deferred to M4 tray"
            ),
            OutputDest::Notification => warn!(
                profile = %profile.name,
                "profile requests notification output; deferred to M4 tray"
            ),
        }
    }

    let cap_minutes = profile.recording.max_duration_minutes;
    let effective_duration = resolve_duration(0, cap_minutes)?;

    // Empty `system_output` is rejected by `Profile::validate`
    // (M2 ships mic + sink monitor only; mic-only mode lands in M3
    // alongside the rate parameterisation), so this is always a
    // non-empty node name or `"default"` here.
    let opts = RecordOptions {
        mic: profile.sources.mic.clone(),
        monitor: profile.sources.system_output.clone(),
        output: output.clone(),
    };

    let report = record_blocking(opts, effective_duration)
        .map_err(|e| eyre!("recording failed: {e}"))?;

    info!(
        session_id = %report.session_id,
        duration_ms = u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX),
        samples_written = report.samples_written,
        underruns = report.underruns,
        warnings = report.warnings.len(),
        audio_path = %report.audio_path.display(),
        profile = %profile.name,
        "recording complete (profile)",
    );

    if profile.transcription.auto {
        let opts = TranscribeOpts {
            backend: profile.transcription.backend.as_str().to_owned(),
            model: profile.transcription.model.clone(),
            language: profile.transcription.language.clone(),
        };
        run_transcribe_blocking(&report.audio_path, &opts)?;
    }

    Ok(())
}

fn run_record_with_flags(args: &RecordArgs) -> color_eyre::Result<()> {
    let output = args
        .output
        .clone()
        .ok_or_else(|| eyre!("--output is required when --profile is not set"))?;
    info!(
        mic = %args.mic,
        monitor = %args.monitor,
        output = %output.display(),
        duration_s = args.duration,
        max_duration_minutes = args.max_duration_minutes,
        "record requested",
    );

    let effective_duration = resolve_duration(args.duration, args.max_duration_minutes)?;
    info!(
        requested_duration_s = args.duration,
        effective_duration_s = effective_duration,
        "duration resolved against safety cap"
    );

    let opts = RecordOptions {
        mic: args.mic.clone(),
        // `--monitor ""` is rejected by `devices::resolve` with a
        // typed `InvalidArgument` — M2 ships mic + sink monitor
        // only.
        monitor: args.monitor.clone(),
        output: output.clone(),
    };

    let report = record_blocking(opts, effective_duration)
        .map_err(|e| eyre!("recording failed: {e}"))?;

    info!(
        session_id = %report.session_id,
        duration_ms = u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX),
        samples_written = report.samples_written,
        underruns = report.underruns,
        warnings = report.warnings.len(),
        audio_path = %report.audio_path.display(),
        "recording complete",
    );

    if let Some(backend) = &args.transcribe {
        let model = args
            .model
            .clone()
            .ok_or_else(|| eyre!("--model is required when --transcribe is set"))?;
        let language = args
            .lang
            .clone()
            .ok_or_else(|| eyre!("--lang is required when --transcribe is set"))?;
        let opts = TranscribeOpts {
            backend: backend.clone(),
            model,
            language,
        };
        run_transcribe_blocking(&report.audio_path, &opts)?;
    }

    Ok(())
}

fn run_transcribe_blocking(audio: &std::path::Path, opts: &TranscribeOpts) -> color_eyre::Result<()> {
    info!(
        backend = %opts.backend,
        model = %opts.model,
        language = %opts.language,
        audio = %audio.display(),
        "post-record transcribe starting",
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| eyre!("failed to build tokio runtime: {e}"))?;

    match rt.block_on(transcribe::transcribe_file(audio, opts)) {
        Ok(art) => {
            info!(
                txt = %art.txt_path.display(),
                json = %art.json_path.display(),
                audio_duration_ms =
                    u64::try_from(art.audio_duration.as_millis()).unwrap_or(u64::MAX),
                transcribe_duration_ms =
                    u64::try_from(art.duration.as_millis()).unwrap_or(u64::MAX),
                language = %art.language,
                model = %art.model,
                "post-record transcribe complete",
            );
            Ok(())
        }
        Err(err) => Err(eyre!(
            "recording succeeded ({}) but transcribe failed: {err}",
            audio.display(),
        )),
    }
}

/// Apply the runaway-recording safeguard (`max_duration_minutes` from
/// IDEA.md § 1) against the user-supplied `--duration` value.
fn resolve_duration(duration_s: u64, max_minutes: u64) -> color_eyre::Result<u64> {
    if max_minutes == 0 {
        return Ok(duration_s);
    }
    let cap_s = max_minutes.saturating_mul(60);
    if duration_s == 0 {
        return Ok(cap_s);
    }
    if duration_s > cap_s {
        bail!(
            "--duration {duration_s}s exceeds --max-duration-minutes {max_minutes} ({cap_s}s); \
             pass --max-duration-minutes 0 to opt out of the safeguard"
        );
    }
    Ok(duration_s)
}

pub(crate) async fn run_transcribe_async(args: &TranscribeArgs) -> color_eyre::Result<()> {
    let opts = if let Some(name) = &args.profile {
        let profile = profile::load(name).map_err(|e| eyre!("{e}"))?;
        TranscribeOpts {
            backend: profile.transcription.backend.as_str().to_owned(),
            model: profile.transcription.model.clone(),
            language: profile.transcription.language.clone(),
        }
    } else {
        TranscribeOpts {
            backend: args.backend.clone(),
            model: args.model.clone(),
            language: args.language.clone(),
        }
    };

    info!(
        input = %args.input.display(),
        backend = %opts.backend,
        model = %opts.model,
        language = %opts.language,
        "transcribe requested",
    );

    let art = transcribe::transcribe_file(&args.input, &opts)
        .await
        .map_err(|err| eyre!("{err}"))?;

    info!(
        txt = %art.txt_path.display(),
        json = %art.json_path.display(),
        audio_duration_ms =
            u64::try_from(art.audio_duration.as_millis()).unwrap_or(u64::MAX),
        transcribe_duration_ms =
            u64::try_from(art.duration.as_millis()).unwrap_or(u64::MAX),
        language = %art.language,
        model = %art.model,
        "transcribe complete",
    );

    Ok(())
}

pub(crate) fn run_transcribe(args: &TranscribeArgs) -> color_eyre::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| eyre!("failed to build tokio runtime: {e}"))?;
    rt.block_on(run_transcribe_async(args))
}

pub(crate) fn run_profile(cmd: &ProfileCmd) -> color_eyre::Result<()> {
    match cmd {
        ProfileCmd::List => profile_commands::list(),
        ProfileCmd::Show { name } => profile_commands::show(name),
        ProfileCmd::Clone { src, dst } => profile_commands::clone(src, dst),
        ProfileCmd::Migrate { name } => profile_commands::migrate(name),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::match_wildcard_for_single_variants
)]
mod tests {
    use clap::Parser;

    use super::{ProfileCmd, RecordArgs, TranscribeArgs, resolve_duration};

    #[derive(Debug, Parser)]
    #[command(name = "zwhisper")]
    struct TestCli {
        #[command(subcommand)]
        command: TestCommand,
    }

    #[derive(Debug, clap::Subcommand)]
    enum TestCommand {
        Record(RecordArgs),
        Transcribe(TranscribeArgs),
        #[command(subcommand)]
        Profile(ProfileCmd),
    }

    #[test]
    fn cap_disabled_passes_duration_through() {
        assert_eq!(resolve_duration(0, 0).unwrap(), 0);
        assert_eq!(resolve_duration(99_999, 0).unwrap(), 99_999);
    }

    #[test]
    fn duration_zero_uses_cap() {
        assert_eq!(resolve_duration(0, 240).unwrap(), 240 * 60);
    }

    #[test]
    fn duration_within_cap_is_passed_through() {
        assert_eq!(resolve_duration(60, 240).unwrap(), 60);
        assert_eq!(resolve_duration(240 * 60, 240).unwrap(), 240 * 60);
    }

    #[test]
    fn duration_above_cap_is_rejected() {
        let err = resolve_duration(240 * 60 + 1, 240).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exceeds"), "unexpected message: {msg}");
        assert!(msg.contains("max-duration-minutes"), "unexpected message: {msg}");
    }

    #[test]
    fn record_with_profile_parses() {
        let cli = TestCli::try_parse_from(["zwhisper", "record", "--profile", "meeting"])
            .expect("parse should succeed");
        match cli.command {
            TestCommand::Record(args) => {
                assert_eq!(args.profile.as_deref(), Some("meeting"));
                assert!(args.output.is_none());
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn record_profile_conflicts_with_output() {
        let err = TestCli::try_parse_from([
            "zwhisper",
            "record",
            "--profile",
            "meeting",
            "--output",
            "/tmp/x.flac",
        ])
        .expect_err("clap must reject --profile + --output");
        let msg = err.to_string();
        assert!(
            msg.contains("--profile") || msg.contains("--output") || msg.contains("conflict"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn record_profile_conflicts_with_transcribe_chain() {
        let err = TestCli::try_parse_from([
            "zwhisper",
            "record",
            "--profile",
            "meeting",
            "--transcribe",
            "whisper-cpp",
        ])
        .expect_err("clap must reject --profile + --transcribe");
        assert!(err.to_string().contains("conflict")
            || err.to_string().contains("--transcribe"));
    }

    #[test]
    fn record_either_profile_or_output_required() {
        let err = TestCli::try_parse_from(["zwhisper", "record", "--duration", "3"])
            .expect_err("clap must require one of --profile / --output");
        assert!(
            err.to_string().contains("--profile") || err.to_string().contains("--output"),
            "{err}"
        );
    }

    #[test]
    fn record_with_transcribe_requires_model_and_lang() {
        let err = TestCli::try_parse_from([
            "zwhisper",
            "record",
            "--output",
            "/tmp/x.flac",
            "--transcribe",
            "whisper-cpp",
        ])
        .expect_err("clap must require --model / --lang once --transcribe is set");
        let msg = err.to_string();
        assert!(msg.contains("--model") || msg.contains("--lang"), "{msg}");
    }

    #[test]
    fn record_full_legacy_invocation_parses() {
        let cli = TestCli::try_parse_from([
            "zwhisper",
            "record",
            "--output",
            "/tmp/x.flac",
            "--duration",
            "3",
            "--transcribe",
            "whisper-cpp",
            "--model",
            "small",
            "--lang",
            "en",
        ])
        .expect("parse should succeed");
        match cli.command {
            TestCommand::Record(args) => {
                assert_eq!(args.output.as_deref().unwrap().to_str(), Some("/tmp/x.flac"));
                assert_eq!(args.transcribe.as_deref(), Some("whisper-cpp"));
                assert_eq!(args.model.as_deref(), Some("small"));
                assert_eq!(args.lang.as_deref(), Some("en"));
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn transcribe_with_profile_conflicts_with_backend_flags() {
        let err = TestCli::try_parse_from([
            "zwhisper",
            "transcribe",
            "/tmp/x.flac",
            "--profile",
            "meeting",
            "--backend",
            "whisper-cpp",
        ])
        .expect_err("clap must reject --profile + --backend");
        let msg = err.to_string();
        assert!(msg.contains("conflict") || msg.contains("--profile"));
    }

    #[test]
    fn profile_subcommands_parse() {
        for argv in [
            ["zwhisper", "profile", "list", "", ""].as_slice(),
            ["zwhisper", "profile", "show", "meeting", ""].as_slice(),
            ["zwhisper", "profile", "clone", "meeting", "my"].as_slice(),
            ["zwhisper", "profile", "migrate", "meeting", ""].as_slice(),
        ] {
            let argv: Vec<&str> = argv.iter().filter(|s| !s.is_empty()).copied().collect();
            TestCli::try_parse_from(argv).expect("parse should succeed");
        }
    }
}
