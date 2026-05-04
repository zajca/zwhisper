//! M7 — Whisper.cpp model SHA256 manifest, embedded at build time.
//!
//! The single source of truth is `crates/zwhisper-settings/checksums.toml`
//! (`include_str!`'d here). Group D (M7-plan § D3) chose compile-time
//! embedding over a runtime fetch so a manifest-server compromise cannot
//! tamper with the trust anchor — adding a new model is a release event,
//! not a config event.
//!
//! Public surface used by [`crate::download::ModelDownloader`]:
//! - [`ChecksumManifest::embedded`] — singleton initialised lazily.
//! - [`ChecksumManifest::lookup`] — returns the [`Entry`] for a model
//!   name, or `None` when the user typed an unknown name (DoD #10
//!   "refuse unknown models with a friendly error").
//! - [`ChecksumManifest::known_models`] — iterator used by
//!   [`crate::tabs::models`] to build one row per known model.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

/// Compile-time embedded TOML manifest. Bumped at release time.
const EMBEDDED: &str = include_str!("../checksums.toml");

/// One row in the manifest — checksum + expected size.
///
/// `size_bytes` is the second guard (DoD #9) on top of `sha256`: a
/// `Content-Length` mismatch at HEAD time aborts before the
/// downloader opens a `.part` file, so an HTML 200 from a captive
/// portal cannot pass the checksum gate by accident.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Hex-encoded SHA-256 of the model file. Lower-case, 64 chars.
    pub sha256: String,
    /// Expected `Content-Length` of the response body. Required
    /// match: `==` (no tolerance). HTTPS proxies that re-compress
    /// would break this; we accept that cost.
    pub size_bytes: u64,
}

/// Parsed manifest. Wraps a [`BTreeMap`] so the iteration order is
/// deterministic for tab rendering.
#[derive(Debug, Clone)]
pub(crate) struct ChecksumManifest(BTreeMap<String, Entry>);

impl ChecksumManifest {
    /// Parse a TOML body into a manifest. Used by [`Self::embedded`]
    /// and by tests that want to inject a fixture.
    pub(crate) fn parse(toml_text: &str) -> Result<Self, toml::de::Error> {
        let map: BTreeMap<String, Entry> = toml::from_str(toml_text)?;
        Ok(Self(map))
    }

    /// Lazily-initialised singleton. The first call parses
    /// [`EMBEDDED`]; subsequent calls return the cached reference.
    /// Parse failure is a release-time bug (the TOML is checked into
    /// git), so we surface it via `tracing::error!` and return an
    /// empty manifest — the UI then disables every download row.
    pub(crate) fn embedded() -> &'static Self {
        static INSTANCE: OnceLock<ChecksumManifest> = OnceLock::new();
        INSTANCE.get_or_init(|| match Self::parse(EMBEDDED) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "embedded checksums.toml failed to parse — \
                     this is a release-time bug; no models will be \
                     available for download"
                );
                Self(BTreeMap::new())
            }
        })
    }

    /// Look up `model_name` (e.g. `"tiny"`, `"large-v3"`). Returns
    /// `None` when the name is absent — callers must surface a
    /// friendly "unknown model" error per DoD #10.
    pub(crate) fn lookup(&self, model_name: &str) -> Option<&Entry> {
        self.0.get(model_name)
    }

    /// Iterator over known model names in deterministic order.
    /// Used by the Models tab to populate one row per model.
    pub(crate) fn known_models(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parse_embedded_manifest_lists_five_classics() {
        let manifest = ChecksumManifest::parse(EMBEDDED).expect("embedded TOML parses");
        let names: Vec<&str> = manifest.known_models().collect();
        // BTreeMap iteration order is alphabetical.
        assert_eq!(names, vec!["base", "large-v3", "medium", "small", "tiny"]);
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let manifest = ChecksumManifest::parse(EMBEDDED).expect("embedded TOML parses");
        assert!(manifest.lookup("does-not-exist").is_none());
        assert!(manifest.lookup("").is_none());
    }

    #[test]
    fn lookup_returns_full_entry_for_tiny() {
        let manifest = ChecksumManifest::parse(EMBEDDED).expect("embedded TOML parses");
        let tiny = manifest.lookup("tiny").expect("tiny is a classic");
        assert_eq!(tiny.sha256.len(), 64, "sha256 must be 64 hex chars");
        assert!(
            tiny.sha256.chars().all(|c| c.is_ascii_hexdigit()),
            "sha256 must be hex"
        );
        assert!(tiny.size_bytes > 0);
    }

    #[test]
    fn embedded_singleton_is_stable() {
        let a = ChecksumManifest::embedded();
        let b = ChecksumManifest::embedded();
        assert!(std::ptr::eq(a, b), "OnceLock must hand out the same ref");
    }

    #[test]
    fn malformed_toml_surfaces_error() {
        let err = ChecksumManifest::parse("[tiny]\nsha256 = 123\n").unwrap_err();
        // We do not match on the exact message — toml versions move
        // it around. Just assert that *some* error materialised so
        // the embedded fallback path in `embedded()` is reachable.
        assert!(!err.to_string().is_empty());
    }
}
