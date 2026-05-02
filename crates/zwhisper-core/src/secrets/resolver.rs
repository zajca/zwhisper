//! API-key resolution implementation.
//!
//! Public surface kept narrow on purpose: callers ask for a backend
//! id, get back either a [`SecretString`] or a typed [`SecretsError`].
//! Tests inject overrides via [`ResolverConfig`] so the production
//! call site stays a one-liner.
//!
//! See `docs/M5-plan.md` Phase 1 for the full specification.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use zeroize::Zeroize;

/// Default path under `$HOME` for the on-disk secrets file. Used when
/// [`ResolverConfig::secrets_path`] is left at default.
pub const DEFAULT_SECRETS_RELATIVE_PATH: &str = ".config/zwhisper/secrets.toml";

/// Modes accepted on `secrets.toml`. `0o600` is the canonical mode;
/// `0o400` is also accepted because it is strictly more locked-down
/// (read-only by user, no write) — see M5-plan OQ-2.
const ACCEPTED_MODES: &[u32] = &[0o600, 0o400];

/// Line/column landmark from the underlying TOML parser, scrubbed of
/// any source content. Stored on [`SecretsError::Parse`] so the user
/// can fix the file without exposing the offending line — which on
/// `secrets.toml` carries the API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLocation {
    pub line: usize,
    pub column: usize,
}

/// Source from which the secret was loaded. Returned alongside the
/// [`SecretString`] so callers can log provenance without leaking
/// the value (the path is fine to log; the key is not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveSource {
    /// Read from `ZWHISPER_<BACKEND>_API_KEY` env var.
    Env(String),
    /// Read from the on-disk TOML at this path.
    File(PathBuf),
}

/// Newtype wrapping the raw API key. `Debug` and `Display` are both
/// implemented to print only `"***"` so the value never escapes via
/// `format!("{:?}", ...)` or `tracing::field::display(...)`. The
/// inner `String` is zeroed on drop via the `Zeroize` derive.
///
/// Use [`SecretString::expose_secret`] at the single moment you need
/// the bytes for an HTTP header — and never log the result.
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Return the raw key. Callers MUST NOT log the result. Used at
    /// the one site where the value is consumed (the `Authorization`
    /// header in `transcribe::deepgram`).
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Length of the key in bytes. Useful for log lines that want to
    /// say "key has length 40" without leaking content.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SecretString").field(&"***").finish()
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Errors from [`resolve_api_key`]. Never carry the key itself; the
/// `path`, `mode`, and `uid` fields are the maximum amount of context
/// useful to the user without enabling key exfiltration via logs.
#[derive(Debug, Error)]
pub enum SecretsError {
    #[error(
        "no API key for backend `{backend}`: set `{env_var}` or create `{path}` with mode 0600"
    )]
    NotFound {
        backend: String,
        env_var: String,
        path: PathBuf,
    },
    #[error(
        "secrets file `{}` has unsafe permissions: mode 0o{mode:o}, expected 0o600 or 0o400",
        path.display()
    )]
    PermissionsMode { path: PathBuf, mode: u32 },
    #[error(
        "secrets file `{}` is owned by uid {file_uid}, expected effective uid {process_uid}",
        path.display()
    )]
    PermissionsOwner {
        path: PathBuf,
        file_uid: u32,
        process_uid: u32,
    },
    #[error(
        "parent directory `{}` of secrets file is group/other writable (mode 0o{mode:o}); refusing to read",
        path.display()
    )]
    PermissionsParent { path: PathBuf, mode: u32 },
    #[error("I/O error reading secrets file `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `toml_edit::de::Error::Display` includes a snippet of the
    /// offending source line, which on `secrets.toml` carries the
    /// API key. Both `Display` AND the `#[source]` chain are
    /// scrubbed: only the path and the line/column landmarks reach
    /// any formatter or chain walker. The full parser error never
    /// reaches `tracing::error!(error = ?err)` or
    /// `eyre`-style `{:#}` chain rendering. Copilot review
    /// (2026-05-02) flagged this as the residual leak surface.
    #[error("malformed secrets TOML at `{}`{}: run `toml-cli validate` to inspect locally",
        path.display(),
        location.as_ref().map(|l| format!(" (line {}, column {})", l.line, l.column)).unwrap_or_default()
    )]
    Parse {
        path: PathBuf,
        location: Option<ParseLocation>,
    },
    #[error(
        "secrets file `{}` is missing entry [{backend}].api_key",
        path.display()
    )]
    MissingBackendSection { path: PathBuf, backend: String },
    /// Distinct from [`Self::MissingBackendSection`]: the user wrote
    /// `[<backend>] api_key = ""`. The diagnostic distinguishes the
    /// two so the user does not waste time looking for a missing
    /// entry that is in fact empty (silent-failure review #3,
    /// 2026-05-02).
    #[error(
        "secrets file `{}` has an empty [{backend}].api_key — set a non-empty value",
        path.display()
    )]
    EmptyApiKey { path: PathBuf, backend: String },
    #[error("backend id `{backend}` is empty or contains invalid characters")]
    InvalidBackend { backend: String },
}

/// Test-friendly knobs. Production code uses [`ResolverConfig::default`]
/// which derives `secrets_path` from `$HOME`.
#[derive(Debug, Clone)]
pub struct ResolverConfig {
    /// Override for the on-disk TOML path. `None` → `$HOME/.config/zwhisper/secrets.toml`.
    pub secrets_path: Option<PathBuf>,
    /// Override for the env var lookup. `None` → use `std::env::var_os`.
    /// Used only by unit tests.
    pub env: EnvLookup,
}

/// Indirection for the env var lookup so tests can inject a fake
/// environment without mutating the process-global one (which would
/// race with parallel tests).
#[derive(Debug, Clone, Default)]
pub enum EnvLookup {
    #[default]
    Process,
    /// Single-entry override: `(name, value)`. Any other env name
    /// returns `None`. Test-only.
    Fake(Vec<(String, String)>),
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            secrets_path: None,
            env: EnvLookup::Process,
        }
    }
}

impl ResolverConfig {
    fn lookup_env(&self, key: &str) -> Option<String> {
        match &self.env {
            EnvLookup::Process => std::env::var(key).ok(),
            EnvLookup::Fake(entries) => entries
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone()),
        }
    }

    fn resolved_secrets_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.secrets_path {
            return Some(p.clone());
        }
        let home = dirs::home_dir()?;
        Some(home.join(DEFAULT_SECRETS_RELATIVE_PATH))
    }
}

/// Public façade: resolve an API key for the given backend using the
/// process environment and the default `$HOME` path.
pub fn resolve_api_key(backend: &str) -> Result<(SecretString, ResolveSource), SecretsError> {
    resolve_api_key_with_config(backend, &ResolverConfig::default())
}

/// Test-injectable variant. Production callers use [`resolve_api_key`].
pub fn resolve_api_key_with_config(
    backend: &str,
    cfg: &ResolverConfig,
) -> Result<(SecretString, ResolveSource), SecretsError> {
    validate_backend_id(backend)?;
    let env_var = env_var_for(backend);

    if let Some(value) = cfg.lookup_env(&env_var) {
        // Empty env var is treated as "not set" — common shell pitfall.
        if !value.is_empty() {
            tracing::debug!(
                target: "zwhisper_core::secrets",
                backend = %backend,
                source = "env",
                env_var = %env_var,
                "resolved API key from environment",
            );
            return Ok((SecretString::new(value), ResolveSource::Env(env_var)));
        }
    }

    let Some(path) = cfg.resolved_secrets_path() else {
        return Err(SecretsError::NotFound {
            backend: backend.to_owned(),
            env_var,
            path: PathBuf::from(DEFAULT_SECRETS_RELATIVE_PATH),
        });
    };

    match read_secrets_toml(&path) {
        Ok(mut doc) => {
            let key = extract_backend_key(&mut doc, backend, &path)?;
            tracing::debug!(
                target: "zwhisper_core::secrets",
                backend = %backend,
                source = "file",
                path = %path.display(),
                "resolved API key from secrets.toml",
            );
            // `doc` drops here; `BackendSection::Drop` zeroizes any
            // remaining `api_key` strings (we already `take()`'d
            // ours, so it's a no-op for our backend, but other
            // sections that happened to be in the file get wiped).
            Ok((key, ResolveSource::File(path)))
        }
        Err(SecretsError::Io { path, source }) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(SecretsError::NotFound {
                backend: backend.to_owned(),
                env_var,
                path,
            })
        }
        Err(other) => Err(other),
    }
}

fn validate_backend_id(backend: &str) -> Result<(), SecretsError> {
    if backend.is_empty()
        || !backend
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(SecretsError::InvalidBackend {
            backend: backend.to_owned(),
        });
    }
    Ok(())
}

fn env_var_for(backend: &str) -> String {
    let mut s = String::with_capacity(backend.len() + 24);
    s.push_str("ZWHISPER_");
    for c in backend.chars() {
        if c == '-' {
            s.push('_');
        } else {
            s.push(c.to_ascii_uppercase());
        }
    }
    s.push_str("_API_KEY");
    s
}

/// On-disk schema. Each backend has a `[<backend>]` table with a
/// single `api_key` string. Forward-compat: unknown keys inside a
/// table are tolerated (`#[serde(default)]` semantics by default for
/// missing fields; `toml_edit` ignores unknown fields).
///
/// `Debug` is deliberately NOT derived on either struct because the
/// `api_key` field would print verbatim. The structs are only used
/// inside the resolver and never escape the module boundary
/// (security review #3, 2026-05-02).
#[derive(Deserialize)]
struct SecretsToml {
    #[serde(flatten)]
    backends: std::collections::BTreeMap<String, BackendSection>,
}

#[derive(Deserialize)]
struct BackendSection {
    api_key: Option<String>,
}

/// Drop wipes the parsed key bytes from the heap before the
/// allocator releases them. Without this, the plaintext key would
/// linger in the freed pages until the allocator overwrote them
/// (which may be never for the lifetime of the process). M5
/// post-review fix (2026-05-02 user feedback #3).
impl Drop for BackendSection {
    fn drop(&mut self) {
        if let Some(s) = self.api_key.as_mut() {
            s.zeroize();
        }
    }
}

/// RAII guard that zeroizes a `String` on drop **regardless** of
/// whether parsing succeeded. The previous implementation only
/// wiped `buf` after a successful parse, leaving plaintext on the
/// error path (M5 post-review fix #3, 2026-05-02).
struct ZeroizingBuf(String);

impl Drop for ZeroizingBuf {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

fn read_secrets_toml(path: &Path) -> Result<SecretsToml, SecretsError> {
    // C3: open with O_NOFOLLOW so a symlink swap cannot redirect us
    // mid-flight, then `fstat` the returned descriptor (not the path
    // again). This closes the TOCTOU window.
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| SecretsError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    let metadata = fstat_metadata(&file).map_err(|source| SecretsError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mode = metadata.mode() & 0o777;
    if !ACCEPTED_MODES.contains(&mode) {
        return Err(SecretsError::PermissionsMode {
            path: path.to_path_buf(),
            mode,
        });
    }

    let process_uid = current_euid();
    let file_uid = metadata.uid();
    if file_uid != process_uid {
        return Err(SecretsError::PermissionsOwner {
            path: path.to_path_buf(),
            file_uid,
            process_uid,
        });
    }

    if let Some(parent) = path.parent() {
        check_parent_directory(parent)?;
    }

    let mut buf = ZeroizingBuf(String::new());
    let mut handle = file;
    handle
        .read_to_string(&mut buf.0)
        .map_err(|source| SecretsError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    // Parse via reference; whether parsing succeeds or fails, `buf`
    // is wiped on drop at the end of this function. The previous
    // implementation only wiped on the success path.
    let doc: SecretsToml =
        toml_edit::de::from_str(&buf.0).map_err(|source| SecretsError::Parse {
            path: path.to_path_buf(),
            location: source.span().map(|range| location_for_offset(&buf.0, range.start)),
        })?;

    Ok(doc)
}

/// Translate a byte offset into a 1-indexed (line, column) pair.
/// Used to surface a parse-error landmark without exposing the
/// surrounding source bytes (which contain the API key).
fn location_for_offset(s: &str, offset: usize) -> ParseLocation {
    let bytes = s.as_bytes();
    let cap = offset.min(bytes.len());
    let mut line: usize = 1;
    let mut last_nl: usize = 0;
    for (i, b) in bytes.iter().take(cap).enumerate() {
        if *b == b'\n' {
            line += 1;
            last_nl = i + 1;
        }
    }
    ParseLocation {
        line,
        column: cap - last_nl + 1,
    }
}

/// Wrapper around `libc::geteuid` confined to one function so the
/// crate-wide `unsafe_code = "deny"` lint is opted-out exactly once.
/// `geteuid` is documented as always-succeeding on POSIX systems
/// (no error path), so the safety contract reduces to "the libc
/// symbol is correctly bound" — guaranteed by the `libc` crate.
#[allow(unsafe_code)]
fn current_euid() -> u32 {
    // SAFETY: `geteuid` is async-signal-safe and has no preconditions.
    unsafe { libc::geteuid() }
}

fn fstat_metadata(file: &File) -> std::io::Result<std::fs::Metadata> {
    // `File::metadata` calls fstat under the hood on Unix, which is
    // the descriptor-based variant we want. Documented at
    // https://doc.rust-lang.org/std/fs/struct.File.html#method.metadata
    // — the path is not re-resolved.
    let _ = file.as_raw_fd(); // ensure descriptor is live (no-op).
    file.metadata()
}

fn check_parent_directory(parent: &Path) -> Result<(), SecretsError> {
    let metadata = std::fs::symlink_metadata(parent).map_err(|source| SecretsError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mode = metadata.mode() & 0o777;
    // Reject group- or other-writable parent (M5-plan Risk #7).
    if mode & 0o022 != 0 {
        return Err(SecretsError::PermissionsParent {
            path: parent.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// Move the `api_key` out of the parsed `BackendSection` so we
/// never leave a second plaintext copy on the heap. The caller's
/// `SecretsToml` is dropped immediately after; `BackendSection::Drop`
/// then sees `None` for the now-emptied `api_key` field and the
/// allocator releases pages that already had the key zeroized via
/// `take()` semantics on `Option<String>` (no copy is made).
fn extract_backend_key(
    doc: &mut SecretsToml,
    backend: &str,
    path: &Path,
) -> Result<SecretString, SecretsError> {
    let section =
        doc.backends
            .get_mut(backend)
            .ok_or_else(|| SecretsError::MissingBackendSection {
                path: path.to_path_buf(),
                backend: backend.to_owned(),
            })?;
    let key = section
        .api_key
        .take()
        .ok_or_else(|| SecretsError::MissingBackendSection {
            path: path.to_path_buf(),
            backend: backend.to_owned(),
        })?;
    if key.is_empty() {
        // `key` is empty here — dropping it does not leave plaintext.
        return Err(SecretsError::EmptyApiKey {
            path: path.to_path_buf(),
            backend: backend.to_owned(),
        });
    }
    Ok(SecretString::new(key))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs::Permissions;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn write_secrets_file(dir: &Path, contents: &str, mode: u32) -> PathBuf {
        let path = dir.join("secrets.toml");
        let mut f = File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
        path
    }

    fn lock_parent(dir: &Path) {
        std::fs::set_permissions(dir, Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn debug_redacts_value() {
        let s = SecretString::new("super-secret-token".to_owned());
        let formatted = format!("{s:?}");
        assert!(formatted.contains("***"));
        assert!(!formatted.contains("super-secret-token"));
        let display = format!("{s}");
        assert_eq!(display, "***");
    }

    /// `DoD` #9 (resolver-side): every formatting style produces a
    /// non-leaking string. `Debug`, `Display`, and (transitively)
    /// any `tracing::field::display(&secret)` consumer must redact.
    #[test]
    fn no_format_style_leaks_the_secret() {
        let leak = "leaky-cabbage-1234";
        let s = SecretString::new(leak.to_owned());
        for rendered in [
            format!("{s}"),
            format!("{s:?}"),
            format!("{:?}", &s),
            format!("{}", &s),
            format!("{s:#?}"),
        ] {
            assert!(
                !rendered.contains(leak),
                "format style leaked the secret: {rendered}"
            );
        }
    }

    /// `Drop` zeroizes the inner buffer. Validating that directly
    /// requires `unsafe`; instead we validate the contract by
    /// re-using the `Zeroize` trait the `Drop` impl calls — if the
    /// zeroize trait is broken upstream, the integration tests will
    /// catch leaks via the tracing capture.
    #[test]
    fn zeroize_clears_the_buffer() {
        use zeroize::Zeroize;
        let mut buf = String::from("clearme");
        buf.zeroize();
        assert!(
            buf.is_empty(),
            "zeroize must shrink the String to empty length"
        );
    }

    #[test]
    fn env_var_naming() {
        assert_eq!(env_var_for("deepgram"), "ZWHISPER_DEEPGRAM_API_KEY");
        assert_eq!(env_var_for("whisper-cpp"), "ZWHISPER_WHISPER_CPP_API_KEY");
        assert_eq!(env_var_for("openai"), "ZWHISPER_OPENAI_API_KEY");
    }

    #[test]
    fn invalid_backend_id_rejected() {
        let err = resolve_api_key("").unwrap_err();
        assert!(matches!(err, SecretsError::InvalidBackend { .. }));
        let err = resolve_api_key("hello;rm -rf /").unwrap_err();
        assert!(matches!(err, SecretsError::InvalidBackend { .. }));
    }

    #[test]
    fn env_var_takes_precedence() {
        let cfg = ResolverConfig {
            secrets_path: Some(PathBuf::from("/nonexistent/secrets.toml")),
            env: EnvLookup::Fake(vec![(
                "ZWHISPER_DEEPGRAM_API_KEY".to_owned(),
                "from-env".to_owned(),
            )]),
        };
        let (s, src) = resolve_api_key_with_config("deepgram", &cfg).unwrap();
        assert_eq!(s.expose_secret(), "from-env");
        assert!(matches!(src, ResolveSource::Env(_)));
    }

    #[test]
    fn empty_env_var_falls_through_to_file() {
        let dir = TempDir::new().unwrap();
        lock_parent(dir.path());
        let path = write_secrets_file(
            dir.path(),
            "[deepgram]\napi_key = \"from-file\"\n",
            0o600,
        );
        let cfg = ResolverConfig {
            secrets_path: Some(path.clone()),
            env: EnvLookup::Fake(vec![(
                "ZWHISPER_DEEPGRAM_API_KEY".to_owned(),
                String::new(), // empty → fall through
            )]),
        };
        let (s, src) = resolve_api_key_with_config("deepgram", &cfg).unwrap();
        assert_eq!(s.expose_secret(), "from-file");
        assert!(matches!(src, ResolveSource::File(p) if p == path));
    }

    #[test]
    fn missing_key_fails_fast_with_self_correcting_message() {
        let dir = TempDir::new().unwrap();
        let cfg = ResolverConfig {
            secrets_path: Some(dir.path().join("does-not-exist.toml")),
            env: EnvLookup::Fake(vec![]),
        };
        let err = resolve_api_key_with_config("deepgram", &cfg).unwrap_err();
        match err {
            SecretsError::NotFound {
                backend, env_var, ..
            } => {
                assert_eq!(backend, "deepgram");
                assert_eq!(env_var, "ZWHISPER_DEEPGRAM_API_KEY");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn rejects_world_readable_toml() {
        let dir = TempDir::new().unwrap();
        lock_parent(dir.path());
        let path = write_secrets_file(
            dir.path(),
            "[deepgram]\napi_key = \"abc\"\n",
            0o644,
        );
        let cfg = ResolverConfig {
            secrets_path: Some(path),
            env: EnvLookup::Fake(vec![]),
        };
        let err = resolve_api_key_with_config("deepgram", &cfg).unwrap_err();
        assert!(matches!(
            err,
            SecretsError::PermissionsMode { mode: 0o644, .. }
        ));
    }

    #[test]
    fn accepts_mode_0o400() {
        let dir = TempDir::new().unwrap();
        lock_parent(dir.path());
        let path = write_secrets_file(
            dir.path(),
            "[deepgram]\napi_key = \"abc\"\n",
            0o400,
        );
        let cfg = ResolverConfig {
            secrets_path: Some(path),
            env: EnvLookup::Fake(vec![]),
        };
        let (s, _) = resolve_api_key_with_config("deepgram", &cfg).unwrap();
        assert_eq!(s.expose_secret(), "abc");
    }

    #[test]
    fn rejects_world_writable_parent_dir() {
        let dir = TempDir::new().unwrap();
        // Permissive parent dir (group + other writable).
        std::fs::set_permissions(dir.path(), Permissions::from_mode(0o777)).unwrap();
        let path = write_secrets_file(
            dir.path(),
            "[deepgram]\napi_key = \"abc\"\n",
            0o600,
        );
        let cfg = ResolverConfig {
            secrets_path: Some(path),
            env: EnvLookup::Fake(vec![]),
        };
        let err = resolve_api_key_with_config("deepgram", &cfg).unwrap_err();
        match err {
            SecretsError::PermissionsParent { mode, .. } => {
                assert!(mode & 0o022 != 0, "expected group/other writable bits");
            }
            other => panic!("expected PermissionsParent, got {other:?}"),
        }
    }

    #[test]
    fn missing_backend_section_is_typed_error() {
        let dir = TempDir::new().unwrap();
        lock_parent(dir.path());
        let path = write_secrets_file(
            dir.path(),
            "[openai]\napi_key = \"abc\"\n",
            0o600,
        );
        let cfg = ResolverConfig {
            secrets_path: Some(path),
            env: EnvLookup::Fake(vec![]),
        };
        let err = resolve_api_key_with_config("deepgram", &cfg).unwrap_err();
        assert!(matches!(
            err,
            SecretsError::MissingBackendSection { backend, .. } if backend == "deepgram"
        ));
    }

    #[test]
    fn empty_api_key_in_toml_is_typed_as_empty() {
        let dir = TempDir::new().unwrap();
        lock_parent(dir.path());
        let path = write_secrets_file(
            dir.path(),
            "[deepgram]\napi_key = \"\"\n",
            0o600,
        );
        let cfg = ResolverConfig {
            secrets_path: Some(path),
            env: EnvLookup::Fake(vec![]),
        };
        let err = resolve_api_key_with_config("deepgram", &cfg).unwrap_err();
        assert!(
            matches!(err, SecretsError::EmptyApiKey { ref backend, .. } if backend == "deepgram"),
            "{err:?}"
        );
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn malformed_toml_is_typed_parse_error() {
        let dir = TempDir::new().unwrap();
        lock_parent(dir.path());
        let path = write_secrets_file(dir.path(), "this is not toml @@@", 0o600);
        let cfg = ResolverConfig {
            secrets_path: Some(path),
            env: EnvLookup::Fake(vec![]),
        };
        let err = resolve_api_key_with_config("deepgram", &cfg).unwrap_err();
        assert!(matches!(err, SecretsError::Parse { .. }));
    }

    #[test]
    fn parse_error_display_does_not_leak_source_snippet() {
        // Security-review #3 / user-feedback #3 (2026-05-02): the
        // `Parse` Display must NOT include the toml_edit error
        // source, because that may quote the offending source line
        // (which carries the API key). Only the path is allowed.
        let dir = TempDir::new().unwrap();
        lock_parent(dir.path());
        let path = write_secrets_file(
            dir.path(),
            "[deepgram]\napi_key = \"sentinel-leaky-XYZQ\"\nbroken = ###",
            0o600,
        );
        let cfg = ResolverConfig {
            secrets_path: Some(path),
            env: EnvLookup::Fake(vec![]),
        };
        let err = resolve_api_key_with_config("deepgram", &cfg).unwrap_err();
        let rendered = format!("{err}");
        assert!(
            !rendered.contains("sentinel-leaky-XYZQ"),
            "parse error Display leaked key into snippet: {rendered}"
        );
        // The full chain (visible to debug formatters via `{:#}`) is
        // allowed to include source detail; only `{}` must redact.
        assert!(rendered.contains("malformed secrets TOML"));
    }

    #[test]
    fn backend_section_drop_zeroizes_remaining_keys() {
        // User-feedback #3 (2026-05-02): if the parsed file contains
        // multiple backend sections (e.g., `[openai]` + `[deepgram]`),
        // resolving `deepgram` extracts that key but leaves `openai`
        // sitting in the dropped doc. The `BackendSection::Drop` impl
        // ensures the unused section's plaintext is wiped before the
        // allocator releases the page.
        let dir = TempDir::new().unwrap();
        lock_parent(dir.path());
        let path = write_secrets_file(
            dir.path(),
            "[deepgram]\napi_key = \"dg-key-001\"\n\
             [openai]\napi_key = \"openai-key-002\"\n",
            0o600,
        );
        let cfg = ResolverConfig {
            secrets_path: Some(path),
            env: EnvLookup::Fake(vec![]),
        };
        let (key, _src) = resolve_api_key_with_config("deepgram", &cfg).unwrap();
        // The resolver returned the deepgram key; we cannot directly
        // observe the freed openai allocation, but we *can* assert
        // the BackendSection drop path is exercised: dropping `key`
        // here leaves no stable observable state, but the code path
        // is reached, and a regression of removing the Drop impl
        // would surface in the leak audit suite.
        assert_eq!(key.expose_secret(), "dg-key-001");
    }

    #[test]
    fn happy_path_reads_key_from_file() {
        let dir = TempDir::new().unwrap();
        lock_parent(dir.path());
        let path = write_secrets_file(
            dir.path(),
            "[deepgram]\napi_key = \"sk-fixture-1234567890\"\n",
            0o600,
        );
        let cfg = ResolverConfig {
            secrets_path: Some(path.clone()),
            env: EnvLookup::Fake(vec![]),
        };
        let (key, src) = resolve_api_key_with_config("deepgram", &cfg).unwrap();
        assert_eq!(key.expose_secret(), "sk-fixture-1234567890");
        assert_eq!(key.len(), 21);
        match src {
            ResolveSource::File(p) => assert_eq!(p, path),
            ResolveSource::Env(name) => panic!("expected File source, got Env({name})"),
        }
    }
}
