//! The transcription coordinator (RFC: Coordinator).
//!
//! Successor to the old `transcribe_file` free function. It owns both
//! resolutions — audio representation and model artifact — then invokes
//! the selected backend. The legacy [`transcribe_file`] free function
//! survives as a thin compatibility facade that builds an
//! [`AudioSource`] from a path and delegates here.

use std::path::Path;

use crate::profile::schema::Backend;

use super::audio_source::{AudioCodec, AudioSource, PcmAvailability};
use super::config::AudioConfig;
use super::error::TranscribeError;
use super::model::{ModelArtifact, ModelKindTag};
use super::{
    AudioInputKind, BackendCapabilities, TranscribeOpts, Transcriber, TranscriptArtifacts,
    deepgram, models, registry, whisper_cpp,
};

/// Compatibility facade. Builds an [`AudioSource`] from a path
/// (`DecodeFromArtifact` PCM availability — no runtime cache) and
/// delegates to [`transcribe_source`]. Preserves the historical
/// `transcribe_file(&Path, &TranscribeOpts)` entry point used by the
/// CLI and daemon.
pub async fn transcribe_file(
    audio: &Path,
    opts: &TranscribeOpts,
) -> Result<TranscriptArtifacts, TranscribeError> {
    let cfg = AudioConfig::default();
    let codec = AudioCodec::from_path(audio).unwrap_or(AudioCodec::Flac);
    let source =
        AudioSource::from_encoded_file(audio, codec, cfg.asr_sample_rate_hz, chrono::Utc::now());
    transcribe_source(&source, opts).await
}

/// The coordinator entry point. Resolves the model artifact and the
/// audio input, checks them against the backend's capabilities,
/// reconciles the cross-axis ASR rate, then invokes the backend.
pub async fn transcribe_source(
    audio: &AudioSource,
    opts: &TranscribeOpts,
) -> Result<TranscriptArtifacts, TranscribeError> {
    let backend = build_backend(opts)?;
    let caps = backend.capabilities();

    // ----- Model resolution -----
    let models_dir =
        models::models_dir().map_err(|e| TranscribeError::ModelResolution(e.to_string()))?;
    let reg = registry::embedded();
    let spec = reg.lookup(opts.backend, &opts.model);
    let model_artifact = resolve_model_artifact(opts, &models_dir)?;

    if !caps.accepts_model_kind(model_artifact.tag()) {
        return Err(TranscribeError::UnsupportedModelKind {
            backend: backend.id(),
            kind: kind_label(model_artifact.tag()),
        });
    }

    // ----- Audio resolution (capability check) -----
    let chosen_input = choose_audio_input(&caps, audio)?;

    // ----- Cross-axis reconciliation -----
    // The one numeric invariant linking the two axes: a PCM-preferring
    // backend expects a specific ASR rate, and the PCM views are
    // produced at the configured normalized rate. These must agree
    // before we hand the backend PCM at the wrong rate.
    if matches!(
        chosen_input,
        AudioInputKind::PcmBuffer | AudioInputKind::PcmChunks
    ) {
        if let Some(spec) = spec {
            if let Some(expected) = spec.runtime.expected_asr_rate_hz {
                let actual = audio.metadata.normalized_sample_rate_hz;
                if expected != actual {
                    return Err(TranscribeError::AsrRateMismatch { expected, actual });
                }
            }
        }
    }

    tracing::debug!(
        backend = backend.id(),
        model = %opts.model,
        model_kind = kind_label(model_artifact.tag()),
        audio_input = chosen_input.label(),
        "coordinator resolved transcription inputs",
    );

    backend.transcribe(audio, &model_artifact, opts).await
}

/// Build the backend instance for the selected [`Backend`]. Unsupported
/// or not-compiled-in backends surface a typed error naming the reason.
fn build_backend(opts: &TranscribeOpts) -> Result<Box<dyn Transcriber>, TranscribeError> {
    match opts.backend {
        Backend::WhisperCpp => Ok(Box::new(whisper_cpp::WhisperCppLocal::new())),
        Backend::Deepgram => Ok(Box::new(deepgram::DeepgramBatch::new(
            opts.deepgram_settings(),
        ))),
        Backend::Parakeet => {
            #[cfg(feature = "parakeet")]
            {
                Ok(Box::new(super::parakeet::ParakeetLocal::new()))
            }
            #[cfg(not(feature = "parakeet"))]
            {
                Err(TranscribeError::BackendNotCompiled {
                    backend: "parakeet",
                    feature: "parakeet",
                })
            }
        }
        other @ (Backend::AssemblyAi | Backend::OpenAi) => {
            Err(TranscribeError::BackendUnsupported {
                backend: other.as_str().to_owned(),
            })
        }
    }
}

/// Resolve `(backend, model_id)` to a [`ModelArtifact`].
///
/// The registry is the primary path. For whisper.cpp, an id absent from
/// the registry falls back to the historical `ggml-<id>.bin` convention
/// (via [`models::resolve_model`]) so locally-placed custom models keep
/// working — this is the one documented backend-specific fallback and it
/// preserves the frozen public surface.
fn resolve_model_artifact(
    opts: &TranscribeOpts,
    models_dir: &Path,
) -> Result<ModelArtifact, TranscribeError> {
    let reg = registry::embedded();
    match reg.resolve(opts.backend, &opts.model, models_dir) {
        Ok(artifact) => Ok(artifact),
        Err(TranscribeError::ModelNotFound { .. }) if opts.backend == Backend::WhisperCpp => {
            // Legacy whisper convention: any allow-listed name maps to
            // ggml-<name>.bin so user-placed custom models still resolve.
            let path = models::resolve_model(&opts.model)?;
            Ok(ModelArtifact::File(path))
        }
        Err(e) => Err(e),
    }
}

/// Pick the audio input representation the backend will receive, given
/// what it accepts and what the source actually offers. Returns a typed
/// error when no accepted representation can be produced.
fn choose_audio_input(
    caps: &BackendCapabilities,
    audio: &AudioSource,
) -> Result<AudioInputKind, TranscribeError> {
    // File-preferring backends only need the artifact path; the artifact
    // always exists for a constructed AudioSource.
    if caps.preferred_input == AudioInputKind::EncodedFile
        && caps.accepts_input(AudioInputKind::EncodedFile)
    {
        return Ok(AudioInputKind::EncodedFile);
    }

    // PCM-preferring backends: prefer a live PCM view; fall back to
    // decode-from-artifact (decode is wired by the PCM decode module);
    // accept the encoded file only if the backend also lists it.
    let pcm_live = matches!(
        audio.pcm,
        PcmAvailability::InMemory(_) | PcmAvailability::Chunked(_)
    );
    let pcm_decodable = matches!(audio.pcm, PcmAvailability::DecodeFromArtifact);

    if caps.accepts_input(AudioInputKind::PcmBuffer)
        || caps.accepts_input(AudioInputKind::PcmChunks)
    {
        if pcm_live || pcm_decodable {
            // Report the backend's preferred PCM flavour for diagnostics;
            // the backend itself pulls the concrete representation from
            // the AudioSource.
            return Ok(match caps.preferred_input {
                AudioInputKind::PcmChunks => AudioInputKind::PcmChunks,
                _ => AudioInputKind::PcmBuffer,
            });
        }
    }

    if caps.accepts_input(AudioInputKind::EncodedFile) {
        return Ok(AudioInputKind::EncodedFile);
    }

    Err(TranscribeError::UnsupportedAudioInput {
        backend_preferred: caps.preferred_input.label(),
        reason: "no accepted audio representation could be produced from the source",
    })
}

fn kind_label(kind: ModelKindTag) -> &'static str {
    match kind {
        ModelKindTag::SingleFile => "single-file",
        ModelKindTag::DirectoryBundle => "directory-bundle",
        ModelKindTag::Remote => "remote",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn assemblyai_backend_is_unsupported() {
        let opts = TranscribeOpts {
            backend: Backend::AssemblyAi,
            model: "x".to_owned(),
            ..Default::default()
        };
        // `Box<dyn Transcriber>` is not Debug, so match instead of
        // `unwrap_err`.
        let Err(err) = build_backend(&opts) else {
            panic!("expected an error for assemblyai");
        };
        match err {
            TranscribeError::BackendUnsupported { backend } => assert_eq!(backend, "assemblyai"),
            other => panic!("expected BackendUnsupported, got {other:?}"),
        }
    }

    #[cfg(not(feature = "parakeet"))]
    #[test]
    fn parakeet_without_feature_reports_not_compiled() {
        let opts = TranscribeOpts {
            backend: Backend::Parakeet,
            model: "parakeet-tdt-0.6b-v3".to_owned(),
            ..Default::default()
        };
        let Err(err) = build_backend(&opts) else {
            panic!("expected BackendNotCompiled without the parakeet feature");
        };
        assert!(matches!(err, TranscribeError::BackendNotCompiled { .. }));
    }

    #[test]
    fn whisper_caps_choose_encoded_file() {
        let backend = whisper_cpp::WhisperCppLocal::new();
        let caps = backend.capabilities();
        let now = chrono::Utc::now();
        let src =
            AudioSource::from_encoded_file(Path::new("/clip.flac"), AudioCodec::Flac, 16_000, now);
        assert_eq!(
            choose_audio_input(&caps, &src).unwrap(),
            AudioInputKind::EncodedFile
        );
    }
}
