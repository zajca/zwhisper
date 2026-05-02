//! Transcription backend abstractions and implementations (M1+).

pub(crate) mod discovery;
pub mod error;
pub(crate) mod models;
pub(crate) mod whisper_cpp;

use std::path::{Path, PathBuf};
use std::time::Duration;

// Re-exports stay live even before M1 phase 3 wires them into the CLI
// surface — phase 3 imports them via `crate::transcribe::…`.
#[allow(unused_imports)]
pub(crate) use discovery::locate_whisper_cli;
#[allow(unused_imports)]
pub use error::TranscribeError;

/// Static description of what a backend can do. Drives M2 profile
/// validation and M5 backend-selection logic; M1 only exposes a
/// single backend so this is mostly metadata for now.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Phase 4 wires these into the CLI status/diagnostics surface.
pub struct Capabilities {
    pub streaming: bool,
    pub true_diarization: bool,
    /// ISO 639-1 codes plus "auto"; empty Vec means "auto only".
    pub languages: Vec<&'static str>,
}

/// Plain-Rust input shape for [`transcribe_file`]. Stays free of
/// whisper.cpp-specific types so M5 can add cloud backends without
/// changing this struct.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Phase 4 wires the constructor into run_transcribe.
pub struct TranscribeOpts {
    /// Backend identifier, e.g. `"whisper-cpp"`. M5 widens the set.
    pub backend: String,
    /// Model name (no path, no `ggml-` prefix), e.g. `"small"`.
    pub model: String,
    /// ISO 639-1 language code or `"auto"`.
    pub language: String,
}

/// Result of a successful transcription. Mirrors the shape M3 will
/// emit on `TranscriptComplete(s session_id, s txt_path, s json_path)`
/// — adding fields here is cheap, removing them is a wire-format
/// break.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Phase 4 reads these fields when printing the CLI summary.
pub struct TranscriptArtifacts {
    pub txt_path: PathBuf,
    pub json_path: PathBuf,
    /// Wall-clock duration of the [`transcribe_file`] call.
    pub duration: Duration,
    /// FLAC duration the backend saw, parsed from the JSON's last
    /// segment offset. `Duration::ZERO` for empty transcripts.
    pub audio_duration: Duration,
    /// Resolved language (after autodetect, if any).
    pub language: String,
    /// Resolved model name (echoed back from `TranscribeOpts`).
    pub model: String,
}

/// Backend-side trait. M1 ships a single implementation
/// ([`whisper_cpp::WhisperCppLocal`]); M5 adds cloud backends.
#[async_trait::async_trait]
#[allow(dead_code)] // Phase 4 calls this through the public façade.
pub trait Transcriber: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    async fn transcribe_file(
        &self,
        audio: &Path,
        opts: &TranscribeOpts,
    ) -> Result<TranscriptArtifacts, TranscribeError>;
}

/// Public façade used by the CLI in Phase 4. Selects a backend by
/// id and dispatches; unknown ids surface
/// [`TranscribeError::BackendUnknown`] with the supported set.
#[allow(dead_code)] // Phase 4 wires this into run_transcribe.
pub async fn transcribe_file(
    audio: &Path,
    opts: &TranscribeOpts,
) -> Result<TranscriptArtifacts, TranscribeError> {
    match opts.backend.as_str() {
        "whisper-cpp" => {
            let backend = whisper_cpp::WhisperCppLocal::new();
            backend.transcribe_file(audio, opts).await
        }
        other => Err(TranscribeError::BackendUnknown {
            name: other.to_owned(),
            supported: vec!["whisper-cpp"],
        }),
    }
}
