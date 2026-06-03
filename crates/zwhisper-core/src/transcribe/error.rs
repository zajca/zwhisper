//! Typed transcription errors. Variants are added incrementally per
//! M1-plan.md (Phase 2a/2b/3) so the public surface stays stable for
//! M3 D-Bus wiring.

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)] // Variants land as the pipeline is wired up in M1 phase 3+.
pub enum TranscribeError {
    #[error(
        "no whisper.cpp binary found; checked {searched:?}. Install whisper.cpp \
         (e.g. AUR `whisper.cpp` on Arch) or set ZWHISPER_WHISPER_CLI to its path"
    )]
    BackendUnavailable { searched: Vec<PathBuf> },

    #[error(
        "model `{name}` not found at {}; place `ggml-{name}.bin` there \
         or set ZWHISPER_MODELS_DIR to the shared whisper.cpp model directory \
         (e.g. `curl -L -o {} \
         https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin`)",
        expected.display(),
        expected.display()
    )]
    ModelNotFound { name: String, expected: PathBuf },

    #[error("invalid model name `{name}`: {reason}")]
    InvalidModelName { name: String, reason: &'static str },

    #[error("invalid model directory `{path}` from {env_var}: {reason}")]
    InvalidModelDir {
        env_var: &'static str,
        path: PathBuf,
        reason: &'static str,
    },

    #[error("invalid whisper.cpp option `{option}`: {reason}")]
    InvalidBackendOption {
        option: String,
        reason: &'static str,
    },

    #[error("failed to open audio file {}: {source}", path.display())]
    InputAudio {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Could not spawn the backend binary at all (binary missing,
    /// permission denied, etc.). Distinct from a backend that ran
    /// and failed — see [`Self::BackendExitedNonZero`].
    #[error("failed to spawn whisper.cpp at {}: {source}", tool.display())]
    BackendSpawn {
        tool: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The backend ran to completion but exited with a non-zero
    /// status. The full stderr is preserved for tracing emission;
    /// the `Display` impl truncates to keep terminal output sane.
    #[error(
        "{} exited with status {status}: {}",
        tool.display(),
        truncate_for_display(stderr)
    )]
    BackendExitedNonZero {
        tool: PathBuf,
        status: ExitStatus,
        stderr: String,
    },

    /// Backend exited 0 but did not produce one of the expected
    /// output files (`<stem>.txt` or `<stem>.json`).
    #[error("expected output file not produced: {}", path.display())]
    OutputMissing { path: PathBuf },

    /// Backend produced the file, but we could not move/copy it next
    /// to the audio file (filesystem error, e.g. EXDEV when the
    /// tempdir lives on a different mount).
    #[error("could not move output file {}: {source}", path.display())]
    OutputUnreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Unknown backend identifier passed via CLI. The supported set
    /// is included so the user can self-correct without consulting
    /// docs.
    #[error("unknown backend `{name}`; supported: {supported:?}")]
    BackendUnknown {
        name: String,
        supported: Vec<&'static str>,
    },

    /// `whisper.cpp` produced JSON whose shape did not match the
    /// schema we depend on (missing `transcription` array, wrong
    /// types, etc.). Distinct from [`Self::OutputUnreadable`]
    /// (filesystem error) and [`Self::OutputMissing`] (file not
    /// produced at all). Carries the originating path so users
    /// can inspect the offending JSON.
    #[error("whisper.cpp JSON output at {} has unexpected shape: {source}", path.display())]
    JsonShape {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    // ----- M5 cloud-backend variants (non-exhaustive growth area) -----
    /// Cloud backend resolution failed because no API key was found.
    /// Distinct from [`Self::BackendAuth`] (key found but rejected).
    #[error("backend `{backend}` is missing API key: {source}")]
    BackendKeyMissing {
        backend: &'static str,
        #[source]
        source: crate::secrets::SecretsError,
    },

    /// Cloud backend rejected the request as unauthorized — the
    /// resolved API key is invalid, expired, or insufficiently
    /// scoped.
    #[error(
        "backend `{backend}` rejected the request as unauthorized (HTTP {status}); \
         check that the API key is valid"
    )]
    BackendAuth { backend: &'static str, status: u16 },

    /// Cloud backend rejected the request because the project ran
    /// out of credit or hit a rate limit (HTTP 402 / 429). Caller
    /// can treat this as exhausted.
    #[error(
        "backend `{backend}` reports quota exhaustion (HTTP {status}{}): {message}",
        retry_after_s.map_or(String::new(), |s| format!(", retry-after {s}s"))
    )]
    BackendQuota {
        backend: &'static str,
        status: u16,
        retry_after_s: Option<u64>,
        message: String,
    },

    /// Network plumbing problem — DNS, TCP connect, TLS handshake.
    /// Surfaced after the retry budget has been exhausted.
    /// `source` is `Box`ed to keep [`TranscribeError`] under the
    /// clippy `result_large_err` threshold.
    #[error("backend `{backend}` network error: {source}")]
    BackendNetwork {
        backend: &'static str,
        #[source]
        source: Box<reqwest::Error>,
    },

    /// Per-call timeout elapsed before the backend produced a
    /// response. Total timeout is capped by the profile's
    /// `transcription.deepgram.timeout_s`.
    #[error("backend `{backend}` timed out after {timeout_s}s")]
    BackendTimeout {
        backend: &'static str,
        timeout_s: u64,
    },

    /// Backend returned a non-2xx response that does not fit
    /// [`Self::BackendAuth`] / [`Self::BackendQuota`]. Body is
    /// truncated to keep log lines bounded.
    #[error(
        "backend `{backend}` returned HTTP {status}: {}",
        truncate_for_display(body_excerpt)
    )]
    BackendBadResponse {
        backend: &'static str,
        status: u16,
        body_excerpt: String,
    },

    /// Misconfiguration caught before any network I/O — e.g., a
    /// non-https endpoint URL, an empty base URL, an unsupported
    /// model identifier. Always recoverable by editing the profile.
    #[error("backend `{backend}` configuration error: {message}")]
    BackendConfig {
        backend: &'static str,
        message: String,
    },

    /// Backend returned a 2xx but the JSON body did not match the
    /// shape we deserialise. Distinct from [`Self::BackendBadResponse`]
    /// because the HTTP layer succeeded.
    #[error("backend `{backend}` returned JSON with unexpected shape: {source}")]
    BackendJsonShape {
        backend: &'static str,
        #[source]
        source: serde_json::Error,
    },

    /// Local I/O failure while writing the transcript artifacts to
    /// disk after a successful backend call.
    #[error("backend `{backend}` failed to write artifact at {}: {source}", path.display())]
    ArtifactWrite {
        backend: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    // ----- RFC model boundary (registry + resolution) -----
    /// A [`crate::transcribe::model::ModelSpec`] failed registry-load
    /// validation: the name allow-list rejected a path-deriving field
    /// (`dir_name` / `expected_files` / `relative_path`), a URL was not
    /// HTTPS, or the kind/source pairing was incompatible. Caught before
    /// any resolution or install can act on a hostile spec (CWE-22).
    #[error("invalid model spec `{id}`: {reason}")]
    InvalidModelSpec { id: String, reason: String },

    /// A directory-bundle model is not installed, or is present but
    /// missing required files. Both are the same actionable problem.
    #[error(
        "model bundle `{id}` is not fully installed at {} (missing: {missing:?}); \
         install it with `zwhisper model install {id}`",
        dir.display()
    )]
    ModelBundleIncomplete {
        id: String,
        dir: PathBuf,
        missing: Vec<String>,
    },

    /// Failed to resolve the models directory itself.
    #[error("could not resolve models directory: {0}")]
    ModelResolution(String),

    /// The resolved model artifact's kind is not accepted by the
    /// selected backend.
    #[error("backend `{backend}` does not accept model kind `{kind}`")]
    UnsupportedModelKind {
        backend: &'static str,
        kind: &'static str,
    },

    /// The backend's expected ASR sample rate disagrees with the
    /// normalized PCM rate. The coordinator refuses to hand a backend
    /// PCM at the wrong rate (RFC: Cross-axis reconciliation).
    #[error(
        "ASR sample-rate mismatch: model expects {expected} Hz but normalized PCM is {actual} Hz"
    )]
    AsrRateMismatch { expected: u32, actual: u32 },

    /// No accepted audio representation could be produced for the
    /// backend from the given [`crate::transcribe::AudioSource`].
    #[error("no accepted audio input for backend (prefers `{backend_preferred}`): {reason}")]
    UnsupportedAudioInput {
        backend_preferred: &'static str,
        reason: &'static str,
    },

    /// The selected backend exists as a [`crate::profile::schema::Backend`]
    /// variant but its inference engine was not compiled into this
    /// build. The message names the Cargo feature that enables it.
    #[error("backend `{backend}` is not compiled in; rebuild with `--features {feature}`")]
    BackendNotCompiled {
        backend: &'static str,
        feature: &'static str,
    },

    /// The selected backend is a known [`crate::profile::schema::Backend`]
    /// variant that has no implementation yet (AssemblyAI / OpenAI).
    #[error("backend `{backend}` is not supported in this build")]
    BackendUnsupported { backend: String },

    /// A runtime PCM source (decode-from-artifact, temp-backed, etc.)
    /// failed while a PCM-preferring backend was pulling samples.
    #[error("PCM source error: {0}")]
    PcmSource(String),

    /// Decoding the persisted artifact into normalized PCM failed.
    #[error("failed to decode audio {}: {reason}", path.display())]
    AudioDecode { path: PathBuf, reason: String },
}

/// Truncate a stderr/stdout payload for embedding in `Display`
/// output. Keeps the first 4 KiB so terminal output stays readable
/// while the full body remains accessible via the variant fields
/// for `tracing` emission.
fn truncate_for_display(s: &str) -> String {
    const LIMIT: usize = 4096;
    if s.len() <= LIMIT {
        return s.to_owned();
    }
    // Find a char boundary at or before LIMIT so we never split a
    // multi-byte UTF-8 sequence.
    let mut cut = LIMIT;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… [truncated, full length {} bytes]", &s[..cut], s.len())
}
