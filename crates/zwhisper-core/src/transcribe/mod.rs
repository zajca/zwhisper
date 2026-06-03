//! Transcription backend abstractions and implementations (M1+, RFC).
//!
//! `clippy::result_large_err` is intentionally allowed across this
//! module: [`TranscribeError`] is the public surface for typed backend
//! failures and grows by design as new backends land. The callers who
//! care about cheap returns in the hot path (the daemon lifecycle and
//! the CLI entry points) are not measurably affected by the enum size —
//! they map the error to the wire on the cold path, then propagate.
//! Boxing every variant just to silence the lint pollutes pattern
//! matching for no observable win.
//!
//! ## Architecture (docs/RFC-audio-source-model.md)
//!
//! Two backend-agnostic boundaries replace the old `&Path` + `String`
//! model:
//!
//! - [`audio_source::AudioSource`] — the audio a backend consumes
//!   (persisted artifact + runtime PCM views + normalized metadata).
//! - [`model::ModelArtifact`] — the resolved model a backend loads,
//!   derived from a [`model::ModelSpec`] in the [`registry`].
//!
//! Backends declare what they accept on each axis via
//! [`BackendCapabilities`]; the [`coordinator`] resolves the concrete
//! audio source and model artifact, reconciles the one cross-axis
//! invariant (ASR sample rate), and invokes the backend. The legacy
//! free function [`transcribe_file`] survives as a thin compatibility
//! facade over the coordinator.
#![allow(clippy::result_large_err)]

pub mod archive_extract;
pub mod audio_source;
pub mod bundle_download;
pub mod config;
pub mod coordinator;
pub mod deepgram;
pub mod discovery;
pub mod error;
pub mod model;
pub mod model_management;
pub mod models;
#[cfg(feature = "parakeet")]
pub mod parakeet;
pub mod pcm_decode;
pub mod registry;
pub mod speakers;
pub(crate) mod whisper_cpp;

use std::path::PathBuf;
use std::time::Duration;

#[allow(unused_imports)]
pub(crate) use discovery::locate_whisper_cli;
#[allow(unused_imports)]
pub use error::TranscribeError;
pub use speakers::SpeakerSegment;

// M7 (DoD #18): convenience re-exports at the `transcribe` namespace.
pub use discovery::detect_whisper_cli;
pub use models::{models_dir, resolve_model};

// RFC core types, re-exported at the `transcribe` namespace.
pub use audio_source::{
    AudioArtifact, AudioCodec, AudioMetadata, AudioSource, PcmAvailability, PcmChunkSource,
    PcmFormat, PcmSourceError,
};
pub use bundle_download::{BundleError, BundleInstaller, BundleProgress};
pub use coordinator::{transcribe_file, transcribe_source};
pub use model::{
    ModelArtifact, ModelKind, ModelKindTag, ModelRegistry, ModelSource, ModelSpec, ModelStatus,
    RemoteFile, RuntimeMeta,
};
pub use pcm_decode::{ArtifactDecodeSource, ArtifactProbe, decode_flac_normalized};

use crate::profile::schema::{Backend, DeepgramSettings, WhisperCppSettings};

/// Typed, per-backend settings keyed by backend identity — the side map
/// that replaces the old `BackendConfig` enum. The RFC's "Backend Enum
/// Convergence" step: there is exactly one canonical backend enum
/// ([`Backend`]); the typed settings live here instead of in a parallel
/// enum that drifts from it.
#[derive(Debug, Clone, Default)]
pub struct BackendSettings {
    pub whisper_cpp: Option<WhisperCppSettings>,
    pub deepgram: Option<DeepgramSettings>,
}

/// The audio representation a backend consumes on the audio axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioInputKind {
    EncodedFile,
    PcmBuffer,
    PcmChunks,
}

impl AudioInputKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::EncodedFile => "encoded-file",
            Self::PcmBuffer => "pcm-buffer",
            Self::PcmChunks => "pcm-chunks",
        }
    }
}

/// Static description of what a backend accepts and prefers on both
/// axes. Drives coordinator routing (audio + model-kind matching) and
/// the diagnostics surface.
#[derive(Debug, Clone)]
pub struct BackendCapabilities {
    pub preferred_input: AudioInputKind,
    pub accepted_inputs: Vec<AudioInputKind>,
    /// Model kinds this backend can load. The coordinator checks the
    /// resolved [`ModelArtifact`] against this set via `tag()`.
    pub accepted_model_kinds: Vec<ModelKindTag>,
    pub supports_streaming: bool,
    pub supports_true_diarization: bool,
    /// ISO 639-1 codes plus "auto"; empty Vec means "auto only".
    pub languages: Vec<&'static str>,
}

impl BackendCapabilities {
    pub fn accepts_model_kind(&self, kind: ModelKindTag) -> bool {
        self.accepted_model_kinds.contains(&kind)
    }

    pub fn accepts_input(&self, kind: AudioInputKind) -> bool {
        self.accepted_inputs.contains(&kind)
    }
}

/// Plain-Rust input shape carried to the coordinator and backends.
/// `backend` is the canonical [`Backend`] enum; per-backend tuning lives
/// in [`BackendSettings`]. The legacy `backend: String` /
/// `backend_config: BackendConfig` pair is gone (RFC: Backend Enum
/// Convergence).
#[derive(Debug, Clone)]
pub struct TranscribeOpts {
    pub backend: Backend,
    /// Human-friendly model id, resolved through the [`registry`]
    /// against `backend`. The same field is no longer overloaded with
    /// three different semantics.
    pub model: String,
    /// ISO 639-1 language code or `"auto"`.
    pub language: String,
    /// Typed per-backend settings (side map keyed by `Backend`).
    pub settings: BackendSettings,
}

impl Default for TranscribeOpts {
    fn default() -> Self {
        Self {
            backend: Backend::WhisperCpp,
            model: String::new(),
            language: "auto".to_owned(),
            settings: BackendSettings::default(),
        }
    }
}

impl TranscribeOpts {
    /// The whisper.cpp settings, defaulted when the side map omits them.
    pub fn whisper_cpp_settings(&self) -> WhisperCppSettings {
        self.settings.whisper_cpp.clone().unwrap_or_default()
    }

    /// The Deepgram settings, defaulted when the side map omits them.
    pub fn deepgram_settings(&self) -> DeepgramSettings {
        self.settings.deepgram.clone().unwrap_or_default()
    }
}

/// Result of a successful transcription. Mirrors the M3 wire shape
/// (`TranscriptComplete`). Adding fields is cheap; removing them is a
/// wire-format break.
#[derive(Debug, Clone)]
pub struct TranscriptArtifacts {
    pub txt_path: PathBuf,
    pub json_path: PathBuf,
    /// Wall-clock duration of the transcribe call.
    pub duration: Duration,
    /// Audio duration the backend saw.
    pub audio_duration: Duration,
    /// Resolved language (after autodetect, if any).
    pub language: String,
    /// Resolved model id.
    pub model: String,
    /// Speaker-attributed utterances, when the backend produced them.
    pub speakers: Option<Vec<SpeakerSegment>>,
}

/// Backend-side trait (RFC: Backend Interface). Backends receive a
/// structured [`AudioSource`] and a resolved [`ModelArtifact`] instead
/// of a bare path + string; the coordinator does all resolution.
#[async_trait::async_trait]
pub trait Transcriber: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> BackendCapabilities;

    async fn transcribe(
        &self,
        audio: &AudioSource,
        model: &ModelArtifact,
        opts: &TranscribeOpts,
    ) -> Result<TranscriptArtifacts, TranscribeError>;
}
