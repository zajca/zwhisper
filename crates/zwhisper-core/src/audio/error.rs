use std::borrow::Cow;
use std::path::PathBuf;

use thiserror::Error;

/// Errors raised by the audio façade. Variants are stable across the
/// M0 → M3 daemon split: M3 maps these to D-Bus error names verbatim.
#[derive(Debug, Error)]
#[allow(dead_code)] // Variants land as the pipeline is wired up in phase 3+.
pub enum RecordingError {
    #[error("device discovery failed: {0}")]
    DeviceDiscovery(#[source] DeviceError),

    #[error("device disappeared during recording: {node}")]
    DeviceDisappeared { node: String },

    #[error("pipeline failed at stage `{stage}`: {source}")]
    PipelineFailed {
        /// Stage labels are usually compile-time constants, but bus
        /// errors carry dynamic `GStreamer` element paths. `Cow` lets
        /// both reach the variant without `Box::leak` — important
        /// for the M3 daemon, which must not leak memory per error.
        stage: Cow<'static, str>,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("encoder failed: {0}")]
    EncoderFailed(String),

    #[error("EOS finalisation timed out after {seconds}s")]
    EosTimeout { seconds: u64 },

    #[error("output path `{path}` could not be opened with mode 0600: {source}")]
    OutputPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Errors specific to default-device resolution via `PipeWire` CLI
/// helpers (`wpctl`, `pw-cli`). Kept separate from `RecordingError`
/// so the discovery code can be unit tested without pulling in
/// `GStreamer` types. The `tool` field names which binary failed —
/// the discovery layer shells out to more than one, and conflating
/// them leaves the user chasing the wrong tool when something
/// breaks (e.g. `pw-cli` missing surfacing as "wpctl failed").
#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("`{tool}` invocation failed: {message}")]
    CommandFailed { tool: &'static str, message: String },

    #[error("`wpctl inspect {alias}` did not contain a `node.name` line — output:\n{output}")]
    NodeNameMissing { alias: String, output: String },

    #[error("invalid device argument `{value}`: {reason}")]
    InvalidArgument { value: String, reason: &'static str },
}
