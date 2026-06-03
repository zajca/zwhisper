//! In-process Parakeet backend (RFC Phase 5), via `transcribe-rs` +
//! ONNX Runtime. Compiled only under the `parakeet` Cargo feature.
//!
//! With both RFC boundaries in place, Parakeet is a normal local
//! backend with no special casing in the coordinator:
//!
//! ```text
//! AudioSource (PCM) + ModelArtifact::Directory -> transcribe-rs ParakeetModel
//!                                               -> zwhisper txt/json
//! ```
//!
//! It prefers normalized mono `f32` PCM. When a live PCM view exists it
//! is used directly; otherwise PCM is decoded from the persisted FLAC
//! artifact (the [`super::pcm_decode`] path) — so transcription works
//! after a restart, with PCM produced by the same normalization the
//! live capture branch uses.
//!
//! Parakeet v3 auto-detects language; manual language selection is not
//! exposed (the engine ignores it), so [`BackendCapabilities::languages`]
//! is "auto only" and the transcript language is reported as `auto`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;
use tracing::{debug, info};
use transcribe_rs::onnx::Quantization;
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams, TimestampGranularity};

use super::audio_source::{AudioSource, PcmAvailability};
use super::config::DEFAULT_ASR_SAMPLE_RATE_HZ;
use super::error::TranscribeError;
use super::model::ModelArtifact;
use super::pcm_decode::ArtifactDecodeSource;
use super::{
    AudioInputKind, BackendCapabilities, ModelKindTag, TranscribeOpts, Transcriber,
    TranscriptArtifacts,
};

const BACKEND_ID: &str = "parakeet";
/// Frames per pull when draining a chunk source into the contiguous
/// buffer `transcribe-rs` expects (1 s at the ASR rate).
const DRAIN_CHUNK_FRAMES: usize = 16_000;

/// The Parakeet engine. Stateless across calls — each `transcribe`
/// loads the model fresh on the blocking pool (ONNX session
/// construction + inference are synchronous and CPU/GPU-bound).
#[derive(Debug, Default)]
pub struct ParakeetLocal;

impl ParakeetLocal {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Transcriber for ParakeetLocal {
    fn id(&self) -> &'static str {
        BACKEND_ID
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            // Parakeet consumes normalized mono f32 PCM. It accepts the
            // chunked form too (drained into a buffer); it never reads
            // the encoded file directly.
            preferred_input: AudioInputKind::PcmBuffer,
            accepted_inputs: vec![AudioInputKind::PcmBuffer, AudioInputKind::PcmChunks],
            accepted_model_kinds: vec![ModelKindTag::DirectoryBundle],
            supports_streaming: false,
            supports_true_diarization: false,
            // v3 auto-detects; no manual language selection.
            languages: vec!["auto"],
        }
    }

    async fn transcribe(
        &self,
        audio: &AudioSource,
        model: &ModelArtifact,
        opts: &TranscribeOpts,
    ) -> Result<TranscriptArtifacts, TranscribeError> {
        let started_at = Instant::now();

        let model_dir = match model {
            ModelArtifact::Directory(p) => p.clone(),
            ModelArtifact::File(_) => {
                return Err(TranscribeError::UnsupportedModelKind {
                    backend: BACKEND_ID,
                    kind: "single-file",
                });
            }
            ModelArtifact::Remote { .. } => {
                return Err(TranscribeError::UnsupportedModelKind {
                    backend: BACKEND_ID,
                    kind: "remote",
                });
            }
        };

        // Normalized PCM rate the views are produced at (the coordinator
        // already reconciled this against the model's expected rate).
        let asr_rate = if audio.metadata.normalized_sample_rate_hz == 0 {
            DEFAULT_ASR_SAMPLE_RATE_HZ
        } else {
            audio.metadata.normalized_sample_rate_hz
        };

        let samples = collect_pcm(audio, asr_rate).await?;
        let frames = samples.len() as u64;
        let audio_duration = if asr_rate == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(frames as f64 / f64::from(asr_rate))
        };

        let quantization = detect_quantization(&model_dir);
        debug!(
            backend = BACKEND_ID,
            model_dir = %model_dir.display(),
            frames,
            quantization = ?quantization,
            "loading Parakeet model and running inference",
        );

        // Model load + inference are synchronous and heavy — run on the
        // blocking pool so the async runtime keeps moving.
        let result =
            tokio::task::spawn_blocking(move || run_inference(&model_dir, &quantization, &samples))
                .await
                .map_err(|join| {
                    TranscribeError::PcmSource(format!("parakeet task panicked: {join}"))
                })??;

        let language = "auto".to_owned();
        let audio_path = audio.artifact_path();
        let txt_target = append_extension(audio_path, ".txt");
        let json_target = append_extension(audio_path, ".json");

        write_text(&txt_target, &result.text).await?;

        let duration = started_at.elapsed();
        let envelope = ParakeetEnvelope {
            backend: BACKEND_ID,
            model: &opts.model,
            language: &language,
            audio_duration_s: audio_duration.as_secs_f64(),
            transcribe_duration_s: duration.as_secs_f64(),
            segments: result.segments.as_deref(),
        };
        write_json(&json_target, &envelope).await?;

        info!(
            backend = BACKEND_ID,
            txt = %txt_target.display(),
            json = %json_target.display(),
            audio_duration_ms = u64::try_from(audio_duration.as_millis()).unwrap_or(u64::MAX),
            wall_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            segments = result.segments.as_ref().map_or(0, Vec::len),
            "transcribe ok",
        );

        Ok(TranscriptArtifacts {
            txt_path: txt_target,
            json_path: json_target,
            duration,
            audio_duration,
            language,
            model: opts.model.clone(),
            // Parakeet does not produce speaker labels.
            speakers: None,
        })
    }
}

/// One transcription result lifted out of the `transcribe-rs` types so
/// the blocking closure returns an owned, `Send` value.
struct InferenceResult {
    text: String,
    segments: Option<Vec<Segment>>,
}

#[derive(Debug, Clone, Serialize)]
struct Segment {
    start: f32,
    end: f32,
    text: String,
}

/// Load the model and run inference. Synchronous; called on the blocking
/// pool. Maps `transcribe-rs` errors to a typed [`TranscribeError`].
fn run_inference(
    model_dir: &Path,
    quantization: &Quantization,
    samples: &[f32],
) -> Result<InferenceResult, TranscribeError> {
    let mut model =
        ParakeetModel::load(model_dir, quantization).map_err(|e| TranscribeError::AudioDecode {
            path: model_dir.to_path_buf(),
            reason: format!("parakeet model load failed: {e}"),
        })?;
    let params = ParakeetParams {
        // Ignored by the engine (v3 auto-detects language).
        language: None,
        timestamp_granularity: Some(TimestampGranularity::Segment),
    };
    let result = model
        .transcribe_with(samples, &params)
        .map_err(|e| TranscribeError::PcmSource(format!("parakeet inference failed: {e}")))?;
    let segments = result.segments.map(|segs| {
        segs.into_iter()
            .map(|s| Segment {
                start: s.start,
                end: s.end,
                text: s.text,
            })
            .collect()
    });
    Ok(InferenceResult {
        text: result.text,
        segments,
    })
}

/// Detect the model's quantization from the files present in the bundle
/// directory so the right ONNX file suffix is loaded.
fn detect_quantization(dir: &Path) -> Quantization {
    let has = |suffix: &str| dir.join(format!("encoder-model{suffix}.onnx")).is_file();
    if has(".int8") {
        Quantization::Int8
    } else if has(".int4") {
        Quantization::Int4
    } else if has(".fp16") {
        Quantization::FP16
    } else {
        Quantization::FP32
    }
}

/// Collect the AudioSource's PCM into one contiguous mono `f32` buffer
/// at `asr_rate`. Live PCM is used directly; otherwise PCM is decoded
/// from the persisted artifact.
async fn collect_pcm(audio: &AudioSource, asr_rate: u32) -> Result<Vec<f32>, TranscribeError> {
    match &audio.pcm {
        PcmAvailability::InMemory(buf) => Ok(buf.to_vec()),
        PcmAvailability::Chunked(src) => drain_chunks(src.as_ref()).await,
        PcmAvailability::DecodeFromArtifact => {
            let src =
                ArtifactDecodeSource::open(audio.artifact_path(), asr_rate, DRAIN_CHUNK_FRAMES)?;
            drain_chunks(&src).await
        }
        PcmAvailability::Unavailable => Err(TranscribeError::UnsupportedAudioInput {
            backend_preferred: "pcm-buffer",
            reason: "PCM is unavailable for this source (corrupt artifact or decode disabled)",
        }),
    }
}

async fn drain_chunks(
    src: &dyn super::audio_source::PcmChunkSource,
) -> Result<Vec<f32>, TranscribeError> {
    let mut out: Vec<f32> = Vec::with_capacity(src.total_frames().unwrap_or(0) as usize);
    while let Some(chunk) = src
        .next_chunk()
        .await
        .map_err(|e| TranscribeError::PcmSource(e.to_string()))?
    {
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
struct ParakeetEnvelope<'a> {
    backend: &'static str,
    model: &'a str,
    language: &'a str,
    audio_duration_s: f64,
    transcribe_duration_s: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    segments: Option<&'a [Segment]>,
}

fn append_extension(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    std::path::PathBuf::from(s)
}

async fn write_text(path: &Path, text: &str) -> Result<(), TranscribeError> {
    tokio::fs::write(path, text)
        .await
        .map_err(|source| TranscribeError::ArtifactWrite {
            backend: BACKEND_ID,
            path: path.to_path_buf(),
            source,
        })
}

async fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), TranscribeError> {
    let body =
        serde_json::to_vec_pretty(value).map_err(|source| TranscribeError::BackendJsonShape {
            backend: BACKEND_ID,
            source,
        })?;
    tokio::fs::write(path, body)
        .await
        .map_err(|source| TranscribeError::ArtifactWrite {
            backend: BACKEND_ID,
            path: path.to_path_buf(),
            source,
        })
}

// Keep `Arc` referenced even if a future refactor drops the direct use
// (the InMemory PCM view is `Arc<[f32]>`).
const _: fn() = || {
    fn _assert_arc(_: Arc<[f32]>) {}
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_prefer_pcm_and_directory_bundle() {
        let caps = ParakeetLocal::new().capabilities();
        assert_eq!(caps.preferred_input, AudioInputKind::PcmBuffer);
        assert!(
            caps.accepted_model_kinds
                .contains(&ModelKindTag::DirectoryBundle)
        );
        assert!(!caps.supports_true_diarization);
        assert_eq!(caps.languages, vec!["auto"]);
    }

    #[test]
    fn detect_quantization_prefers_int8_then_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        // No files → FP32 default.
        assert!(matches!(
            detect_quantization(dir.path()),
            Quantization::FP32
        ));
        std::fs::write(dir.path().join("encoder-model.int8.onnx"), b"x").unwrap();
        assert!(matches!(
            detect_quantization(dir.path()),
            Quantization::Int8
        ));
    }

    #[tokio::test]
    async fn collect_pcm_unavailable_is_typed_error() {
        let now = chrono::Utc::now();
        let mut src = AudioSource::from_encoded_file(
            Path::new("/x.flac"),
            super::super::AudioCodec::Flac,
            16_000,
            now,
        );
        src.pcm = PcmAvailability::Unavailable;
        let err = collect_pcm(&src, 16_000).await.unwrap_err();
        assert!(matches!(err, TranscribeError::UnsupportedAudioInput { .. }));
    }

    #[tokio::test]
    async fn collect_pcm_in_memory_passthrough() {
        let now = chrono::Utc::now();
        let mut src = AudioSource::from_encoded_file(
            Path::new("/x.flac"),
            super::super::AudioCodec::Flac,
            16_000,
            now,
        );
        let buf: Arc<[f32]> = Arc::from(vec![0.1_f32, 0.2, 0.3].into_boxed_slice());
        src.pcm = PcmAvailability::InMemory(buf);
        let got = collect_pcm(&src, 16_000).await.unwrap();
        assert_eq!(got, vec![0.1, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn collect_pcm_decodes_from_artifact() {
        // Reuse the committed FLAC fixture; DecodeFromArtifact should
        // run the symphonia decode path and return ~6400 frames.
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("audio")
            .join("sine-16k-mono.flac");
        let now = chrono::Utc::now();
        let src =
            AudioSource::from_encoded_file(&fixture, super::super::AudioCodec::Flac, 16_000, now);
        let got = collect_pcm(&src, 16_000).await.unwrap();
        assert!((6300..=6500).contains(&got.len()), "got {}", got.len());
    }
}
