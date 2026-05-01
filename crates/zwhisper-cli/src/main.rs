use std::path::PathBuf;
use std::sync::OnceLock;

use clap::{Parser, Subcommand};
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

mod audio;
mod cli;
mod profile;
mod transcribe;

use crate::cli::{ProfileCmd, RecordArgs, TranscribeArgs};

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
    /// Record audio to a file (M0 walking skeleton).
    Record(RecordArgs),

    /// Transcribe an existing audio file.
    Transcribe(TranscribeArgs),

    /// Manage user / shipped / embedded TOML profiles (M2).
    #[command(subcommand)]
    Profile(ProfileCmd),

    /// Print runtime status information.
    Status,
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();
    let _log_guard = init_tracing(cli.verbose);

    match &cli.command {
        Command::Record(args) => {
            init_gstreamer()?;
            cli::run_record(args)
        }
        Command::Transcribe(args) => cli::run_transcribe(args),
        Command::Profile(cmd) => cli::run_profile(cmd),
        Command::Status => {
            info!("status command invoked");
            println!("zwhisper: M2 profile system; daemon split lands in M3");
            Ok(())
        }
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

fn init_gstreamer() -> color_eyre::Result<()> {
    static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();
    GST_INIT
        .get_or_init(|| gstreamer::init().map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| color_eyre::eyre::eyre!("failed to initialise GStreamer: {e}"))
}
