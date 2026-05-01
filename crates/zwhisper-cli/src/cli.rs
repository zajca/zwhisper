use std::path::PathBuf;

use clap::Args;
use color_eyre::eyre::bail;
use tracing::info;

use crate::audio::recorder::{RecordOptions, record_blocking};
use crate::transcribe::{self, TranscribeOpts};

/// Default model used by the post-record `--transcribe` shortcut and by
/// the standalone `transcribe` command when `--model` is omitted.
const DEFAULT_MODEL: &str = "small";
/// Default language hint. `auto` triggers whisper.cpp autodetect.
const DEFAULT_LANGUAGE: &str = "auto";

#[derive(Debug, Args)]
pub(crate) struct RecordArgs {
    /// `PipeWire` mic source (node name or `default`).
    #[arg(long, default_value = "default")]
    pub(crate) mic: String,

    /// `PipeWire` sink monitor (node name or `default`).
    #[arg(long, default_value = "default")]
    pub(crate) monitor: String,

    /// Output FLAC path.
    #[arg(long)]
    pub(crate) output: PathBuf,

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
    /// Only `whisper-cpp` is supported in M1; M5 widens this.
    #[arg(long)]
    pub(crate) transcribe: Option<String>,

    /// Model name for the post-record transcribe step. Resolved by name
    /// only via `~/.local/share/zwhisper/models/ggml-{name}.bin`.
    /// `requires` would be a no-op alongside `default_value`, so the
    /// flag is `Option<String>` and the default is applied at use-site
    /// when `--transcribe` is set.
    #[arg(long, requires = "transcribe")]
    pub(crate) model: Option<String>,

    /// Language: ISO 639-1 code (e.g. `cs`, `en`) or `auto` for
    /// autodetect. `requires` would be a no-op alongside
    /// `default_value`, so the flag is `Option<String>` and the
    /// default is applied at use-site when `--transcribe` is set.
    #[arg(long, requires = "transcribe")]
    pub(crate) lang: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct TranscribeArgs {
    /// Input audio file.
    pub(crate) input: PathBuf,

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

pub(crate) fn run_record(args: &RecordArgs) -> color_eyre::Result<()> {
    info!(
        mic = %args.mic,
        monitor = %args.monitor,
        output = %args.output.display(),
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
        monitor: args.monitor.clone(),
        output: args.output.clone(),
    };

    let report = record_blocking(opts, effective_duration)
        .map_err(|e| color_eyre::eyre::eyre!("recording failed: {e}"))?;

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
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        let language = args
            .lang
            .clone()
            .unwrap_or_else(|| DEFAULT_LANGUAGE.to_owned());

        info!(
            backend = %backend,
            model = %model,
            language = %language,
            audio = %report.audio_path.display(),
            "post-record transcribe starting",
        );

        let opts = TranscribeOpts {
            backend: backend.clone(),
            model,
            language,
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| color_eyre::eyre::eyre!("failed to build tokio runtime: {e}"))?;

        match rt.block_on(transcribe::transcribe_file(&report.audio_path, &opts)) {
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
            }
            Err(err) => {
                // FLAC stays on disk — recording succeeded, only
                // transcription failed. The captured audio is the
                // M0 source-of-truth artefact (DoD #1).
                return Err(color_eyre::eyre::eyre!(
                    "recording succeeded ({}) but transcribe failed: {err}",
                    report.audio_path.display(),
                ));
            }
        }
    }

    Ok(())
}

/// Apply the runaway-recording safeguard (`max_duration_minutes` from
/// IDEA.md § 1) against the user-supplied `--duration` value.
///
/// Rules:
/// - `max_minutes == 0` → user explicitly disabled the cap; return
///   `duration` verbatim (including 0 = unlimited).
/// - otherwise the cap is `max_minutes * 60` seconds.
///   - `duration == 0` is interpreted as "run until Ctrl+C", which
///     under a non-zero cap means "stop at the cap" → return the cap.
///   - `duration > cap` is rejected with a typed error: silently
///     capping would surprise scripts that pass an explicit duration.
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
    info!(
        input = %args.input.display(),
        backend = %args.backend,
        model = %args.model,
        language = %args.language,
        "transcribe requested",
    );

    let opts = TranscribeOpts {
        backend: args.backend.clone(),
        model: args.model.clone(),
        language: args.language.clone(),
    };

    let art = transcribe::transcribe_file(&args.input, &opts)
        .await
        .map_err(|err| color_eyre::eyre::eyre!("{err}"))?;

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
        .map_err(|e| color_eyre::eyre::eyre!("failed to build tokio runtime: {e}"))?;
    rt.block_on(run_transcribe_async(args))
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

    use super::{RecordArgs, TranscribeArgs, resolve_duration};

    /// Local mirror of the binary's top-level CLI just for parser
    /// tests — it lets us exercise `clap`'s `requires =` rules
    /// against the same `RecordArgs` definition without dragging
    /// in `init_tracing`/`init_gstreamer`.
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
    fn record_with_transcribe_flag_parses() {
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
                assert_eq!(args.output.to_str(), Some("/tmp/x.flac"));
                assert_eq!(args.duration, 3);
                assert_eq!(args.transcribe.as_deref(), Some("whisper-cpp"));
                assert_eq!(args.model.as_deref(), Some("small"));
                assert_eq!(args.lang.as_deref(), Some("en"));
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn record_without_transcribe_leaves_post_record_options_unset() {
        let cli = TestCli::try_parse_from([
            "zwhisper",
            "record",
            "--output",
            "/tmp/x.flac",
            "--duration",
            "3",
        ])
        .expect("parse should succeed");

        match cli.command {
            TestCommand::Record(args) => {
                assert!(args.transcribe.is_none());
                assert!(args.model.is_none());
                assert!(args.lang.is_none());
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn record_model_without_transcribe_is_rejected() {
        let err = TestCli::try_parse_from([
            "zwhisper",
            "record",
            "--output",
            "/tmp/x.flac",
            "--model",
            "small",
        ])
        .expect_err("clap must reject --model without --transcribe");
        let msg = err.to_string();
        assert!(
            msg.contains("--transcribe") || msg.contains("transcribe"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn record_lang_without_transcribe_is_rejected() {
        let err = TestCli::try_parse_from([
            "zwhisper",
            "record",
            "--output",
            "/tmp/x.flac",
            "--lang",
            "en",
        ])
        .expect_err("clap must reject --lang without --transcribe");
        let msg = err.to_string();
        assert!(
            msg.contains("--transcribe") || msg.contains("transcribe"),
            "unexpected error message: {msg}"
        );
    }
}
