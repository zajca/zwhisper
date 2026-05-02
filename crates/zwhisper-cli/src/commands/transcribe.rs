//! `zwhisper transcribe <file>` — local invocation of `whisper-cli`.
//!
//! M3 keeps this command local: the daemon currently exposes only the
//! end-to-end record-then-transcribe flow via `Recorder1.StartRecording`,
//! not a transcribe-only RPC. Adding such an RPC is M4 work. Until
//! then, the CLI still pulls the `transcribe` feature of
//! `zwhisper-core` and shells out to `whisper-cli` itself — the
//! `transcribe` feature pulls `whisper-cli` discovery + tokio process
//! but **not** `GStreamer`, so this stays compatible with `DoD` #8.

use color_eyre::eyre::eyre;
use tracing::info;
use zwhisper_core::profile;
use zwhisper_core::transcribe::{self, TranscribeOpts};

use crate::cli::TranscribeArgs;

use super::build_runtime;

pub(crate) fn run(args: &TranscribeArgs) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    rt.block_on(run_async(args))
}

async fn run_async(args: &TranscribeArgs) -> color_eyre::Result<()> {
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
