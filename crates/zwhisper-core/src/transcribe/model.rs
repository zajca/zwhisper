//! Backend-agnostic model source model (RFC: Model Source Model).
//!
//! The second first-class boundary. It replaces the implicit
//! `model == ggml-<name>.bin` assumption with a registry so single-file,
//! directory-bundle, and remote models share one resolution path, one
//! download/verify path, and one status surface instead of per-backend
//! branching.
//!
//! Two orthogonal axes, never duplicated:
//!
//! - [`ModelKind`] is the **shape** axis — what on-disk (or off-disk)
//!   shape the model takes. `expected_files` (the bundle-completeness
//!   contract) lives ONLY here.
//! - [`ModelSource`] is the **transport** axis — how the bytes are
//!   fetched and verified, if at all. It never restates `expected_files`.
//!
//! A [`ModelSpec`] pairs one `ModelKind` with one `ModelSource`; the
//! registry checks they are compatible and that every path-deriving
//! field passes the name allow-list at **registry-load time** (before
//! any resolution or install can act on a hostile spec — CWE-22).

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use crate::profile::schema::Backend;

use super::error::TranscribeError;

/// Discriminant-only companion to [`ModelKind`]. This is the ONLY
/// backend-vs-kind matching key: backends declare accepted tags and the
/// coordinator compares against `kind.tag()`. No separate
/// hand-maintained enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKindTag {
    SingleFile,
    DirectoryBundle,
    Remote,
}

/// What kind of on-disk (or off-disk) shape a model takes — the SHAPE
/// axis only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelKind {
    /// One weights file, e.g. whisper.cpp `ggml-<id>.bin`.
    SingleFile { file_name: String },
    /// A directory bundle, e.g. Parakeet `<dir>/{encoder,decoder,…}`.
    /// `expected_files` is the single source of truth for "what makes
    /// this bundle complete" — [`ModelSource`] does not restate it.
    DirectoryBundle {
        dir_name: String,
        /// Files that MUST exist for the bundle to be considered
        /// installed. Validated both post-extract and on status checks.
        /// Every entry must pass the model-name allow-list at
        /// registry-load time.
        expected_files: Vec<String>,
    },
    /// No local artifact; the model lives behind a provider API.
    Remote,
}

impl ModelKind {
    pub fn tag(&self) -> ModelKindTag {
        match self {
            Self::SingleFile { .. } => ModelKindTag::SingleFile,
            Self::DirectoryBundle { .. } => ModelKindTag::DirectoryBundle,
            Self::Remote => ModelKindTag::Remote,
        }
    }
}

/// The concrete, resolved artifact handed to a backend. Mirrors the
/// `AudioSource` pattern: the coordinator resolves this before invoking
/// a backend, so backends never re-derive paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelArtifact {
    File(PathBuf),
    Directory(PathBuf),
    Remote { id: String },
}

impl ModelArtifact {
    pub fn tag(&self) -> ModelKindTag {
        match self {
            Self::File(_) => ModelKindTag::SingleFile,
            Self::Directory(_) => ModelKindTag::DirectoryBundle,
            Self::Remote { .. } => ModelKindTag::Remote,
        }
    }
}

/// One file inside a [`ModelSource::MultiFile`] bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    /// Install destination relative to the bundle directory. Validated
    /// against the name allow-list at registry-load time.
    pub relative_path: String,
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// How a model is fetched and verified, if at all — the TRANSPORT axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSource {
    /// Cloud/remote model: nothing to download or verify locally.
    /// Provider auth/secrets live in the backend's settings, NOT here.
    None,
    /// A file (or bundle) already present on disk at a user-supplied
    /// path; zwhisper does not download it but still verifies shape and,
    /// when a checksum is given, integrity.
    LocalPath {
        path: PathBuf,
        sha256: Option<String>,
    },
    /// Single file fetched over HTTPS and verified by SHA-256 + size
    /// (the existing whisper.cpp downloader contract).
    SingleFile {
        url: String,
        sha256: String,
        size_bytes: u64,
    },
    /// Multiple files fetched over HTTPS into a `DirectoryBundle`, each
    /// verified independently. No extraction step, no archive attack
    /// surface.
    MultiFile { files: Vec<RemoteFile> },
    /// Archive fetched over HTTPS, verified by SHA-256 + size, then
    /// extracted into a `DirectoryBundle`. See the downloader's
    /// "Archive Security" rules. The bomb caps are [`NonZeroU64`] so
    /// "0 == disabled" is unrepresentable at the type level.
    Archive {
        url: String,
        sha256: String,
        size_bytes: u64,
        max_unpacked_bytes: NonZeroU64,
        max_entry_count: NonZeroU64,
    },
}

impl ModelSource {
    /// `true` when this source is downloadable over the network.
    pub fn is_remote_download(&self) -> bool {
        matches!(
            self,
            Self::SingleFile { .. } | Self::MultiFile { .. } | Self::Archive { .. }
        )
    }
}

/// Free-form runtime metadata. Kept opaque to the registry except
/// `expected_asr_rate_hz`, which the coordinator reconciles against the
/// normalized PCM rate (cross-axis reconciliation).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeMeta {
    /// The ASR sample rate a local PCM model expects, when applicable.
    pub expected_asr_rate_hz: Option<u32>,
    pub quantization: Option<String>,
    pub runtime_hint: Option<String>,
}

/// Backend-agnostic description of one model. `backend` reuses the
/// existing [`Backend`] enum — no new backend enumeration is introduced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    /// Human-friendly id, e.g. "small", "parakeet-tdt-0.6b-v3", "nova-3".
    pub id: String,
    pub backend: Backend,
    pub kind: ModelKind,
    pub source: ModelSource,
    /// ISO 639 codes the model supports, or empty for "auto only".
    pub languages: Vec<String>,
    pub runtime: RuntimeMeta,
}

/// Uniform installed/missing status across all kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    Installed,
    Missing,
    /// Bundle present but incomplete or checksum-mismatched.
    Corrupt {
        detail: String,
    },
    /// Remote model: nothing to install; presence is provider-defined.
    RemoteManaged,
}

/// Validate one path component against the model-name allow-list:
/// `[A-Za-z0-9._-]`, non-empty, not `.`/`..`, no path separators, no
/// drive/UNC prefixes. This is the registry-load path-injection guard
/// (CWE-22). Rejecting any separator means bundles are flat (no
/// sub-directories), which is the safe default and matches every model
/// we ship.
pub(crate) fn validate_name_component(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value == "." || value == ".." {
        return Err(format!("{field} must not be `.` or `..`"));
    }
    for ch in value.chars() {
        let allowed = ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-';
        if !allowed {
            return Err(format!(
                "{field} `{value}` contains a forbidden character; allowed: A-Z a-z 0-9 . _ - \
                 (no path separators, no `:`, no `..`)"
            ));
        }
    }
    Ok(())
}

impl ModelSpec {
    /// Registry-load validation: the name allow-list on every
    /// path-deriving field, plus kind/source compatibility. Returns a
    /// typed error so a malformed or hostile spec is rejected before any
    /// resolution or install can act on it.
    pub fn validate(&self) -> Result<(), TranscribeError> {
        let reject = |reason: String| TranscribeError::InvalidModelSpec {
            id: self.id.clone(),
            reason,
        };

        if self.id.is_empty() {
            return Err(reject("model id must not be empty".to_owned()));
        }

        // ----- name allow-list on path-deriving fields -----
        match &self.kind {
            ModelKind::SingleFile { file_name } => {
                validate_name_component(file_name, "kind.file_name").map_err(reject)?;
            }
            ModelKind::DirectoryBundle {
                dir_name,
                expected_files,
            } => {
                validate_name_component(dir_name, "kind.dir_name").map_err(reject)?;
                if expected_files.is_empty() {
                    return Err(reject(
                        "DirectoryBundle.expected_files must not be empty".to_owned(),
                    ));
                }
                for f in expected_files {
                    validate_name_component(f, "kind.expected_files[]").map_err(reject)?;
                }
            }
            ModelKind::Remote => {}
        }

        if let ModelSource::MultiFile { files } = &self.source {
            if files.is_empty() {
                return Err(reject("MultiFile.files must not be empty".to_owned()));
            }
            for f in files {
                validate_name_component(&f.relative_path, "source.files[].relative_path")
                    .map_err(reject)?;
                require_https(&f.url).map_err(reject)?;
            }
        }
        if let ModelSource::SingleFile { url, .. } | ModelSource::Archive { url, .. } = &self.source
        {
            require_https(url).map_err(reject)?;
        }

        // ----- kind/source compatibility -----
        let ok = matches!(
            (&self.kind, &self.source),
            (ModelKind::SingleFile { .. }, ModelSource::SingleFile { .. })
                | (ModelKind::SingleFile { .. }, ModelSource::LocalPath { .. })
                | (
                    ModelKind::DirectoryBundle { .. },
                    ModelSource::MultiFile { .. }
                )
                | (
                    ModelKind::DirectoryBundle { .. },
                    ModelSource::Archive { .. }
                )
                | (
                    ModelKind::DirectoryBundle { .. },
                    ModelSource::LocalPath { .. }
                )
                | (ModelKind::Remote, ModelSource::None)
        );
        if !ok {
            return Err(reject(format!(
                "kind {:?} is not compatible with source {:?} \
                 (Archive/MultiFile require DirectoryBundle; Remote requires source None)",
                self.kind.tag(),
                source_label(&self.source),
            )));
        }

        Ok(())
    }
}

fn source_label(s: &ModelSource) -> &'static str {
    match s {
        ModelSource::None => "None",
        ModelSource::LocalPath { .. } => "LocalPath",
        ModelSource::SingleFile { .. } => "SingleFile",
        ModelSource::MultiFile { .. } => "MultiFile",
        ModelSource::Archive { .. } => "Archive",
    }
}

fn require_https(url: &str) -> Result<(), String> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(format!("url must be https://, got {url:?}"))
    }
}

/// Backend-agnostic registry of every model the app knows about.
/// Validated once at load; resolution and status dispatch on
/// [`ModelKind`].
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    specs: Vec<ModelSpec>,
}

impl ModelRegistry {
    /// Construct from a set of specs, validating each at load time.
    /// Returns the first validation failure so a hostile entry cannot
    /// reach resolution.
    pub fn new(specs: Vec<ModelSpec>) -> Result<Self, TranscribeError> {
        for spec in &specs {
            spec.validate()?;
        }
        Ok(Self { specs })
    }

    /// An empty registry. Infallible — used as a last-resort fallback
    /// when the embedded specs somehow fail validation (a unit test
    /// prevents that), keeping the `OnceLock` closure total without an
    /// `unwrap`/`expect`.
    pub fn empty() -> Self {
        Self { specs: Vec::new() }
    }

    pub fn specs(&self) -> &[ModelSpec] {
        &self.specs
    }

    /// All specs registered for a given backend.
    pub fn for_backend(&self, backend: Backend) -> impl Iterator<Item = &ModelSpec> {
        self.specs.iter().filter(move |s| s.backend == backend)
    }

    /// Exact `(backend, id)` lookup.
    pub fn lookup(&self, backend: Backend, id: &str) -> Option<&ModelSpec> {
        self.specs
            .iter()
            .find(|s| s.backend == backend && s.id == id)
    }

    /// First spec whose id matches, regardless of backend. Convenience
    /// for the CLI `model` command, where the user types an id without a
    /// backend.
    pub fn find_by_id(&self, id: &str) -> Option<&ModelSpec> {
        self.specs.iter().find(|s| s.id == id)
    }

    /// `true` when the backend's registered specs are remote-only
    /// (i.e. it never has a local artifact). Used to resolve provider
    /// model ids that are not individually enumerated (e.g. Deepgram's
    /// many nova variants) without hardcoding per-backend branches.
    fn backend_is_remote(&self, backend: Backend) -> bool {
        let mut any = false;
        for s in self.for_backend(backend) {
            any = true;
            if s.kind.tag() != ModelKindTag::Remote {
                return false;
            }
        }
        any
    }

    /// Resolve a `(backend, model_id)` to a concrete [`ModelArtifact`].
    ///
    /// - Exact registry hit → resolve per [`ModelKind`] (and verify the
    ///   local artifact exists; missing/incomplete → typed error, never
    ///   a silent fallback to a different model).
    /// - No hit but the backend is remote-only → synthesize
    ///   `Remote { id }` (provider model ids are not gatekept locally).
    /// - Otherwise → [`TranscribeError::ModelNotFound`].
    pub fn resolve(
        &self,
        backend: Backend,
        model_id: &str,
        models_dir: &Path,
    ) -> Result<ModelArtifact, TranscribeError> {
        if let Some(spec) = self.lookup(backend, model_id) {
            return self.resolve_spec(spec, models_dir);
        }
        if self.backend_is_remote(backend) {
            return Ok(ModelArtifact::Remote {
                id: model_id.to_owned(),
            });
        }
        let known: Vec<String> = self.for_backend(backend).map(|s| s.id.clone()).collect();
        tracing::debug!(
            backend = backend.as_str(),
            model = model_id,
            known = ?known,
            "model id not found in registry for backend"
        );
        // The `Display` impl already names the install action.
        Err(TranscribeError::ModelNotFound {
            name: model_id.to_owned(),
            expected: models_dir.join(format!("<{}>", backend.as_str())),
        })
    }

    /// Resolve a known spec, validating the local artifact for local
    /// kinds.
    pub fn resolve_spec(
        &self,
        spec: &ModelSpec,
        models_dir: &Path,
    ) -> Result<ModelArtifact, TranscribeError> {
        match &spec.kind {
            ModelKind::SingleFile { file_name } => {
                let path = models_dir.join(file_name);
                if !path.is_file() {
                    return Err(TranscribeError::ModelNotFound {
                        name: spec.id.clone(),
                        expected: path,
                    });
                }
                Ok(ModelArtifact::File(path))
            }
            ModelKind::DirectoryBundle {
                dir_name,
                expected_files,
            } => {
                let dir = models_dir.join(dir_name);
                // A missing directory and a present-but-incomplete one
                // are the same actionable problem: the bundle is not
                // (fully) installed. Surface the bundle-shaped error in
                // both cases so the message is correct for a directory
                // bundle (never the whisper-specific `ModelNotFound`
                // text). When the dir is absent, every expected file is
                // "missing".
                let missing: Vec<String> = if dir.is_dir() {
                    expected_files
                        .iter()
                        .filter(|f| !dir.join(f).is_file())
                        .cloned()
                        .collect()
                } else {
                    expected_files.clone()
                };
                if !missing.is_empty() {
                    return Err(TranscribeError::ModelBundleIncomplete {
                        id: spec.id.clone(),
                        dir,
                        missing,
                    });
                }
                Ok(ModelArtifact::Directory(dir))
            }
            ModelKind::Remote => Ok(ModelArtifact::Remote {
                id: spec.id.clone(),
            }),
        }
    }

    /// Compute the install status of a spec without resolving it.
    pub fn status(&self, spec: &ModelSpec, models_dir: &Path) -> ModelStatus {
        match &spec.kind {
            ModelKind::SingleFile { file_name } => {
                if models_dir.join(file_name).is_file() {
                    ModelStatus::Installed
                } else {
                    ModelStatus::Missing
                }
            }
            ModelKind::DirectoryBundle {
                dir_name,
                expected_files,
            } => {
                let dir = models_dir.join(dir_name);
                if !dir.exists() {
                    return ModelStatus::Missing;
                }
                if !dir.is_dir() {
                    return ModelStatus::Corrupt {
                        detail: format!("{} exists but is not a directory", dir.display()),
                    };
                }
                let missing: Vec<&String> = expected_files
                    .iter()
                    .filter(|f| !dir.join(f).is_file())
                    .collect();
                if missing.is_empty() {
                    ModelStatus::Installed
                } else if missing.len() == expected_files.len() {
                    ModelStatus::Missing
                } else {
                    ModelStatus::Corrupt {
                        detail: format!("bundle incomplete; missing {missing:?}"),
                    }
                }
            }
            ModelKind::Remote => ModelStatus::RemoteManaged,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn nz(v: u64) -> NonZeroU64 {
        NonZeroU64::new(v).unwrap()
    }

    fn single_file_spec(id: &str) -> ModelSpec {
        ModelSpec {
            id: id.to_owned(),
            backend: Backend::WhisperCpp,
            kind: ModelKind::SingleFile {
                file_name: format!("ggml-{id}.bin"),
            },
            source: ModelSource::SingleFile {
                url: format!("https://example.com/ggml-{id}.bin"),
                sha256: "deadbeef".to_owned(),
                size_bytes: 10,
            },
            languages: vec![],
            runtime: RuntimeMeta::default(),
        }
    }

    fn bundle_spec() -> ModelSpec {
        ModelSpec {
            id: "parakeet-v3".to_owned(),
            backend: Backend::Parakeet,
            kind: ModelKind::DirectoryBundle {
                dir_name: "parakeet-v3-int8".to_owned(),
                expected_files: vec!["encoder.int8.onnx".to_owned(), "vocab.txt".to_owned()],
            },
            source: ModelSource::MultiFile {
                files: vec![RemoteFile {
                    relative_path: "encoder.int8.onnx".to_owned(),
                    url: "https://hf.co/x/encoder.int8.onnx".to_owned(),
                    sha256: "abc".to_owned(),
                    size_bytes: 1,
                }],
            },
            languages: vec![],
            runtime: RuntimeMeta {
                expected_asr_rate_hz: Some(16_000),
                quantization: Some("int8".to_owned()),
                runtime_hint: Some("onnx".to_owned()),
            },
        }
    }

    #[test]
    fn valid_specs_pass_registry_load() {
        ModelRegistry::new(vec![single_file_spec("small"), bundle_spec()]).unwrap();
    }

    #[test]
    fn dir_name_traversal_rejected_at_load() {
        let mut spec = bundle_spec();
        spec.kind = ModelKind::DirectoryBundle {
            dir_name: "../escape".to_owned(),
            expected_files: vec!["x".to_owned()],
        };
        let err = ModelRegistry::new(vec![spec]).unwrap_err();
        assert!(matches!(err, TranscribeError::InvalidModelSpec { .. }));
    }

    #[test]
    fn expected_file_with_separator_rejected() {
        let mut spec = bundle_spec();
        spec.kind = ModelKind::DirectoryBundle {
            dir_name: "ok".to_owned(),
            expected_files: vec!["sub/dir/file".to_owned()],
        };
        let err = ModelRegistry::new(vec![spec]).unwrap_err();
        assert!(matches!(err, TranscribeError::InvalidModelSpec { .. }));
    }

    #[test]
    fn multifile_relative_path_traversal_rejected() {
        let mut spec = bundle_spec();
        spec.source = ModelSource::MultiFile {
            files: vec![RemoteFile {
                relative_path: "../../etc/passwd".to_owned(),
                url: "https://hf.co/x".to_owned(),
                sha256: "a".to_owned(),
                size_bytes: 1,
            }],
        };
        let err = ModelRegistry::new(vec![spec]).unwrap_err();
        assert!(matches!(err, TranscribeError::InvalidModelSpec { .. }));
    }

    #[test]
    fn non_https_source_url_rejected() {
        let mut spec = single_file_spec("small");
        spec.source = ModelSource::SingleFile {
            url: "http://insecure.example/x.bin".to_owned(),
            sha256: "a".to_owned(),
            size_bytes: 1,
        };
        let err = ModelRegistry::new(vec![spec]).unwrap_err();
        assert!(matches!(err, TranscribeError::InvalidModelSpec { .. }));
    }

    #[test]
    fn incompatible_kind_source_rejected() {
        // SingleFile kind with an Archive source.
        let spec = ModelSpec {
            kind: ModelKind::SingleFile {
                file_name: "x.bin".to_owned(),
            },
            source: ModelSource::Archive {
                url: "https://x/a.tar.gz".to_owned(),
                sha256: "a".to_owned(),
                size_bytes: 1,
                max_unpacked_bytes: nz(1),
                max_entry_count: nz(1),
            },
            ..single_file_spec("x")
        };
        let err = ModelRegistry::new(vec![spec]).unwrap_err();
        assert!(matches!(err, TranscribeError::InvalidModelSpec { .. }));
    }

    #[test]
    fn resolve_single_file_present_and_missing() {
        let tmp = TempDir::new().unwrap();
        let models = tmp.path();
        let reg = ModelRegistry::new(vec![single_file_spec("small")]).unwrap();

        // Missing.
        let err = reg
            .resolve(Backend::WhisperCpp, "small", models)
            .unwrap_err();
        assert!(matches!(err, TranscribeError::ModelNotFound { .. }));

        // Present.
        fs::write(models.join("ggml-small.bin"), b"x").unwrap();
        let art = reg.resolve(Backend::WhisperCpp, "small", models).unwrap();
        assert_eq!(art, ModelArtifact::File(models.join("ggml-small.bin")));
    }

    #[test]
    fn resolve_bundle_incomplete_is_typed_error() {
        let tmp = TempDir::new().unwrap();
        let models = tmp.path();
        let reg = ModelRegistry::new(vec![bundle_spec()]).unwrap();
        let dir = models.join("parakeet-v3-int8");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("encoder.int8.onnx"), b"x").unwrap();
        // vocab.txt missing → incomplete.
        let err = reg
            .resolve(Backend::Parakeet, "parakeet-v3", models)
            .unwrap_err();
        match err {
            TranscribeError::ModelBundleIncomplete { missing, .. } => {
                assert_eq!(missing, vec!["vocab.txt".to_owned()]);
            }
            other => panic!("expected ModelBundleIncomplete, got {other:?}"),
        }
    }

    #[test]
    fn status_reports_installed_missing_corrupt() {
        let tmp = TempDir::new().unwrap();
        let models = tmp.path();
        let reg = ModelRegistry::new(vec![bundle_spec()]).unwrap();
        let spec = &reg.specs()[0];

        assert_eq!(reg.status(spec, models), ModelStatus::Missing);

        let dir = models.join("parakeet-v3-int8");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("encoder.int8.onnx"), b"x").unwrap();
        assert!(matches!(
            reg.status(spec, models),
            ModelStatus::Corrupt { .. }
        ));

        fs::write(dir.join("vocab.txt"), b"x").unwrap();
        assert_eq!(reg.status(spec, models), ModelStatus::Installed);
    }

    #[test]
    fn remote_backend_resolves_arbitrary_id() {
        let remote_spec = ModelSpec {
            id: "nova-3".to_owned(),
            backend: Backend::Deepgram,
            kind: ModelKind::Remote,
            source: ModelSource::None,
            languages: vec![],
            runtime: RuntimeMeta::default(),
        };
        let reg = ModelRegistry::new(vec![remote_spec]).unwrap();
        // An unlisted id still resolves to Remote for a remote-only backend.
        let art = reg
            .resolve(Backend::Deepgram, "nova-3-medical", Path::new("/models"))
            .unwrap();
        assert_eq!(
            art,
            ModelArtifact::Remote {
                id: "nova-3-medical".to_owned()
            }
        );
    }

    #[test]
    fn missing_local_model_for_non_remote_backend_errs() {
        let reg = ModelRegistry::new(vec![single_file_spec("small")]).unwrap();
        let err = reg
            .resolve(Backend::WhisperCpp, "does-not-exist", Path::new("/models"))
            .unwrap_err();
        assert!(matches!(err, TranscribeError::ModelNotFound { .. }));
    }
}
