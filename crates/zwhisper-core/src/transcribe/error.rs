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
        "model `{name}` not found at {}; download `ggml-{name}.bin` into \
         ~/.local/share/zwhisper/models/ (e.g. \
         `curl -L -o {} \
         https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin`)",
        expected.display(),
        expected.display()
    )]
    ModelNotFound { name: String, expected: PathBuf },

    #[error("invalid model name `{name}`: {reason}")]
    InvalidModelName { name: String, reason: &'static str },

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
