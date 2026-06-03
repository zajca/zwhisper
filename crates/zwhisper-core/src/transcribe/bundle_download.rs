//! Directory-bundle model download (RFC Phase 3): `MultiFile` and
//! `Archive` model sources, with the unified security contract.
//!
//! The single-file whisper downloader lives in [`super::model_management`]
//! and is unchanged. This module adds the two directory-bundle transports
//! and preserves every download invariant, none silently dropped:
//!
//! - **HTTPS-only, enforced at the client.** The `reqwest` client is
//!   built with `https_only(true)` AND a custom redirect policy that
//!   rejects any hop whose scheme is not `https` — a 3xx redirect to a
//!   non-HTTPS URL is refused, never followed (CWE-757).
//! - **Content-type guard.** `text/html` and executable MIME types are
//!   rejected for every kind; Archive additionally requires an
//!   archive/octet media type. (`text/plain` is allowed for `MultiFile`
//!   because bundles legitimately ship `vocab.txt`/`config.json`;
//!   SHA-256 is the authoritative integrity guard.)
//! - **SHA-256 + size verification BEFORE install.** Every file (and,
//!   for `Archive`, the archive) is verified before any extraction or
//!   the atomic directory swap. For `Archive`, no extractor byte is read
//!   until the archive's hash matches.
//! - **Same-filesystem atomic directory install.** Staging lives at
//!   `models_dir/.partial/<dir_name>.staging/` — co-located with the
//!   final bundle path, so the final `rename(2)` is a true atomic swap
//!   with no cross-device copy fallback. A failed/cancelled install
//!   leaves the final bundle path untouched and removes the staging tree.
//! - **Archive extraction** is delegated to [`super::archive_extract`],
//!   which performs lexical zip-slip rejection, symlink/hardlink
//!   rejection, and byte/entry bomb caps.

use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Client;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderMap};
use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::{self as tokio_fs, File};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::archive_extract::{self, ExtractLimits};
use super::model::{ModelKind, ModelSource, ModelSpec, RemoteFile};
use super::model_management::hex_lower;

const USER_AGENT: &str = concat!("zwhisper/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_INACTIVITY: Duration = Duration::from_secs(120);
const MAX_REDIRECTS: usize = 10;
const PARTIAL_SUBDIR: &str = ".partial";
const STAGING_SUFFIX: &str = ".staging";
/// Per-entry uncompressed cap for archive extraction (bundles are model
/// weights — large single files are expected, but a single multi-GiB
/// entry past this is refused). Derived from the archive's total cap.
const ARCHIVE_ENTRY_FRACTION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleProgress {
    Resolving,
    Fetching {
        file: String,
        bytes_done: u64,
        total: u64,
    },
    Verifying {
        file: String,
    },
    Extracting,
    Installing,
    Installed,
    Cancelled,
}

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("bundle spec is not a directory bundle")]
    NotADirectoryBundle,

    #[error("bundle source `{0}` is not downloadable here")]
    UnsupportedSource(&'static str),

    #[error("refusing to download into non-absolute models_dir {0:?}")]
    NonAbsoluteModelsDir(PathBuf),

    #[error("network error fetching {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: Box<reqwest::Error>,
    },

    #[error("HTTP {status} fetching {url}")]
    Http { url: String, status: u16 },

    #[error("rejected content-type {content_type:?} for {url}")]
    ContentType { url: String, content_type: String },

    #[error("size mismatch for {url}: expected {expected}, got {actual}")]
    SizeMismatch {
        url: String,
        expected: u64,
        actual: u64,
    },

    #[error("SHA-256 mismatch for {url}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },

    #[error("download stalled past the inactivity timeout for {url}")]
    Stalled { url: String },

    #[error("archive extraction failed: {0}")]
    Extract(#[from] archive_extract::ExtractError),

    #[error("installed bundle is incomplete; missing {missing:?}")]
    BundleIncomplete { missing: Vec<String> },

    #[error("download cancelled")]
    Cancelled,

    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// One bundle install job, built from a [`ModelSpec`] whose kind is a
/// [`ModelKind::DirectoryBundle`] and whose source is `MultiFile` or
/// `Archive`.
#[derive(Debug)]
pub struct BundleInstaller {
    dir_name: String,
    expected_files: Vec<String>,
    source: ModelSource,
    models_dir: PathBuf,
    cancel: CancellationToken,
    client: Client,
}

impl BundleInstaller {
    pub fn for_spec(
        spec: &ModelSpec,
        models_dir: PathBuf,
        cancel: CancellationToken,
    ) -> Result<Self, BundleError> {
        let (dir_name, expected_files) = match &spec.kind {
            ModelKind::DirectoryBundle {
                dir_name,
                expected_files,
            } => (dir_name.clone(), expected_files.clone()),
            _ => return Err(BundleError::NotADirectoryBundle),
        };
        if !spec.source.is_remote_download() {
            return Err(BundleError::UnsupportedSource(match spec.source {
                ModelSource::None => "None",
                ModelSource::LocalPath { .. } => "LocalPath",
                _ => "unknown",
            }));
        }
        if models_dir.as_os_str().is_empty() || !models_dir.is_absolute() {
            return Err(BundleError::NonAbsoluteModelsDir(models_dir));
        }
        Ok(Self {
            dir_name,
            expected_files,
            source: spec.source.clone(),
            models_dir,
            cancel,
            client: build_https_only_client()?,
        })
    }

    /// Test seam: build with a caller-supplied client (e.g. one that
    /// accepts a loopback HTTP wiremock URL).
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn for_spec_with_client(
        spec: &ModelSpec,
        models_dir: PathBuf,
        cancel: CancellationToken,
        client: Client,
    ) -> Result<Self, BundleError> {
        let mut me = Self::for_spec_no_client(spec, models_dir, cancel)?;
        me.client = client;
        Ok(me)
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn for_spec_no_client(
        spec: &ModelSpec,
        models_dir: PathBuf,
        cancel: CancellationToken,
    ) -> Result<Self, BundleError> {
        let (dir_name, expected_files) = match &spec.kind {
            ModelKind::DirectoryBundle {
                dir_name,
                expected_files,
            } => (dir_name.clone(), expected_files.clone()),
            _ => return Err(BundleError::NotADirectoryBundle),
        };
        Ok(Self {
            dir_name,
            expected_files,
            source: spec.source.clone(),
            models_dir,
            cancel,
            client: Client::new(),
        })
    }

    pub fn final_dir(&self) -> PathBuf {
        self.models_dir.join(&self.dir_name)
    }

    fn staging_dir(&self) -> PathBuf {
        self.models_dir
            .join(PARTIAL_SUBDIR)
            .join(format!("{}{STAGING_SUFFIX}", self.dir_name))
    }

    /// Download, verify, and atomically install the bundle. Returns the
    /// final bundle directory on success.
    pub async fn install(
        &self,
        tx: Option<UnboundedSender<BundleProgress>>,
    ) -> Result<PathBuf, BundleError> {
        let send = |p: BundleProgress| {
            if let Some(tx) = &tx {
                let _ = tx.send(p);
            }
        };
        send(BundleProgress::Resolving);

        let final_dir = self.final_dir();
        if self.bundle_complete(&final_dir) {
            send(BundleProgress::Installed);
            return Ok(final_dir);
        }

        let staging = self.staging_dir();
        // Always start from a clean staging tree.
        let _ = tokio_fs::remove_dir_all(&staging).await;
        tokio_fs::create_dir_all(&staging)
            .await
            .map_err(|e| self.io_err(&staging, e))?;

        let result = match &self.source {
            ModelSource::MultiFile { files } => {
                self.install_multifile(files, &staging, &send).await
            }
            ModelSource::Archive {
                url,
                sha256,
                size_bytes,
                max_unpacked_bytes,
                max_entry_count,
            } => {
                self.install_archive(
                    url,
                    sha256,
                    *size_bytes,
                    max_unpacked_bytes.get(),
                    max_entry_count.get(),
                    &staging,
                    &send,
                )
                .await
            }
            other => Err(BundleError::UnsupportedSource(match other {
                ModelSource::None => "None",
                ModelSource::LocalPath { .. } => "LocalPath",
                _ => "unknown",
            })),
        };

        if let Err(e) = result {
            // Leave the final bundle untouched; remove the staging tree.
            let _ = tokio_fs::remove_dir_all(&staging).await;
            return Err(e);
        }

        // Validate completeness before the atomic swap.
        let missing = self.missing_files(&staging);
        if !missing.is_empty() {
            let _ = tokio_fs::remove_dir_all(&staging).await;
            return Err(BundleError::BundleIncomplete { missing });
        }

        send(BundleProgress::Installing);
        self.atomic_install(&staging, &final_dir).await?;
        send(BundleProgress::Installed);
        Ok(final_dir)
    }

    async fn install_multifile(
        &self,
        files: &[RemoteFile],
        staging: &Path,
        send: &impl Fn(BundleProgress),
    ) -> Result<(), BundleError> {
        for file in files {
            self.check_cancelled()?;
            // `relative_path` was allow-listed at registry load; join is
            // safe (no separators/traversal possible).
            let dest = staging.join(&file.relative_path);
            if let Some(parent) = dest.parent() {
                tokio_fs::create_dir_all(parent)
                    .await
                    .map_err(|e| self.io_err(parent, e))?;
            }
            self.download_verify_file(
                &file.url,
                &file.sha256,
                file.size_bytes,
                &dest,
                ContentKind::File,
                &file.relative_path,
                send,
            )
            .await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn install_archive(
        &self,
        url: &str,
        sha256: &str,
        size_bytes: u64,
        max_unpacked_bytes: u64,
        max_entry_count: u64,
        staging: &Path,
        send: &impl Fn(BundleProgress),
    ) -> Result<(), BundleError> {
        // 1. Stream the archive to a co-located .part file and verify
        //    SHA-256 + size BEFORE any extractor byte is read.
        let part = staging.join(".archive.part");
        self.download_verify_file(
            url,
            sha256,
            size_bytes,
            &part,
            ContentKind::Archive(archive_format_for(url)),
            "archive",
            send,
        )
        .await?;

        // 2. Extract into the staging dir under the bomb caps.
        send(BundleProgress::Extracting);
        let limits = ExtractLimits {
            max_unpacked_bytes,
            max_entries: max_entry_count,
            // Allow a single entry up to the whole unpacked budget
            // (model weights are large single files).
            max_entry_bytes: max_unpacked_bytes.max(ARCHIVE_ENTRY_FRACTION),
        };
        let format = archive_format_for(url);
        let part_for_blocking = part.clone();
        let staging_for_blocking = staging.to_path_buf();
        // Extraction is synchronous CPU + fs work; run it on the blocking
        // pool so the async runtime keeps moving.
        let extract = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&part_for_blocking).map_err(|e| {
                archive_extract::ExtractError::Io(format!(
                    "open archive {}: {e}",
                    part_for_blocking.display()
                ))
            })?;
            match format {
                ArchiveFormat::Zip => {
                    archive_extract::extract_zip(file, &staging_for_blocking, &limits)
                }
                ArchiveFormat::TarGz => {
                    archive_extract::extract_tar_gz(file, &staging_for_blocking, &limits)
                }
            }
        })
        .await
        .map_err(|join| BundleError::Io {
            path: part.display().to_string(),
            source: std::io::Error::other(format!("extraction task panicked: {join}")),
        })?;
        extract?;

        // 3. The verified archive is no longer needed.
        let _ = tokio_fs::remove_file(&part).await;
        Ok(())
    }

    /// Stream `url` to `dest`, validating content-type, size, and
    /// SHA-256. Nothing is trusted until the hash matches.
    #[allow(clippy::too_many_arguments)]
    async fn download_verify_file(
        &self,
        url: &str,
        expected_sha: &str,
        expected_size: u64,
        dest: &Path,
        kind: ContentKind,
        label: &str,
        send: &impl Fn(BundleProgress),
    ) -> Result<(), BundleError> {
        self.check_cancelled()?;
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| self.net_err(url, e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(BundleError::Http {
                url: url.to_owned(),
                status: status.as_u16(),
            });
        }
        validate_content_type(resp.headers(), kind, url)?;
        if let Some(declared) = content_length(resp.headers()) {
            if declared != expected_size {
                return Err(BundleError::SizeMismatch {
                    url: url.to_owned(),
                    expected: expected_size,
                    actual: declared,
                });
            }
        }

        let mut resp = resp;
        let mut file = File::create(dest).await.map_err(|e| self.io_err(dest, e))?;
        let mut hasher = Sha256::new();
        let mut bytes_done: u64 = 0;
        loop {
            let chunk = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Err(BundleError::Cancelled),
                c = tokio::time::timeout(STREAM_INACTIVITY, resp.chunk()) => match c {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => return Err(self.net_err(url, e)),
                    Err(_) => return Err(BundleError::Stalled { url: url.to_owned() }),
                },
            };
            let Some(bytes) = chunk else { break };
            if bytes.is_empty() {
                continue;
            }
            hasher.update(&bytes);
            file.write_all(&bytes)
                .await
                .map_err(|e| self.io_err(dest, e))?;
            bytes_done = bytes_done.saturating_add(bytes.len() as u64);
            send(BundleProgress::Fetching {
                file: label.to_owned(),
                bytes_done,
                total: expected_size,
            });
        }
        file.flush().await.map_err(|e| self.io_err(dest, e))?;
        file.sync_data().await.map_err(|e| self.io_err(dest, e))?;

        send(BundleProgress::Verifying {
            file: label.to_owned(),
        });
        if bytes_done != expected_size {
            let _ = tokio_fs::remove_file(dest).await;
            return Err(BundleError::SizeMismatch {
                url: url.to_owned(),
                expected: expected_size,
                actual: bytes_done,
            });
        }
        let actual = hex_lower(&hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected_sha) {
            let _ = tokio_fs::remove_file(dest).await;
            return Err(BundleError::ChecksumMismatch {
                url: url.to_owned(),
                expected: expected_sha.to_owned(),
                actual,
            });
        }
        Ok(())
    }

    async fn atomic_install(&self, staging: &Path, final_dir: &Path) -> Result<(), BundleError> {
        if let Some(parent) = final_dir.parent() {
            tokio_fs::create_dir_all(parent)
                .await
                .map_err(|e| self.io_err(parent, e))?;
        }
        // If a (corrupt/incomplete) final dir exists, remove it first —
        // rename onto a non-empty dir fails. The tiny non-atomic window
        // only affects re-installing an already-broken bundle.
        if final_dir.exists() {
            tokio_fs::remove_dir_all(final_dir)
                .await
                .map_err(|e| self.io_err(final_dir, e))?;
        }
        // Same filesystem (both under models_dir) → atomic rename.
        tokio_fs::rename(staging, final_dir)
            .await
            .map_err(|e| self.io_err(final_dir, e))
    }

    fn bundle_complete(&self, dir: &Path) -> bool {
        dir.is_dir() && self.missing_files(dir).is_empty()
    }

    fn missing_files(&self, dir: &Path) -> Vec<String> {
        self.expected_files
            .iter()
            .filter(|f| !dir.join(f).is_file())
            .cloned()
            .collect()
    }

    fn check_cancelled(&self) -> Result<(), BundleError> {
        if self.cancel.is_cancelled() {
            Err(BundleError::Cancelled)
        } else {
            Ok(())
        }
    }

    #[allow(clippy::unused_self)] // method form keeps call sites uniform (`self.io_err(..)`).
    fn io_err(&self, path: &Path, source: std::io::Error) -> BundleError {
        BundleError::Io {
            path: path.display().to_string(),
            source,
        }
    }

    #[allow(clippy::unused_self)]
    fn net_err(&self, url: &str, source: reqwest::Error) -> BundleError {
        BundleError::Network {
            url: url.to_owned(),
            source: Box::new(source),
        }
    }
}

/// Build the `reqwest` client used for ALL bundle downloads:
/// `https_only(true)` plus a custom redirect policy that refuses any hop
/// whose scheme is not `https` (CWE-757 — a redirect must never
/// downgrade TLS).
fn build_https_only_client() -> Result<Client, BundleError> {
    let redirect = Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error(std::io::Error::other("too many redirects"));
        }
        if attempt.url().scheme() != "https" {
            return attempt.error(std::io::Error::other(
                "refusing to follow a non-HTTPS redirect (TLS downgrade)",
            ));
        }
        attempt.follow()
    });
    Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .https_only(true)
        .redirect(redirect)
        .use_rustls_tls()
        .build()
        .map_err(|e| BundleError::Network {
            url: "<client-build>".to_owned(),
            source: Box::new(e),
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFormat {
    Zip,
    TarGz,
}

#[derive(Debug, Clone, Copy)]
enum ContentKind {
    File,
    Archive(ArchiveFormat),
}

fn archive_format_for(url: &str) -> ArchiveFormat {
    // Split off a query/fragment, then compare the final extension
    // case-insensitively. `.zip` is the only ZIP form; everything else
    // (.tar.gz / .tgz / unknown) is treated as tar.gz.
    let trimmed = url.split(['?', '#']).next().unwrap_or(url);
    let ext = trimmed.rsplit('.').next().map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("zip") => ArchiveFormat::Zip,
        _ => ArchiveFormat::TarGz,
    }
}

const OCTET_TYPES: &[&str] = &[
    "application/octet-stream",
    "application/x-binary",
    "binary/octet-stream",
];
const EXECUTABLE_TYPES: &[&str] = &[
    "application/x-executable",
    "application/x-msdownload",
    "application/x-dosexec",
    "application/vnd.microsoft.portable-executable",
    "application/x-mach-binary",
    "application/x-elf",
    "application/x-sharedlib",
];

fn validate_content_type(
    headers: &HeaderMap,
    kind: ContentKind,
    url: &str,
) -> Result<(), BundleError> {
    let raw = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bare = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    let reject = || BundleError::ContentType {
        url: url.to_owned(),
        content_type: raw.clone(),
    };

    // Dangerous types are refused for every kind.
    if bare == "text/html" || EXECUTABLE_TYPES.contains(&bare.as_str()) {
        return Err(reject());
    }
    // A missing/empty content-type is tolerated (SHA-256 is the
    // authoritative guard); the server simply omitted it.
    if bare.is_empty() {
        return Ok(());
    }
    match kind {
        ContentKind::File => {
            // octet-stream set + plain text (vocab.txt/config.json).
            if OCTET_TYPES.contains(&bare.as_str()) || bare == "text/plain" {
                Ok(())
            } else {
                Err(reject())
            }
        }
        ContentKind::Archive(format) => {
            if OCTET_TYPES.contains(&bare.as_str())
                || archive_media_types(format).contains(&bare.as_str())
            {
                Ok(())
            } else {
                Err(reject())
            }
        }
    }
}

fn archive_media_types(format: ArchiveFormat) -> &'static [&'static str] {
    match format {
        ArchiveFormat::Zip => &["application/zip", "application/x-zip-compressed"],
        ArchiveFormat::TarGz => &[
            "application/gzip",
            "application/x-gzip",
            "application/x-tar",
            "application/x-compressed-tar",
        ],
    }
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::io::Write;
    use std::num::NonZeroU64;

    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::profile::schema::Backend;
    use crate::transcribe::model::{ModelKind, RuntimeMeta};

    fn loopback_client() -> Client {
        // Mirrors the production client (custom redirect policy) but
        // without https_only so a loopback http:// wiremock URL works.
        let redirect = Policy::custom(|attempt| {
            if attempt.url().scheme() == "https"
                || attempt.url().host_str() == Some("127.0.0.1")
                || attempt.url().host_str() == Some("localhost")
            {
                attempt.follow()
            } else {
                attempt.error(std::io::Error::other("non-https redirect"))
            }
        });
        Client::builder()
            .user_agent(USER_AGENT)
            .redirect(redirect)
            .build()
            .unwrap()
    }

    fn multifile_spec(server: &str, files: Vec<RemoteFile>) -> ModelSpec {
        let expected_files = files.iter().map(|f| f.relative_path.clone()).collect();
        let _ = server;
        ModelSpec {
            id: "test-bundle".to_owned(),
            backend: Backend::Parakeet,
            kind: ModelKind::DirectoryBundle {
                dir_name: "test-bundle".to_owned(),
                expected_files,
            },
            source: ModelSource::MultiFile { files },
            languages: vec![],
            runtime: RuntimeMeta::default(),
        }
    }

    fn remote_file(server: &str, name: &str, body: &[u8]) -> RemoteFile {
        RemoteFile {
            relative_path: name.to_owned(),
            url: format!("{server}/{name}"),
            sha256: hex_lower(&Sha256::digest(body)),
            size_bytes: body.len() as u64,
        }
    }

    async fn mount_file(server: &MockServer, name: &str, body: Vec<u8>, content_type: &str) {
        Mock::given(method("GET"))
            .and(wm_path(format!("/{name}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", content_type)
                    .insert_header("Content-Length", body.len().to_string())
                    .set_body_bytes(body),
            )
            .mount(server)
            .await;
    }

    fn installer(spec: &ModelSpec, models_dir: PathBuf) -> BundleInstaller {
        BundleInstaller::for_spec_with_client(
            spec,
            models_dir,
            CancellationToken::new(),
            loopback_client(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn multifile_happy_path_installs_and_is_atomic() {
        let server = MockServer::start().await;
        let enc = b"encoder-weights".to_vec();
        let vocab = b"a\nb\nc\n".to_vec();
        mount_file(
            &server,
            "encoder.onnx",
            enc.clone(),
            "application/octet-stream",
        )
        .await;
        // vocab served as text/plain — must be accepted for MultiFile.
        mount_file(
            &server,
            "vocab.txt",
            vocab.clone(),
            "text/plain; charset=utf-8",
        )
        .await;

        let spec = multifile_spec(
            &server.uri(),
            vec![
                remote_file(&server.uri(), "encoder.onnx", &enc),
                remote_file(&server.uri(), "vocab.txt", &vocab),
            ],
        );
        let dir = TempDir::new().unwrap();
        let inst = installer(&spec, dir.path().to_path_buf());

        let (tx, mut rx) = mpsc::unbounded_channel();
        let final_dir = inst.install(Some(tx)).await.unwrap();

        assert_eq!(std::fs::read(final_dir.join("encoder.onnx")).unwrap(), enc);
        assert_eq!(std::fs::read(final_dir.join("vocab.txt")).unwrap(), vocab);
        // Staging tree removed after the atomic swap.
        assert!(
            !dir.path()
                .join(".partial")
                .join("test-bundle.staging")
                .exists()
        );
        let mut saw_installed = false;
        while let Ok(p) = rx.try_recv() {
            saw_installed |= p == BundleProgress::Installed;
        }
        assert!(saw_installed);
    }

    #[tokio::test]
    async fn multifile_checksum_mismatch_rejects_and_leaves_no_final() {
        let server = MockServer::start().await;
        let body = b"real".to_vec();
        mount_file(&server, "f.onnx", body.clone(), "application/octet-stream").await;
        // Declare a wrong sha256 in the spec.
        let mut file = remote_file(&server.uri(), "f.onnx", &body);
        file.sha256 = "0".repeat(64);
        let spec = multifile_spec(&server.uri(), vec![file]);

        let dir = TempDir::new().unwrap();
        let inst = installer(&spec, dir.path().to_path_buf());
        let err = inst.install(None).await.unwrap_err();
        assert!(matches!(err, BundleError::ChecksumMismatch { .. }));
        assert!(!inst.final_dir().exists(), "no final bundle on failure");
    }

    #[tokio::test]
    async fn html_content_type_rejected() {
        let server = MockServer::start().await;
        let body = b"<html>nope</html>".to_vec();
        mount_file(&server, "f.onnx", body.clone(), "text/html").await;
        let spec = multifile_spec(
            &server.uri(),
            vec![remote_file(&server.uri(), "f.onnx", &body)],
        );
        let dir = TempDir::new().unwrap();
        let inst = installer(&spec, dir.path().to_path_buf());
        let err = inst.install(None).await.unwrap_err();
        assert!(
            matches!(err, BundleError::ContentType { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn archive_targz_happy_path_extracts_and_installs() {
        // Build a real tar.gz containing the two expected files.
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in [
            ("encoder.onnx", b"W".repeat(32)),
            ("vocab.txt", b"v".repeat(8)),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            builder.append_data(&mut h, name, &data[..]).unwrap();
        }
        let tar_bytes = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        let archive = gz.finish().unwrap();
        let sha = hex_lower(&Sha256::digest(&archive));

        let server = MockServer::start().await;
        mount_file(
            &server,
            "bundle.tar.gz",
            archive.clone(),
            "application/gzip",
        )
        .await;

        let spec = ModelSpec {
            id: "arch-bundle".to_owned(),
            backend: Backend::Parakeet,
            kind: ModelKind::DirectoryBundle {
                dir_name: "arch-bundle".to_owned(),
                expected_files: vec!["encoder.onnx".to_owned(), "vocab.txt".to_owned()],
            },
            source: ModelSource::Archive {
                url: format!("{}/bundle.tar.gz", server.uri()),
                sha256: sha,
                size_bytes: archive.len() as u64,
                max_unpacked_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
                max_entry_count: NonZeroU64::new(16).unwrap(),
            },
            languages: vec![],
            runtime: RuntimeMeta::default(),
        };
        let dir = TempDir::new().unwrap();
        let inst = installer(&spec, dir.path().to_path_buf());
        let final_dir = inst.install(None).await.unwrap();
        assert_eq!(
            std::fs::read(final_dir.join("encoder.onnx")).unwrap(),
            b"W".repeat(32)
        );
        assert_eq!(
            std::fs::read(final_dir.join("vocab.txt")).unwrap(),
            b"v".repeat(8)
        );
        assert!(
            !dir.path()
                .join(".partial")
                .join("arch-bundle.staging")
                .exists()
        );
    }

    #[tokio::test]
    async fn archive_checksum_mismatch_never_extracts() {
        let server = MockServer::start().await;
        let archive = b"not really a valid archive but never extracted".to_vec();
        mount_file(
            &server,
            "bundle.tar.gz",
            archive.clone(),
            "application/gzip",
        )
        .await;
        let spec = ModelSpec {
            id: "arch-bundle".to_owned(),
            backend: Backend::Parakeet,
            kind: ModelKind::DirectoryBundle {
                dir_name: "arch-bundle".to_owned(),
                expected_files: vec!["x".to_owned()],
            },
            source: ModelSource::Archive {
                url: format!("{}/bundle.tar.gz", server.uri()),
                sha256: "0".repeat(64),
                size_bytes: archive.len() as u64,
                max_unpacked_bytes: NonZeroU64::new(1024).unwrap(),
                max_entry_count: NonZeroU64::new(4).unwrap(),
            },
            languages: vec![],
            runtime: RuntimeMeta::default(),
        };
        let dir = TempDir::new().unwrap();
        let inst = installer(&spec, dir.path().to_path_buf());
        let err = inst.install(None).await.unwrap_err();
        assert!(matches!(err, BundleError::ChecksumMismatch { .. }));
        assert!(!inst.final_dir().exists());
    }

    #[test]
    fn https_only_client_builds() {
        assert!(build_https_only_client().is_ok());
    }

    #[test]
    fn content_type_table() {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, "text/html".parse().unwrap());
        assert!(validate_content_type(&h, ContentKind::File, "u").is_err());

        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, "text/plain; charset=utf-8".parse().unwrap());
        assert!(validate_content_type(&h, ContentKind::File, "u").is_ok());

        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, "application/x-msdownload".parse().unwrap());
        assert!(validate_content_type(&h, ContentKind::File, "u").is_err());

        // Archive requires an archive/octet type, not text/plain.
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, "text/plain".parse().unwrap());
        assert!(
            validate_content_type(&h, ContentKind::Archive(ArchiveFormat::TarGz), "u").is_err()
        );
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, "application/gzip".parse().unwrap());
        assert!(validate_content_type(&h, ContentKind::Archive(ArchiveFormat::TarGz), "u").is_ok());
    }
}
