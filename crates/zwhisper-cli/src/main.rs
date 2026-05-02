//! `zwhisper` — thin D-Bus client over `zwhisperd`.
//!
//! M3 split the recorder into a daemon + CLI; this binary owns the
//! clap surface and dispatches every command to a `commands::*`
//! module. Every command opens its own current-thread tokio runtime
//! before issuing D-Bus calls — we deliberately do **not** put a
//! `#[tokio::main]` on `main` because two of the four commands
//! (`profile clone`, `profile migrate`) stay synchronous (file I/O
//! against `${XDG_CONFIG_HOME}/zwhisper/profiles/`) and pulling them
//! into an async dispatcher would force them to enter and exit a
//! runtime they do not need. The trade-off: each async command pays
//! a small runtime-construction cost per invocation, which is
//! negligible for an interactive CLI.
//!
//! `GStreamer` is no longer initialised here — the daemon owns the
//! audio path. The CLI dropped the `audio` feature of `zwhisper-core`
//! (`DoD` #8) and `cargo tree -p zwhisper-cli` no longer lists
//! `gstreamer`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

mod cli;
mod commands;
mod profile_commands;

use crate::cli::{BackendCmd, ProfileCmd, RecordArgs, TranscribeArgs};

#[derive(Debug, Parser)]
#[command(
    name = "zwhisper",
    version,
    about = "Linux desktop recorder + whisper.cpp transcription frontend",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Increase log verbosity. Can be repeated (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Record audio via the daemon (M3: D-Bus client over `zwhisperd`).
    Record(RecordArgs),

    /// Transcribe an existing audio file (local; no daemon involvement).
    Transcribe(TranscribeArgs),

    /// Manage user / shipped / embedded TOML profiles (M2 + M3).
    #[command(subcommand)]
    Profile(ProfileCmd),

    /// Direct cloud-backend probes (M5+) — health check, etc.
    /// Bypasses the daemon; reads the API key from the same
    /// resolution chain as the recorder.
    #[command(subcommand)]
    Backend(BackendCmd),

    /// Print runtime status from the daemon.
    Status,
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();
    let _log_guard = init_tracing(cli.verbose);

    match &cli.command {
        Command::Record(args) => commands::record::run(args),
        Command::Transcribe(args) => commands::transcribe::run(args),
        Command::Profile(cmd) => commands::profile::run(cmd),
        Command::Backend(cmd) => commands::backend::run(cmd),
        Command::Status => commands::status::run(),
    }
}

fn init_tracing(verbosity: u8) -> Option<WorkerGuard> {
    let default_level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let stderr_layer = fmt::layer().with_target(false).with_writer(std::io::stderr);

    // Best-effort daily file appender at $XDG_STATE_HOME/zwhisper/zwhisper.log.
    // If we cannot create the directory, log only to stderr — never abort the
    // CLI just because the log file is unavailable.
    let (file_layer, guard) = match log_dir() {
        Some(dir) if std::fs::create_dir_all(&dir).is_ok() => {
            let appender = tracing_appender::rolling::daily(dir, "zwhisper.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(fmt::layer().with_ansi(false).with_writer(writer)),
                Some(guard),
            )
        }
        _ => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    guard
}

fn log_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|base| base.join("zwhisper"))
}
