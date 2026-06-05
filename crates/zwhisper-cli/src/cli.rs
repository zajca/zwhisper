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
#[command(group(
    clap::ArgGroup::new("transcribe-routing")
        .required(false)
        .multiple(false)
        .args(["queue", "detach"])
))]
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

    /// RFC-daemon-role F1.1: route the transcription through the daemon
    /// as a tracked job and **wait** for it (so it lands in
    /// `zwhisper history` and can be retried). Without `--queue` or
    /// `--detach` the command runs LOCALLY in this process with zero
    /// daemon dependency (the headless/ssh/cron guarantee, IDEA §5).
    #[arg(long)]
    pub(crate) queue: bool,

    /// RFC-daemon-role F1.1: enqueue the transcription as a daemon job,
    /// print the `job_id`, and return immediately (do not wait).
    #[arg(long)]
    pub(crate) detach: bool,
}

#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("status-format")
        .required(false)
        .multiple(false)
        .args(["json", "waybar"])
))]
pub(crate) struct StatusArgs {
    /// Print the daemon status as JSON for scripts.
    #[arg(long)]
    pub(crate) json: bool,

    /// Print Waybar-compatible JSON.
    #[arg(long)]
    pub(crate) waybar: bool,
}

#[derive(Debug, Args)]
pub(crate) struct InstructionsArgs {
    /// Print concise Markdown intended for an AI agent operating zwhisper.
    #[arg(long)]
    pub(crate) agent: bool,
}

/// `zwhisper backend` subcommands — direct calls into a backend's
/// API surface, bypassing the daemon. M5 ships a single `health`
/// action that probes Deepgram's `/v1/projects` endpoint with the
/// resolved API key; useful for validating a freshly-rotated
/// `secrets.toml` before kicking off an actual recording. See
/// `IDEA.md` § 4 for the per-backend semantics.
#[derive(Debug, Subcommand)]
pub(crate) enum BackendCmd {
    /// Probe the backend's auth + reachability without uploading
    /// audio. Exit code 0 on OK, 2 on auth / quota / network failure.
    Health {
        /// Backend identifier — currently only `deepgram`. The list
        /// widens as more cloud backends land.
        #[arg(long, default_value = "deepgram")]
        backend: String,
    },
    /// List every backend id and whether its transcription code is
    /// compiled into this build (feature-gated backends like `parakeet`
    /// are default-OFF). Prints the rebuild hint for any that are not.
    List,
}

/// `zwhisper hotkey …` subcommands — manage and diagnose the
/// system-wide hotkey binding (xdg-desktop-portal `GlobalShortcuts`).
/// Bypasses the tray; useful when running CLI-only or scripted from a
/// keyboard mapper. See M6-plan § `DoD` #10–#13 for the per-subcommand
/// truth table and exit codes.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub(crate) enum HotkeyCmd {
    /// Show current binding state (`BOUND` / `NOT_BOUND` / `UNAVAILABLE`).
    Status,
    /// Open the portal `BindShortcuts` dialog so the user picks a chord.
    Bind,
    /// Remove the binding (idempotent — re-running prints `unbound`).
    Unbind,
    /// Diagnostic — report portal availability and version.
    Probe,
}

/// `zwhisper profile` subcommands — pure config-plane helpers; do not
/// touch `GStreamer`.
#[derive(Debug, Subcommand)]
pub(crate) enum ProfileCmd {
    /// List all profiles (user override > shipped > embedded).
    List,
    /// Set the active profile used by `zwhisper toggle`.
    Set {
        /// Profile name (`[A-Za-z0-9._-]+`).
        name: String,
    },
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

/// `zwhisper model` subcommands — manage local whisper.cpp model
/// files used by the `whisper-cpp` backend.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub(crate) enum ModelCmd {
    /// List known models from the embedded manifest and local install state.
    List,
    /// Print the models directory, or the expected path for a known model.
    Path {
        /// Manifest model name, e.g. `tiny`, `small`, `large-v3`.
        model: Option<String>,
    },
    /// Download and verify a known model into the local models directory.
    #[command(alias = "download")]
    Install {
        /// Manifest model name, e.g. `tiny`, `small`, `large-v3`.
        model: String,
    },
    /// Verify an installed model against the embedded SHA-256 manifest.
    Verify {
        /// Manifest model name, e.g. `tiny`, `small`, `large-v3`.
        model: String,
    },
}

/// `zwhisper jobs …` — inspect/cancel daemon transcription jobs
/// (RFC-daemon-role Feature 1).
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub(crate) enum JobsCmd {
    /// List queued/running jobs (the default when no subcommand given is
    /// handled in the dispatcher).
    List,
    /// Cancel a queued or running job by id (best-effort).
    Cancel {
        /// Job id (UUID) as printed by `transcribe --detach` / `jobs`.
        id: String,
    },
}

/// `zwhisper history …` — durable session history (RFC-daemon-role
/// Feature 2). Bare `zwhisper history` lists recent sessions.
#[derive(Debug, Subcommand)]
pub(crate) enum HistoryCmd {
    /// Drop a session from the index. With `--delete-files`, also remove
    /// the referenced audio/transcript files (otherwise audio is kept).
    Forget {
        /// Session id (UUID).
        id: String,
        /// Also delete the audio + transcript files from disk.
        #[arg(long)]
        delete_files: bool,
    },
}

/// `zwhisper history [--limit N]` flags. The optional `forget`
/// subcommand is handled separately; a bare invocation lists.
#[derive(Debug, Args)]
pub(crate) struct HistoryArgs {
    /// Maximum number of recent sessions to show.
    #[arg(long)]
    pub(crate) limit: Option<u32>,

    #[command(subcommand)]
    pub(crate) command: Option<HistoryCmd>,
}

/// Destination for `zwhisper output last`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum OutputTarget {
    /// Copy the transcript text into the clipboard.
    Clipboard,
    /// Raise a desktop notification.
    Notify,
    /// Type the transcript at the cursor via wtype (wlroots only).
    Type,
}

/// `zwhisper output …` — one-shot manual delivery of the last
/// transcript (RFC-daemon-role F3.2 fallback for missed best-effort
/// delivery).
#[derive(Debug, Subcommand)]
pub(crate) enum OutputCmd {
    /// Deliver the most recent finished transcript to the chosen target.
    Last {
        /// Where to deliver: `clipboard`, `notify`, or `type`.
        #[arg(long)]
        to: OutputTarget,
    },
}

/// `zwhisper deliver …` — the session-bound delivery consumer
/// (RFC-daemon-role Feature 3). Normally run via the auto-enabled
/// `graphical-session.target` systemd user unit.
#[derive(Debug, Args)]
pub(crate) struct DeliverArgs {
    /// Subscribe to `Jobs1.JobCompleted` and honour each job's resolved
    /// `outputs` (clipboard/notification). Required — there is no other
    /// mode yet, but the flag keeps room for future one-shot modes and
    /// makes the long-running intent explicit.
    #[arg(long)]
    pub(crate) listen: bool,
}

/// `zwhisper audio …` subcommands — guided microphone setup &
/// calibration (RFC-mic-setup, Phases 1+2). Read-only enumeration and
/// metering, plus a calibration flow that measures speech level,
/// recommends a safe `PipeWire` volume, and (with `--apply`) sets it.
///
/// The whole group sits behind the default-on `setup` feature; building
/// the CLI with `--no-default-features` drops it entirely. None of these
/// commands need GStreamer or a running daemon — they shell out to
/// `pw-dump` / `wpctl` / `pw-cat` via `zwhisper-core`'s `setup` module.
#[cfg(feature = "setup")]
#[derive(Debug, Subcommand)]
pub(crate) enum AudioCmd {
    /// Enumerate audio inputs and outputs (id, description, default
    /// marker, monitor marker, volume%). Sources first, then sinks.
    Devices {
        /// Emit machine-readable JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },

    /// Live VU meter from raw `pw-cat` PCM. Refreshes an ASCII bar with
    /// peak/RMS dBFS and a clip indicator until Ctrl+C. Read-only.
    Meter {
        /// Device selector: `default`, a `node.name`, or a numeric id.
        /// Defaults to the current default source.
        #[arg(long)]
        source: Option<String>,
    },

    /// Measure speech level, recommend a safe volume, and optionally
    /// apply it / persist it to a profile. A dry run by default.
    Calibrate(AudioCalibrateArgs),

    /// Interactive wizard (RFC-mic-setup Phase 4): pick a mic, calibrate
    /// it, choose a dictation/meeting preset, optionally make it the
    /// default source, and write the choice into a user-override
    /// profile. Composes the `devices` / `calibrate` building blocks and
    /// always applies the calibrated volume after an explicit
    /// confirmation. Needs a TTY and real hardware.
    Setup(AudioSetupArgs),
}

/// Flags for `zwhisper audio setup`. Every flag is optional — the
/// wizard prompts interactively for anything not supplied — so the
/// surface mirrors [`AudioCalibrateArgs`] without inheriting its
/// non-interactive switches (`--apply` / `--set-default` are decided in
/// the wizard, not on the command line).
#[cfg(feature = "setup")]
#[derive(Debug, Args)]
pub(crate) struct AudioSetupArgs {
    /// User-override profile to write the chosen mic + preset into. When
    /// omitted the wizard suggests a name (the preset) and prompts. A
    /// shipped/embedded name is cloned to a user override automatically.
    #[arg(long)]
    pub(crate) profile: Option<String>,

    /// Override the target speech peak in dBFS (default from
    /// `SetupConfig`). `allow_hyphen_values` lets clap accept the
    /// negative dBFS value (`--target-peak-db -6.0`) instead of
    /// mistaking the leading `-` for a new flag.
    #[arg(long, allow_hyphen_values = true)]
    pub(crate) target_peak_db: Option<f32>,

    /// Override the saturation cap: the highest linear volume the
    /// calibration loop will set (default from `SetupConfig`). Lower it
    /// for saturation-prone hardware (e.g. an ALC1220) when the wizard
    /// warns about a high noise floor.
    #[arg(long)]
    pub(crate) max_volume: Option<f32>,
}

/// Flags for `zwhisper audio calibrate`. Pulled into its own `Args`
/// struct (mirroring `RecordArgs` / `TranscribeArgs`) so the long flag
/// set stays readable and the parser truth-table can target it directly.
#[cfg(feature = "setup")]
#[derive(Debug, Args)]
pub(crate) struct AudioCalibrateArgs {
    /// Device selector: `default`, a `node.name`, or a numeric id.
    /// Defaults to the current default source.
    #[arg(long)]
    pub(crate) source: Option<String>,

    /// User-override profile to persist `sources.mic` (the selected
    /// node) into. The profile must already be a user override — clone a
    /// shipped/embedded profile first (`zwhisper profile clone`).
    #[arg(long)]
    pub(crate) profile: Option<String>,

    /// Override the target speech peak in dBFS (default from
    /// `SetupConfig`, mid of the RFC −9…−6 window). `allow_hyphen_values`
    /// lets clap accept the negative dBFS value (`--target-peak-db -6.0`)
    /// instead of mistaking the leading `-` for a new flag.
    #[arg(long, allow_hyphen_values = true)]
    pub(crate) target_peak_db: Option<f32>,

    /// Seconds of speech to capture per iteration (default from
    /// `SetupConfig`).
    #[arg(long)]
    pub(crate) seconds: Option<f32>,

    /// Actually set the recommended `PipeWire` volume (and iterate to
    /// converge). Without this flag the command is a dry run that only
    /// prints the recommendation.
    #[arg(long)]
    pub(crate) apply: bool,

    /// Also make the calibrated device the system default source
    /// (`wpctl set-default`). Global; gated behind this explicit flag.
    #[arg(long)]
    pub(crate) set_default: bool,

    /// Override the saturation cap: the highest linear volume the
    /// recommender / apply loop will set (default from `SetupConfig`).
    #[arg(long)]
    pub(crate) max_volume: Option<f32>,
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

    #[cfg(feature = "setup")]
    use super::AudioCmd;
    use super::{
        HotkeyCmd, InstructionsArgs, ModelCmd, ProfileCmd, RecordArgs, StatusArgs, TranscribeArgs,
        resolve_duration,
    };

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
        Status(StatusArgs),
        Instructions(InstructionsArgs),
        #[command(subcommand)]
        Profile(ProfileCmd),
        #[command(subcommand)]
        Model(ModelCmd),
        /// M6 — universal toggle, no flags.
        Toggle,
        /// M6 — hotkey binding management.
        #[command(subcommand)]
        Hotkey(HotkeyCmd),
        /// RFC-mic-setup — `audio {devices,meter,calibrate}`.
        #[cfg(feature = "setup")]
        #[command(subcommand)]
        Audio(AudioCmd),
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
            ["zwhisper", "profile", "set", "meeting", ""].as_slice(),
            ["zwhisper", "profile", "show", "meeting", ""].as_slice(),
            ["zwhisper", "profile", "clone", "meeting", "my"].as_slice(),
            ["zwhisper", "profile", "migrate", "meeting", ""].as_slice(),
        ] {
            let argv: Vec<&str> = argv.iter().filter(|s| !s.is_empty()).copied().collect();
            TestCli::try_parse_from(argv).expect("parse should succeed");
        }
    }

    #[test]
    fn model_subcommands_parse() {
        let list =
            TestCli::try_parse_from(["zwhisper", "model", "list"]).expect("parse should succeed");
        assert!(matches!(list.command, TestCommand::Model(ModelCmd::List)));

        let path = TestCli::try_parse_from(["zwhisper", "model", "path", "tiny"])
            .expect("parse should succeed");
        assert!(matches!(
            path.command,
            TestCommand::Model(ModelCmd::Path { ref model }) if model.as_deref() == Some("tiny")
        ));

        let dir_path =
            TestCli::try_parse_from(["zwhisper", "model", "path"]).expect("parse should succeed");
        assert!(matches!(
            dir_path.command,
            TestCommand::Model(ModelCmd::Path { model: None })
        ));

        for command in ["install", "download"] {
            let install = TestCli::try_parse_from(["zwhisper", "model", command, "large-v3"])
                .expect("parse should succeed");
            assert!(matches!(
                install.command,
                TestCommand::Model(ModelCmd::Install { ref model }) if model == "large-v3"
            ));
        }

        let verify = TestCli::try_parse_from(["zwhisper", "model", "verify", "large-v3"])
            .expect("parse should succeed");
        assert!(matches!(
            verify.command,
            TestCommand::Model(ModelCmd::Verify { ref model }) if model == "large-v3"
        ));
    }

    // ============================================================
    // M6 — `toggle` and `hotkey {…}` parser truth-table tests.
    // The parser surface is the only thing tested here; the
    // dispatchers live in `commands::toggle` / `commands::hotkey`
    // and have their own unit tests against the exit-code mapping.
    // ============================================================

    #[test]
    fn parses_toggle_no_args() {
        let cli = TestCli::try_parse_from(["zwhisper", "toggle"]).expect("parse should succeed");
        assert!(matches!(cli.command, TestCommand::Toggle));
    }

    #[test]
    fn parses_hotkey_status() {
        let cli = TestCli::try_parse_from(["zwhisper", "hotkey", "status"])
            .expect("parse should succeed");
        match cli.command {
            TestCommand::Hotkey(cmd) => assert_eq!(cmd, HotkeyCmd::Status),
            other => panic!("expected Hotkey(Status), got {other:?}"),
        }
    }

    #[test]
    fn parses_hotkey_bind() {
        let cli =
            TestCli::try_parse_from(["zwhisper", "hotkey", "bind"]).expect("parse should succeed");
        match cli.command {
            TestCommand::Hotkey(cmd) => assert_eq!(cmd, HotkeyCmd::Bind),
            other => panic!("expected Hotkey(Bind), got {other:?}"),
        }
    }

    #[test]
    fn parses_hotkey_unbind() {
        let cli = TestCli::try_parse_from(["zwhisper", "hotkey", "unbind"])
            .expect("parse should succeed");
        match cli.command {
            TestCommand::Hotkey(cmd) => assert_eq!(cmd, HotkeyCmd::Unbind),
            other => panic!("expected Hotkey(Unbind), got {other:?}"),
        }
    }

    #[test]
    fn parses_hotkey_probe() {
        let cli =
            TestCli::try_parse_from(["zwhisper", "hotkey", "probe"]).expect("parse should succeed");
        match cli.command {
            TestCommand::Hotkey(cmd) => assert_eq!(cmd, HotkeyCmd::Probe),
            other => panic!("expected Hotkey(Probe), got {other:?}"),
        }
    }

    #[test]
    fn status_formats_parse() {
        for argv in [
            ["zwhisper", "status", "--json"].as_slice(),
            ["zwhisper", "status", "--waybar"].as_slice(),
        ] {
            let cli = TestCli::try_parse_from(argv).unwrap();
            match cli.command {
                TestCommand::Status(_) => {}
                other => panic!("expected status command, got {other:?}"),
            }
        }
    }

    #[test]
    fn status_formats_conflict() {
        let err = TestCli::try_parse_from(["zwhisper", "status", "--json", "--waybar"])
            .expect_err("clap must reject two status formats");
        assert!(err.to_string().contains("--json") || err.to_string().contains("--waybar"));
    }

    #[test]
    fn instructions_agent_parses() {
        let cli = TestCli::try_parse_from(["zwhisper", "instructions", "--agent"]).unwrap();
        match cli.command {
            TestCommand::Instructions(args) => assert!(args.agent),
            other => panic!("expected instructions command, got {other:?}"),
        }
    }

    // ============================================================
    // RFC-mic-setup — `audio {devices,meter,calibrate}` parser
    // truth-table. The parser surface is the regression net here; the
    // runtime behaviour (pw-cat metering, calibration loop) lives in
    // `commands::audio` and is hardware-verified separately.
    // ============================================================

    #[cfg(feature = "setup")]
    #[test]
    fn parses_audio_devices_plain_and_json() {
        let plain = TestCli::try_parse_from(["zwhisper", "audio", "devices"])
            .expect("parse should succeed");
        match plain.command {
            TestCommand::Audio(AudioCmd::Devices { json }) => assert!(!json),
            other => panic!("expected Audio(Devices), got {other:?}"),
        }

        let json = TestCli::try_parse_from(["zwhisper", "audio", "devices", "--json"])
            .expect("parse should succeed");
        match json.command {
            TestCommand::Audio(AudioCmd::Devices { json }) => assert!(json),
            other => panic!("expected Audio(Devices {{ json: true }}), got {other:?}"),
        }
    }

    #[cfg(feature = "setup")]
    #[test]
    fn parses_audio_meter_default_and_with_source() {
        let bare =
            TestCli::try_parse_from(["zwhisper", "audio", "meter"]).expect("parse should succeed");
        match bare.command {
            TestCommand::Audio(AudioCmd::Meter { source }) => assert!(source.is_none()),
            other => panic!("expected Audio(Meter), got {other:?}"),
        }

        let with_source =
            TestCli::try_parse_from(["zwhisper", "audio", "meter", "--source", "alsa_input.mic"])
                .expect("parse should succeed");
        match with_source.command {
            TestCommand::Audio(AudioCmd::Meter { source }) => {
                assert_eq!(source.as_deref(), Some("alsa_input.mic"));
            }
            other => panic!("expected Audio(Meter {{ source }}), got {other:?}"),
        }
    }

    #[cfg(feature = "setup")]
    #[test]
    fn parses_audio_calibrate_dry_run_defaults() {
        let cli = TestCli::try_parse_from(["zwhisper", "audio", "calibrate"])
            .expect("parse should succeed");
        match cli.command {
            TestCommand::Audio(AudioCmd::Calibrate(args)) => {
                assert!(args.source.is_none());
                assert!(args.profile.is_none());
                assert!(args.target_peak_db.is_none());
                assert!(args.seconds.is_none());
                assert!(!args.apply, "calibrate must default to a dry run");
                assert!(!args.set_default, "set-default must default off");
                assert!(args.max_volume.is_none());
            }
            other => panic!("expected Audio(Calibrate), got {other:?}"),
        }
    }

    #[cfg(feature = "setup")]
    #[test]
    fn parses_audio_calibrate_full_flag_combo() {
        let cli = TestCli::try_parse_from([
            "zwhisper",
            "audio",
            "calibrate",
            "--source",
            "70",
            "--profile",
            "dictation",
            "--target-peak-db",
            "-6.0",
            "--seconds",
            "5",
            "--apply",
            "--set-default",
            "--max-volume",
            "0.3",
        ])
        .expect("parse should succeed");
        match cli.command {
            TestCommand::Audio(AudioCmd::Calibrate(args)) => {
                assert_eq!(args.source.as_deref(), Some("70"));
                assert_eq!(args.profile.as_deref(), Some("dictation"));
                assert_eq!(args.target_peak_db, Some(-6.0));
                assert_eq!(args.seconds, Some(5.0));
                assert!(args.apply);
                assert!(args.set_default);
                assert_eq!(args.max_volume, Some(0.3));
            }
            other => panic!("expected Audio(Calibrate) full combo, got {other:?}"),
        }
    }

    #[cfg(feature = "setup")]
    #[test]
    fn audio_requires_a_subcommand() {
        TestCli::try_parse_from(["zwhisper", "audio"])
            .expect_err("clap must require an audio subcommand");
    }

    #[cfg(feature = "setup")]
    #[test]
    fn parses_audio_setup_no_args() {
        let cli =
            TestCli::try_parse_from(["zwhisper", "audio", "setup"]).expect("parse should succeed");
        match cli.command {
            TestCommand::Audio(AudioCmd::Setup(args)) => {
                assert!(args.profile.is_none(), "profile must be optional");
                assert!(args.target_peak_db.is_none());
                assert!(args.max_volume.is_none());
            }
            other => panic!("expected Audio(Setup), got {other:?}"),
        }
    }

    #[cfg(feature = "setup")]
    #[test]
    fn parses_audio_setup_full_flag_combo() {
        let cli = TestCli::try_parse_from([
            "zwhisper",
            "audio",
            "setup",
            "--profile",
            "dictation",
            "--target-peak-db",
            "-6.0",
            "--max-volume",
            "0.3",
        ])
        .expect("parse should succeed");
        match cli.command {
            TestCommand::Audio(AudioCmd::Setup(args)) => {
                assert_eq!(args.profile.as_deref(), Some("dictation"));
                assert_eq!(args.target_peak_db, Some(-6.0));
                assert_eq!(args.max_volume, Some(0.3));
            }
            other => panic!("expected Audio(Setup) full combo, got {other:?}"),
        }
    }
}
