//! Transcription backend abstractions and implementations (M1+).
//!
//! `clippy::result_large_err` is intentionally allowed across this
//! module: [`TranscribeError`] is the public surface for typed
//! backend failures and grows by design as new backends land. The
//! callers who care about cheap returns in the hot path (the daemon
//! lifecycle and the CLI entry points) are not measurably affected
//! by the enum size — they map the error to the wire on the cold
//! path, then propagate. Boxing every variant just to silence the
//! lint pollutes pattern matching for no observable win.
#![allow(clippy::result_large_err)]

pub mod deepgram;
// M7 (DoD #18): `discovery` and `models` are now `pub` so the
// settings UI can call `discovery::detect_whisper_cli` and
// `models::{resolve_model, models_dir}` directly. The `Locator` and
// `ModelDirProvider` traits stay `pub(crate)`; only the free-function
// entry points cross the crate boundary.
pub mod discovery;
pub mod error;
pub mod models;
pub mod speakers;
pub(crate) mod whisper_cpp;

use std::path::{Path, PathBuf};
use std::time::Duration;

// Re-exports stay live even before M1 phase 3 wires them into the CLI
// surface — phase 3 imports them via `crate::transcribe::…`.
#[allow(unused_imports)]
pub(crate) use discovery::locate_whisper_cli;
#[allow(unused_imports)]
pub use error::TranscribeError;
pub use speakers::SpeakerSegment;

// M7 (DoD #18): convenience re-exports at the `transcribe` namespace.
// External callers may use either `zwhisper_core::transcribe::models::*`
// (preserved) or these shorthand paths.
pub use discovery::detect_whisper_cli;
pub use models::{models_dir, resolve_model};

use crate::profile::schema::DeepgramSettings;

/// Backend-specific configuration carried by [`TranscribeOpts`].
/// M5 introduces this enum as the typed alternative to the legacy
/// `backend: String` routing key — the goal is M6 to drop the
/// string and dispatch purely on this enum. Variants stay additive:
/// `AssemblyAI` / `OpenAI` lands as new variants without touching
/// this surface.
#[derive(Debug, Clone, Default)]
pub enum BackendConfig {
    #[default]
    WhisperCpp,
    Deepgram(DeepgramSettings),
}

impl BackendConfig {
    /// Routing key matching `[transcription].backend` strings in the
    /// profile schema. Used while the legacy
    /// [`TranscribeOpts::backend`] string field still exists.
    pub fn id(&self) -> &'static str {
        match self {
            Self::WhisperCpp => "whisper-cpp",
            Self::Deepgram(_) => "deepgram",
        }
    }
}

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
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // Phase 4 wires the constructor into run_transcribe.
pub struct TranscribeOpts {
    /// Backend identifier, e.g. `"whisper-cpp"`. M5 widens the set.
    /// **Deprecated for M6 removal**; new code routes via
    /// [`BackendConfig`]. Kept for one milestone so wire formats
    /// (D-Bus + CLI flags) stay stable.
    pub backend: String,
    /// Model name (no path, no `ggml-` prefix), e.g. `"small"`.
    pub model: String,
    /// ISO 639-1 language code or `"auto"`.
    pub language: String,
    /// Typed backend config. Defaults to [`BackendConfig::WhisperCpp`]
    /// so existing call sites compile unchanged.
    #[allow(dead_code)]
    pub backend_config: BackendConfig,
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
    /// Speaker-attributed utterances, when the backend produced them.
    /// `None` for backends without diarization (whisper.cpp). M5 adds
    /// this; whisper-cpp callers MUST pass `None` so the resulting
    /// `transcript.json` omits the `speakers` array entirely (kept
    /// out via `serde(skip_serializing_if = "Option::is_none")` at
    /// the JSON-writer site).
    pub speakers: Option<Vec<SpeakerSegment>>,
}

/// Backend-side trait. M1 ships a single implementation
/// (`whisper_cpp::WhisperCppLocal`); M5 adds cloud backends.
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

/// Public façade used by the CLI and daemon. Selects a backend by
/// looking at [`TranscribeOpts::backend_config`] first; falls back
/// to the legacy [`TranscribeOpts::backend`] string for cross-version
/// compatibility. Unknown ids surface
/// [`TranscribeError::BackendUnknown`] with the supported set.
#[allow(dead_code)] // Phase 4 wires this into run_transcribe.
pub async fn transcribe_file(
    audio: &Path,
    opts: &TranscribeOpts,
) -> Result<TranscriptArtifacts, TranscribeError> {
    // Prefer the typed `backend_config` enum; the legacy
    // `backend: String` field is checked only when the enum is at
    // its `Default` (`WhisperCpp`) and the user explicitly asked
    // for a different cloud backend via the legacy CLI surface.
    match &opts.backend_config {
        BackendConfig::Deepgram(settings) => {
            let backend = deepgram::DeepgramBatch::new(settings.clone());
            backend.transcribe_file(audio, opts).await
        }
        BackendConfig::WhisperCpp => match opts.backend.as_str() {
            // Legacy routing: a CLI/daemon caller built `TranscribeOpts`
            // with the default `backend_config` and only the string
            // field. Honour it so the M5 cutover does not break the
            // M3 wire format.
            "deepgram" => {
                let backend = deepgram::DeepgramBatch::new(DeepgramSettings::default());
                backend.transcribe_file(audio, opts).await
            }
            "whisper-cpp" | "" => {
                let backend = whisper_cpp::WhisperCppLocal::new();
                backend.transcribe_file(audio, opts).await
            }
            other => Err(TranscribeError::BackendUnknown {
                name: other.to_owned(),
                supported: vec!["whisper-cpp", "deepgram"],
            }),
        },
    }
}
