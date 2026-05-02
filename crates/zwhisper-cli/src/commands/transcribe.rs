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
use zwhisper_core::profile::schema::{Backend, DeepgramSettings};
use zwhisper_core::transcribe::{self, BackendConfig, TranscribeOpts};

use crate::cli::TranscribeArgs;

use super::build_runtime;

pub(crate) fn run(args: &TranscribeArgs) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    rt.block_on(run_async(args))
}

async fn run_async(args: &TranscribeArgs) -> color_eyre::Result<()> {
    let opts = if let Some(name) = &args.profile {
        let profile = profile::load(name).map_err(|e| eyre!("{e}"))?;
        let backend_config = match profile.transcription.backend {
            Backend::Deepgram => BackendConfig::Deepgram(
                profile
                    .transcription
                    .deepgram
                    .clone()
                    .unwrap_or_default(),
            ),
            Backend::WhisperCpp => BackendConfig::WhisperCpp,
            other => {
                return Err(eyre!(
                    "backend `{}` is not supported in this build",
                    other.as_str()
                ));
            }
        };
        // Pick the model from the most-specific block: when the
        // profile carries a `[transcription.deepgram]` table, its
        // `model` overrides the generic `[transcription].model`.
        // Without this, the backend-specific knob would be silently
        // ignored when both fields disagreed (user feedback #2,
        // 2026-05-02).
        let model = match (&profile.transcription.backend, &profile.transcription.deepgram) {
            (Backend::Deepgram, Some(dg)) => dg.model.clone(),
            _ => profile.transcription.model.clone(),
        };
        TranscribeOpts {
            backend: profile.transcription.backend.as_str().to_owned(),
            model,
            language: profile.transcription.language.clone(),
            backend_config,
        }
    } else {
        // CLI flags route through the legacy string only — adding
        // a `--deepgram-settings` family of flags is out of M5
        // scope (cloud-only flow goes via profiles).
        let backend_config = match args.backend.as_str() {
            "deepgram" => BackendConfig::Deepgram(DeepgramSettings::default()),
            _ => BackendConfig::WhisperCpp,
        };
        TranscribeOpts {
            backend: args.backend.clone(),
            model: args.model.clone(),
            language: args.language.clone(),
            backend_config,
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
