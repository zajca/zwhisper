//! The embedded [`ModelRegistry`]: every model zwhisper ships knowledge
//! of, expressed as backend-agnostic [`ModelSpec`]s.
//!
//! Phase 1 of the RFC builds this from three sources:
//!
//! - the existing embedded whisper manifest — every current model
//!   becomes a [`ModelKind::SingleFile`] spec (one source of truth for
//!   the ggml SHA-256 values stays in `model_management`);
//! - a [`ModelKind::Remote`] spec for Deepgram;
//! - a [`ModelKind::DirectoryBundle`] + [`ModelSource::MultiFile`] spec
//!   for Parakeet TDT 0.6B v3 (int8), with per-file SHA-256 + size
//!   verified against the upstream HuggingFace repository.
//!
//! The registry is validated at load (allow-list on every path-deriving
//! field + kind/source compatibility); `embedded()` caches the
//! validated instance. A unit test asserts the embedded specs validate,
//! so a malformed addition fails CI loudly rather than silently
//! disabling models at runtime.

use std::sync::OnceLock;

use crate::profile::schema::Backend;

use super::model::{ModelKind, ModelRegistry, ModelSource, ModelSpec, RemoteFile, RuntimeMeta};
use super::model_management::{ModelManifest, ModelSourceConfig};

/// HuggingFace base for the Parakeet v3 int8 ONNX bundle. `/resolve/main`
/// 302-redirects to the xethub CAS bridge; the downloader follows
/// HTTPS→HTTPS redirects (and rejects any downgrade to HTTP).
const PARAKEET_V3_INT8_BASE: &str =
    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main";

/// Parakeet v3 int8 bundle directory name under `models_dir`.
const PARAKEET_V3_INT8_DIR: &str = "parakeet-tdt-0.6b-v3-int8";

/// Build the SingleFile specs for the embedded whisper manifest. The
/// URL is resolved from the default whisper base template; the SHA-256
/// and size come from the manifest, keeping one source of truth.
fn whisper_specs() -> Vec<ModelSpec> {
    let manifest = ModelManifest::embedded();
    let url_template = ModelSourceConfig::default();
    let mut out = Vec::new();
    for (name, entry) in manifest.known_models() {
        // `resolve_url` only fails on a malformed base template, which
        // is a compile-time constant here; skip any that somehow fail
        // rather than poisoning the whole registry.
        let Ok(url) = url_template.resolve_url(name) else {
            tracing::error!(
                model = name,
                "could not resolve whisper model URL for registry"
            );
            continue;
        };
        out.push(ModelSpec {
            id: name.to_owned(),
            backend: Backend::WhisperCpp,
            kind: ModelKind::SingleFile {
                file_name: format!("ggml-{name}.bin"),
            },
            source: ModelSource::SingleFile {
                url,
                sha256: entry.sha256.clone(),
                size_bytes: entry.size_bytes,
            },
            languages: vec![],
            runtime: RuntimeMeta {
                // whisper.cpp consumes the encoded FLAC directly; it has
                // no PCM rate expectation of its own.
                expected_asr_rate_hz: None,
                quantization: None,
                runtime_hint: Some("whisper.cpp".to_owned()),
            },
        });
    }
    out
}

/// The Deepgram remote spec. Deepgram supports many model ids; this
/// single Remote spec marks the backend remote-only so any provider id
/// resolves to `ModelArtifact::Remote` without local gatekeeping.
fn deepgram_spec() -> ModelSpec {
    ModelSpec {
        id: "nova-3".to_owned(),
        backend: Backend::Deepgram,
        kind: ModelKind::Remote,
        source: ModelSource::None,
        languages: vec![],
        runtime: RuntimeMeta {
            expected_asr_rate_hz: None,
            quantization: None,
            runtime_hint: Some("deepgram-batch".to_owned()),
        },
    }
}

/// Parakeet TDT 0.6B v3 (int8) as a directory bundle fetched per-file
/// from HuggingFace. Checksums + sizes verified against the upstream
/// repo on 2026-06-02; the four files are exactly the set
/// `transcribe-rs` expects in the model directory.
fn parakeet_v3_int8_spec() -> ModelSpec {
    let file = |name: &str, sha256: &str, size_bytes: u64| RemoteFile {
        relative_path: name.to_owned(),
        url: format!("{PARAKEET_V3_INT8_BASE}/{name}"),
        sha256: sha256.to_owned(),
        size_bytes,
    };
    let files = vec![
        file(
            "encoder-model.int8.onnx",
            "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09",
            652_183_999,
        ),
        file(
            "decoder_joint-model.int8.onnx",
            "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70",
            18_202_004,
        ),
        file(
            "nemo128.onnx",
            "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
            139_764,
        ),
        file(
            "vocab.txt",
            "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
            93_939,
        ),
    ];
    let expected_files = files.iter().map(|f| f.relative_path.clone()).collect();
    ModelSpec {
        id: "parakeet-tdt-0.6b-v3".to_owned(),
        backend: Backend::Parakeet,
        kind: ModelKind::DirectoryBundle {
            dir_name: PARAKEET_V3_INT8_DIR.to_owned(),
            expected_files,
        },
        source: ModelSource::MultiFile { files },
        // v3 auto-detects language; no manual selection. Empty = auto.
        languages: vec![],
        runtime: RuntimeMeta {
            expected_asr_rate_hz: Some(16_000),
            quantization: Some("int8".to_owned()),
            runtime_hint: Some("transcribe-rs/onnx".to_owned()),
        },
    }
}

/// All embedded specs, unvalidated. Exposed for tests.
pub(crate) fn embedded_specs() -> Vec<ModelSpec> {
    let mut specs = whisper_specs();
    specs.push(deepgram_spec());
    specs.push(parakeet_v3_int8_spec());
    specs
}

/// The process-wide validated registry. On the (test-prevented) event
/// that an embedded spec fails validation, logs and returns an empty
/// registry rather than panicking, mirroring `ModelManifest::embedded`.
pub fn embedded() -> &'static ModelRegistry {
    static INSTANCE: OnceLock<ModelRegistry> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        ModelRegistry::new(embedded_specs()).unwrap_or_else(|err| {
            tracing::error!(
                error = %err,
                "embedded model registry failed to validate; no models available"
            );
            ModelRegistry::empty()
        })
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::transcribe::model::ModelKindTag;

    #[test]
    fn embedded_specs_validate_at_load() {
        // The whole point of registry-load validation: a malformed
        // embedded spec must fail here, not silently at runtime.
        ModelRegistry::new(embedded_specs()).expect("embedded specs must validate");
    }

    #[test]
    fn embedded_contains_whisper_deepgram_parakeet() {
        let reg = embedded();
        assert!(
            reg.lookup(Backend::WhisperCpp, "small").is_some(),
            "whisper `small` should be registered"
        );
        let dg = reg
            .lookup(Backend::Deepgram, "nova-3")
            .expect("deepgram spec");
        assert_eq!(dg.kind.tag(), ModelKindTag::Remote);
        let pk = reg
            .lookup(Backend::Parakeet, "parakeet-tdt-0.6b-v3")
            .expect("parakeet spec");
        assert_eq!(pk.kind.tag(), ModelKindTag::DirectoryBundle);
        assert_eq!(pk.runtime.expected_asr_rate_hz, Some(16_000));
    }

    #[test]
    fn parakeet_bundle_lists_four_expected_files() {
        let reg = embedded();
        let pk = reg
            .lookup(Backend::Parakeet, "parakeet-tdt-0.6b-v3")
            .unwrap();
        if let ModelKind::DirectoryBundle { expected_files, .. } = &pk.kind {
            assert_eq!(expected_files.len(), 4);
            assert!(expected_files.contains(&"vocab.txt".to_owned()));
            assert!(expected_files.contains(&"encoder-model.int8.onnx".to_owned()));
        } else {
            panic!("parakeet must be a DirectoryBundle");
        }
    }

    #[test]
    fn whisper_specs_carry_manifest_checksums() {
        let reg = embedded();
        let small = reg.lookup(Backend::WhisperCpp, "small").unwrap();
        match &small.source {
            ModelSource::SingleFile {
                sha256,
                size_bytes,
                url,
            } => {
                assert!(!sha256.is_empty());
                assert!(*size_bytes > 0);
                assert!(url.starts_with("https://"));
            }
            other => panic!("expected SingleFile source, got {other:?}"),
        }
    }
}
