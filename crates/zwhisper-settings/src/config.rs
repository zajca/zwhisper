//! M7 — `~/.config/zwhisper/models.toml` parser.
//!
//! Holds the user-configurable `base_url` for the model downloader.
//! Default points at the canonical HuggingFace mirror; the file
//! exists so power users can swap in a private mirror without
//! re-building the binary (DoD #12).
//!
//! Behaviour matrix (CLAUDE.md "no silent defaults"):
//! - File **absent** → return the built-in default. This is the
//!   "no override" case, not the "broken config" case.
//! - File **present and parses** → return parsed value.
//! - File **present and malformed** → return
//!   [`SettingsError::Config`]. Caller must show a banner and
//!   refuse downloads. We never silently fall back to the default
//!   when the user explicitly placed a file there: their intent
//!   was to override, and silently ignoring it would be a footgun.
//!
//! URL substitution rules:
//! - The URL must contain `{model}` exactly once.
//! - No other `{...}` placeholder is permitted (template-injection
//!   guard — a stray `{}` would let a maliciously-crafted model
//!   name reach the URL, e.g. via copy-paste).
//! - The URL must start with `https://` (security guard against
//!   plain-text MITM).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::SettingsError;

/// Built-in default — the current canonical HuggingFace mirror for
/// whisper.cpp ggml models.
const DEFAULT_BASE_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}.bin";

/// Required substitution token. Exactly one occurrence allowed.
const MODEL_PLACEHOLDER: &str = "{model}";

/// Required URL scheme prefix.
const REQUIRED_SCHEME: &str = "https://";

/// Subdirectory under `dirs::config_dir()` that holds zwhisper user
/// config files. Mirrors `zwhisper-core::profile::paths`.
const CONFIG_SUBDIR: &str = "zwhisper";

/// Filename of the models config inside [`CONFIG_SUBDIR`].
const CONFIG_FILENAME: &str = "models.toml";

/// Parsed `models.toml`. Public-via-`pub(crate)` so the Models tab
/// can read `base_url` and pass it into [`crate::download::ModelDownloader`].
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct ModelsConfig {
    /// Configurable base URL with `{model}` placeholder.
    pub(crate) base_url: String,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
        }
    }
}

impl ModelsConfig {
    /// Default platform path — `$XDG_CONFIG_HOME/zwhisper/models.toml`.
    /// Returns `None` only when `dirs::config_dir()` cannot resolve a
    /// home directory (e.g. in a sandbox without `$HOME`).
    pub(crate) fn default_path() -> Option<PathBuf> {
        Some(
            dirs::config_dir()?
                .join(CONFIG_SUBDIR)
                .join(CONFIG_FILENAME),
        )
    }

    /// Load from `path`. If `path` is `None`, use [`Self::default_path`].
    /// Missing file → built-in default. Malformed file → typed error.
    pub(crate) fn load_or_default(path: Option<&Path>) -> Result<Self, SettingsError> {
        let resolved: PathBuf = match path {
            Some(p) => p.to_path_buf(),
            None => match Self::default_path() {
                Some(p) => p,
                None => {
                    // No config home — same outcome as "file absent".
                    // We do not treat this as an error because the
                    // built-in default works without a home dir.
                    return Ok(Self::default());
                }
            },
        };

        if !resolved.is_file() {
            return Ok(Self::default());
        }

        let body = std::fs::read_to_string(&resolved)
            .map_err(|e| SettingsError::Config(format!("reading {}: {e}", resolved.display())))?;

        let parsed: Self = toml::from_str(&body)
            .map_err(|e| SettingsError::Config(format!("parsing {}: {e}", resolved.display())))?;

        // Validate at parse time so a malformed URL surfaces before
        // any download attempt.
        parsed.validate_base_url()?;
        Ok(parsed)
    }

    /// Substitute `{model}` and return the final URL. Fails the same
    /// way [`Self::validate_base_url`] does plus rejects empty model
    /// names (the manifest lookup should catch those first, but we
    /// double-check at the URL boundary).
    pub(crate) fn resolve_url(&self, model_name: &str) -> Result<String, SettingsError> {
        if model_name.is_empty() {
            return Err(SettingsError::Config("model name must not be empty".into()));
        }
        self.validate_base_url()?;
        Ok(self.base_url.replace(MODEL_PLACEHOLDER, model_name))
    }

    /// Reject URLs that would silently MITM (`http://`), contain no
    /// `{model}` placeholder (download would 404 or hit the same URL
    /// for every model), contain multiple `{model}` tokens
    /// (substitution semantics ambiguous), or contain other `{...}`
    /// braces (template-injection guard).
    fn validate_base_url(&self) -> Result<(), SettingsError> {
        let url = &self.base_url;

        if !url.starts_with(REQUIRED_SCHEME) {
            return Err(SettingsError::Config(format!(
                "base_url must start with {REQUIRED_SCHEME}; got {url:?}"
            )));
        }

        let placeholder_count = url.matches(MODEL_PLACEHOLDER).count();
        if placeholder_count == 0 {
            return Err(SettingsError::Config(format!(
                "base_url must contain {MODEL_PLACEHOLDER} placeholder"
            )));
        }
        if placeholder_count > 1 {
            return Err(SettingsError::Config(format!(
                "base_url must contain {MODEL_PLACEHOLDER} exactly once \
                 (found {placeholder_count} occurrences)"
            )));
        }

        // Reject any other `{...}` placeholder. We strip the one
        // legitimate `{model}` first, then look for stray `{`.
        let stripped = url.replace(MODEL_PLACEHOLDER, "");
        if stripped.contains('{') || stripped.contains('}') {
            return Err(SettingsError::Config(format!(
                "base_url contains placeholders other than \
                 {MODEL_PLACEHOLDER}; only one is allowed"
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_config(dir: &TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("models.toml");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn defaults_to_huggingface_url_when_file_absent() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist.toml");
        let cfg = ModelsConfig::load_or_default(Some(&missing)).unwrap();
        assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn loads_user_supplied_url() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            &tmp,
            r#"base_url = "https://example.com/models/ggml-{model}.bin""#,
        );
        let cfg = ModelsConfig::load_or_default(Some(&path)).unwrap();
        assert_eq!(cfg.base_url, "https://example.com/models/ggml-{model}.bin");
    }

    #[test]
    fn base_url_substitutes_model_name() {
        let cfg = ModelsConfig::default();
        let url = cfg.resolve_url("large-v3").unwrap();
        assert_eq!(
            url,
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin"
        );
    }

    #[test]
    fn malformed_toml_falls_back_with_typed_error() {
        // Per CLAUDE.md "no silent defaults": a *broken* config must
        // produce a typed Err, not silently return the default.
        let tmp = TempDir::new().unwrap();
        let path = write_config(&tmp, "this is not valid toml = =");
        let err = ModelsConfig::load_or_default(Some(&path)).unwrap_err();
        match err {
            SettingsError::Config(msg) => {
                assert!(
                    msg.contains("parsing"),
                    "expected parse-stage error, got: {msg}"
                );
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_url_rejects_http_only_urls() {
        let cfg = ModelsConfig {
            base_url: "http://example.com/{model}.bin".into(),
        };
        let err = cfg.resolve_url("tiny").unwrap_err();
        assert!(matches!(err, SettingsError::Config(msg) if msg.contains("https://")));
    }

    #[test]
    fn resolve_url_rejects_extra_placeholders() {
        let cfg = ModelsConfig {
            base_url: "https://example.com/{user}/ggml-{model}.bin".into(),
        };
        let err = cfg.resolve_url("tiny").unwrap_err();
        match err {
            SettingsError::Config(msg) => {
                assert!(
                    msg.contains("placeholders") || msg.contains("placeholder"),
                    "expected placeholder error, got: {msg}"
                );
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_url_rejects_duplicate_model_tokens() {
        let cfg = ModelsConfig {
            base_url: "https://example.com/{model}/ggml-{model}.bin".into(),
        };
        let err = cfg.resolve_url("tiny").unwrap_err();
        assert!(matches!(err, SettingsError::Config(msg) if msg.contains("exactly once")));
    }

    #[test]
    fn resolve_url_rejects_missing_placeholder() {
        let cfg = ModelsConfig {
            base_url: "https://example.com/static.bin".into(),
        };
        let err = cfg.resolve_url("tiny").unwrap_err();
        assert!(matches!(err, SettingsError::Config(msg) if msg.contains("{model}")));
    }

    #[test]
    fn resolve_url_rejects_empty_model_name() {
        let cfg = ModelsConfig::default();
        let err = cfg.resolve_url("").unwrap_err();
        assert!(matches!(err, SettingsError::Config(_)));
    }

    #[test]
    fn malformed_url_in_file_surfaces_at_load_time() {
        // A file present but with an http:// URL should fail at
        // load time, not be deferred to the first resolve_url call.
        let tmp = TempDir::new().unwrap();
        let path = write_config(&tmp, r#"base_url = "http://example.com/ggml-{model}.bin""#);
        let err = ModelsConfig::load_or_default(Some(&path)).unwrap_err();
        assert!(matches!(err, SettingsError::Config(_)));
    }

    #[test]
    fn default_path_uses_zwhisper_subdir() {
        // Sanity: the path layout matches the rest of the project
        // (~/.config/zwhisper/...). We do not assert on $HOME because
        // CI may set XDG_CONFIG_HOME explicitly.
        if let Some(p) = ModelsConfig::default_path() {
            let display = p.display().to_string();
            assert!(
                display.ends_with("zwhisper/models.toml"),
                "unexpected default path: {display}"
            );
        }
    }
}
