use std::io;
use std::path::PathBuf;

use thiserror::Error;

use super::schema::Mode;

/// Identifiers `[transcription].backend` accepts in M5. The slice is
/// re-exported by `BackendUnknown` so users see exactly which set is
/// supported in this build. M2 shipped with `whisper-cpp` only; M5
/// adds the `deepgram` cloud backend per IDEA.md § 4.
pub const SUPPORTED_BACKENDS_M5: &[&str] = &["whisper-cpp", "deepgram"];


/// Errors surfaced by the profile module. Each variant maps to one
/// failure class so the CLI / future daemon can dispatch on them
/// without parsing display strings.
#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("profile {name:?} not found (searched: {searched:?})")]
    NotFound { name: String, searched: Vec<String> },

    #[error("invalid profile name {name:?}: only [A-Za-z0-9._-]+ allowed")]
    InvalidName { name: String },

    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not parse TOML in {path}: {source}")]
    TomlParse {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },

    #[error("could not deserialize profile in {path}: {source}")]
    TomlDeserialize {
        path: PathBuf,
        #[source]
        source: toml_edit::de::Error,
    },

    #[error(
        "{path} is missing top-level `schema_version = N` (integer); \
         legacy profiles must be migrated explicitly"
    )]
    MissingSchemaVersion { path: PathBuf },

    #[error(
        "{path} declares schema_version = {found}, this build supports up to {current}; \
         please upgrade zwhisper"
    )]
    UnsupportedSchemaVersion {
        path: PathBuf,
        found: u32,
        current: u32,
    },

    #[error("migration {from} -> {to} failed for {path}: {source}")]
    MigrationFailed {
        path: PathBuf,
        from: u32,
        to: u32,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("could not write backup for {path}: {source}")]
    BackupFailed {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("validation failed for profile {profile:?}: {message}")]
    Validation { profile: String, message: String },

    #[error(
        "mode {mode:?} is not implemented in this build (M2 ships mono_mix only); \
         see IDEA.md § 11 for the stereo_split roadmap"
    )]
    UnsupportedMode { mode: Mode },

    #[error(
        "transcription backend {backend:?} is not supported in this build \
         (supported: {supported:?})"
    )]
    BackendUnknown {
        backend: String,
        supported: &'static [&'static str],
    },

    #[error("refusing to overwrite {path}; remove it manually or pick a different name")]
    OverwriteRefused { path: PathBuf },
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn display_invalid_name_mentions_charset() {
        let e = ProfileError::InvalidName {
            name: "../etc/passwd".into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("../etc/passwd"));
        assert!(msg.contains("[A-Za-z0-9._-]+"));
    }

    #[test]
    fn display_unsupported_schema_version_includes_versions() {
        let e = ProfileError::UnsupportedSchemaVersion {
            path: PathBuf::from("/tmp/x.toml"),
            found: 99,
            current: 1,
        };
        let msg = e.to_string();
        assert!(msg.contains("99"));
        assert!(msg.contains("up to 1"));
    }

    #[test]
    fn supported_backends_m5_includes_whisper_cpp_and_deepgram() {
        assert_eq!(SUPPORTED_BACKENDS_M5, &["whisper-cpp", "deepgram"]);
    }
}
