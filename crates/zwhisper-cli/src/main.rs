use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod cli;

use crate::cli::{RecordArgs, TranscribeArgs};

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

    /// Print runtime status information.
    Status,
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match &cli.command {
        Command::Record(args) => cli::run_record(args),
        Command::Transcribe(args) => cli::run_transcribe(args),
        Command::Status => {
            info!("status command invoked");
            println!("zwhisper: not running (M0 scaffolding)");
            Ok(())
        }
    }
}

fn init_tracing(verbosity: u8) {
    let default_level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
