use std::path::PathBuf;

use clap::Args;
use color_eyre::eyre::bail;
use tracing::info;

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

    /// Recording duration in seconds (0 = run until Ctrl+C).
    #[arg(long, default_value_t = 0)]
    pub(crate) duration: u64,
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
        "record requested",
    );
    bail!("record: not implemented yet — pending M0 GStreamer pipeline");
}

pub(crate) fn run_transcribe(args: &TranscribeArgs) -> color_eyre::Result<()> {
    info!(
        input = %args.input.display(),
        backend = %args.backend,
        model = %args.model,
        language = %args.language,
        "transcribe requested",
    );
    bail!("transcribe: not implemented yet — pending M1 whisper.cpp integration");
}
