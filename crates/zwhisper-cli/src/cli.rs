//! Clap argument types for `zwhisper`.
//!
//! The clap surface is the regression net for M3 — its shape is kept
//! byte-identical with M2 so that existing parser tests in
//! `tests/cli.rs` and the in-module `mod tests` block still pass.
//! Runtime semantics narrowed in M3:
//!
//! - `record` requires `--profile` at runtime. The bare-flag form
//!   (`--output --mic --monitor --transcribe --model --lang`) still
//!   parses — we keep the clap surface intact — but the `commands::
//!   record::run` dispatcher returns exit code 2 with a hint pointing
//!   at `--profile default` for users who relied on the M0/M1
//!   invocation shape.
//! - `transcribe <file>` keeps both invocation forms (`--profile` or
//!   the raw `--backend / --model / --language` triple). Its
//!   implementation stays local — the daemon does not yet expose a
//!   transcribe-only RPC.
//!
//! `resolve_duration` is no longer wired into the runtime path
//! (the daemon owns `max_duration_minutes` enforcement now), but the
//! function and its unit tests are kept here as a self-contained
//! invariant for the safeguard formula until M4 either re-uses or
//! retires it.

use std::path::PathBuf;

use clap::{Args, Subcommand};

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
    #[arg(
        long,
        requires = "transcribe",
        required_if_eq("transcribe", "whisper-cpp")
    )]
    pub(crate) model: Option<String>,

    /// Language: ISO 639-1 code (e.g. `cs`, `en`) or `auto` for
    /// autodetect. Required when `--transcribe` is set; M2 dropped the
    /// previously-hardcoded `auto` default for the same reason as
    /// `--model`.
    #[arg(
        long,
        requires = "transcribe",
        required_if_eq("transcribe", "whisper-cpp")
    )]
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

/// Apply the runaway-recording safeguard (`max_duration_minutes` from
/// IDEA.md § 1) against the user-supplied `--duration` value.
///
/// Phase 4 moved the runtime enforcement into `zwhisperd`; this
/// function is retained so the formula stays unit-tested in one place
/// until M4 either re-uses (mic-only mode) or retires it.
#[allow(dead_code)]
pub(crate) fn resolve_duration(duration_s: u64, max_minutes: u64) -> color_eyre::Result<u64> {
    use color_eyre::eyre::bail;

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
        assert!(
            msg.contains("max-duration-minutes"),
            "unexpected message: {msg}"
        );
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
        assert!(err.to_string().contains("conflict") || err.to_string().contains("--transcribe"));
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
                assert_eq!(
                    args.output.as_deref().unwrap().to_str(),
                    Some("/tmp/x.flac")
                );
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
