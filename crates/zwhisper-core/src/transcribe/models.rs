//! Whisper model discovery and metadata (GGUF/GGML files on disk).
//!
//! `resolve_model` validates a user-supplied model name and resolves
//! it to a path under `$XDG_DATA_HOME/zwhisper/models/ggml-<name>.bin`
//! (or the platform equivalent via `dirs::data_local_dir()`).
//!
//! The resolver is wrapped behind a [`ModelDirProvider`] trait so
//! tests inject an isolated tempdir without touching `$HOME` or
//! `$XDG_DATA_HOME`. Production wires up [`RealModelDirProvider`].

// Surface is consumed by the runner that lands in M1 phase 3.
// Until then nothing calls `resolve_model` from main.rs — the unit
// tests exercise every branch via `resolve_with`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use super::error::TranscribeError;

/// Indirection over the XDG/data-dir lookup so the resolver can be
/// unit tested without touching process state.
pub(crate) trait ModelDirProvider {
    /// Returns the platform "local data" dir — i.e. the parent of
    /// `zwhisper/models`. On Linux this is typically
    /// `$XDG_DATA_HOME` (defaulting to `~/.local/share`).
    fn data_local_dir(&self) -> Option<PathBuf>;
}

/// Production [`ModelDirProvider`] backed by `dirs::data_local_dir`.
#[derive(Debug, Default)]
pub(crate) struct RealModelDirProvider;

impl ModelDirProvider for RealModelDirProvider {
    fn data_local_dir(&self) -> Option<PathBuf> {
        dirs::data_local_dir()
    }
}

/// Validate `name` against the allow-list `[A-Za-z0-9._-]+` plus a
/// few rejected literals (`""`, `auto`). On success the validated
/// name is returned unchanged so the caller can format it.
fn validate_name(name: &str) -> Result<(), TranscribeError> {
    if name.is_empty() {
        return Err(TranscribeError::InvalidModelName {
            name: name.to_owned(),
            reason: "model name must not be empty",
        });
    }

    if name == "auto" {
        return Err(TranscribeError::InvalidModelName {
            name: name.to_owned(),
            reason: "`auto` is reserved for language autodetect, not a model name",
        });
    }

    // Char-by-char allow-list: ASCII alphanumerics plus `.`, `_`, `-`.
    // Rejects `/`, `\`, `..` traversal sequences (the `.` is allowed
    // but path separators are not, so `../etc/passwd` fails on `/`),
    // spaces, `:`, and any other punctuation.
    for ch in name.chars() {
        let allowed = ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-';
        if !allowed {
            return Err(TranscribeError::InvalidModelName {
                name: name.to_owned(),
                reason: "model name contains forbidden characters; allowed: A-Z a-z 0-9 . _ -",
            });
        }
    }

    Ok(())
}

/// Resolve `name` to `<data_local_dir>/zwhisper/models/ggml-<name>.bin`.
///
/// Validation order:
/// 1. Empty name → `InvalidModelName`.
/// 2. Reserved name `auto` → `InvalidModelName`.
/// 3. Char outside `[A-Za-z0-9._-]` → `InvalidModelName`.
/// 4. `data_local_dir()` returns `None` → `InvalidModelName` (rare;
///    surfaces a misconfiguration rather than a silent fallback).
/// 5. File missing → `ModelNotFound { expected }`.
pub(crate) fn resolve_with<P: ModelDirProvider>(
    provider: &P,
    name: &str,
) -> Result<PathBuf, TranscribeError> {
    validate_name(name)?;

    let Some(data_dir) = provider.data_local_dir() else {
        return Err(TranscribeError::InvalidModelName {
            name: name.to_owned(),
            reason: "cannot resolve XDG data dir; set $XDG_DATA_HOME",
        });
    };

    let expected = data_dir
        .join("zwhisper")
        .join("models")
        .join(format!("ggml-{name}.bin"));

    if !Path::is_file(&expected) {
        return Err(TranscribeError::ModelNotFound {
            name: name.to_owned(),
            expected,
        });
    }

    Ok(expected)
}

/// Production entry point — wires up [`RealModelDirProvider`].
///
/// M7 (DoD #18): promoted from `pub(crate)` to `pub` so
/// `zwhisper-settings` can validate that a downloaded model lands at
/// the path the runtime resolver will read from.
pub fn resolve_model(name: &str) -> Result<PathBuf, TranscribeError> {
    resolve_with(&RealModelDirProvider, name)
}

/// M7 (DoD #18): public thin wrapper that returns the absolute
/// `<data_local_dir>/zwhisper/models` directory `resolve_model` reads
/// from. Wraps the crate-private `RealModelDirProvider` so external
/// crates (notably `zwhisper-settings`) can compute download
/// destinations without exposing the [`ModelDirProvider`] trait.
///
/// Returns the same `InvalidModelName` error variant the resolver
/// itself surfaces when `dirs::data_local_dir()` is unavailable, with
/// an empty `name` field to signal that the failure is global, not
/// per-model.
pub fn models_dir() -> Result<PathBuf, TranscribeError> {
    let Some(data_dir) = RealModelDirProvider.data_local_dir() else {
        return Err(TranscribeError::InvalidModelName {
            name: String::new(),
            reason: "cannot resolve XDG data dir; set $XDG_DATA_HOME",
        });
    };
    Ok(data_dir.join("zwhisper").join("models"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    /// Test [`ModelDirProvider`] returning a fixed tempdir path.
    struct MockModelDirProvider {
        dir: PathBuf,
    }

    impl ModelDirProvider for MockModelDirProvider {
        fn data_local_dir(&self) -> Option<PathBuf> {
            Some(self.dir.clone())
        }
    }

    /// Provider that returns `None` to exercise the misconfiguration
    /// branch.
    struct NoneProvider;

    impl ModelDirProvider for NoneProvider {
        fn data_local_dir(&self) -> Option<PathBuf> {
            None
        }
    }

    /// Build a tempdir, materialise `zwhisper/models/`, optionally
    /// touch a model file, and return the dir + provider.
    fn make_provider(model_files: &[&str]) -> (TempDir, MockModelDirProvider) {
        let tmp = TempDir::new().unwrap();
        let models_dir = tmp.path().join("zwhisper").join("models");
        fs::create_dir_all(&models_dir).unwrap();
        for file in model_files {
            fs::write(models_dir.join(file), b"").unwrap();
        }
        let provider = MockModelDirProvider {
            dir: tmp.path().to_path_buf(),
        };
        (tmp, provider)
    }

    #[test]
    fn valid_name_resolves_to_expected_path() {
        let (tmp, provider) = make_provider(&["ggml-small.bin"]);
        let resolved = resolve_with(&provider, "small").unwrap();
        assert_eq!(
            resolved,
            tmp.path()
                .join("zwhisper")
                .join("models")
                .join("ggml-small.bin")
        );
    }

    #[test]
    fn auto_name_rejected() {
        let (_tmp, provider) = make_provider(&[]);
        let err = resolve_with(&provider, "auto").unwrap_err();
        match err {
            TranscribeError::InvalidModelName { name, reason } => {
                assert_eq!(name, "auto");
                assert!(
                    reason.contains("auto"),
                    "expected reason to mention `auto`, got: {reason}"
                );
            }
            other => panic!("expected InvalidModelName, got {other:?}"),
        }
    }

    #[test]
    fn traversal_rejected_via_dotdot() {
        // `..` itself is two dots (allowed chars) but the `/` is not.
        let (_tmp, provider) = make_provider(&[]);
        let err = resolve_with(&provider, "../etc/passwd").unwrap_err();
        assert!(matches!(err, TranscribeError::InvalidModelName { .. }));
    }

    #[test]
    fn traversal_rejected_via_slash() {
        let (_tmp, provider) = make_provider(&[]);
        let err = resolve_with(&provider, "small/medium").unwrap_err();
        assert!(matches!(err, TranscribeError::InvalidModelName { .. }));
    }

    #[test]
    fn empty_name_rejected() {
        let (_tmp, provider) = make_provider(&[]);
        let err = resolve_with(&provider, "").unwrap_err();
        match err {
            TranscribeError::InvalidModelName { name, reason } => {
                assert_eq!(name, "");
                assert!(reason.contains("empty"));
            }
            other => panic!("expected InvalidModelName, got {other:?}"),
        }
    }

    #[test]
    fn forbidden_chars_rejected() {
        let (_tmp, provider) = make_provider(&[]);
        for bad in ["small!", "small medium", "small:tag", "small\\medium"] {
            let err = resolve_with(&provider, bad).unwrap_err();
            assert!(
                matches!(err, TranscribeError::InvalidModelName { .. }),
                "expected InvalidModelName for {bad:?}"
            );
        }
    }

    #[test]
    fn valid_name_but_missing_file_returns_not_found() {
        let (tmp, provider) = make_provider(&[]);
        let err = resolve_with(&provider, "medium").unwrap_err();
        match err {
            TranscribeError::ModelNotFound { name, expected } => {
                assert_eq!(name, "medium");
                assert_eq!(
                    expected,
                    tmp.path()
                        .join("zwhisper")
                        .join("models")
                        .join("ggml-medium.bin")
                );
            }
            other => panic!("expected ModelNotFound, got {other:?}"),
        }
    }

    #[test]
    fn name_with_dots_and_dashes_passes() {
        let (tmp, provider) = make_provider(&["ggml-large-v3.bin"]);
        let resolved = resolve_with(&provider, "large-v3").unwrap();
        assert_eq!(
            resolved,
            tmp.path()
                .join("zwhisper")
                .join("models")
                .join("ggml-large-v3.bin")
        );
    }

    #[test]
    fn name_with_underscores_passes() {
        let (tmp, provider) = make_provider(&["ggml-small_en.bin"]);
        let resolved = resolve_with(&provider, "small_en").unwrap();
        assert_eq!(
            resolved,
            tmp.path()
                .join("zwhisper")
                .join("models")
                .join("ggml-small_en.bin")
        );
    }

    #[test]
    fn caller_does_not_double_prefix_ggml() {
        // We do NOT strip a leading "ggml-" prefix; that's the
        // caller's responsibility. Document the literal behaviour
        // so a future contributor doesn't add silent stripping.
        let (tmp, provider) = make_provider(&["ggml-ggml-small.bin"]);
        let resolved = resolve_with(&provider, "ggml-small").unwrap();
        assert_eq!(
            resolved,
            tmp.path()
                .join("zwhisper")
                .join("models")
                .join("ggml-ggml-small.bin")
        );
    }

    #[test]
    fn missing_data_dir_surfaces_invalid_model_name() {
        let err = resolve_with(&NoneProvider, "small").unwrap_err();
        match err {
            TranscribeError::InvalidModelName { name, reason } => {
                assert_eq!(name, "small");
                assert!(reason.contains("XDG"));
            }
            other => panic!("expected InvalidModelName, got {other:?}"),
        }
    }
}
