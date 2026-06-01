//! Whisper.cpp model manifest, local paths, and secure downloader.
//!
//! This module is intentionally CLI-friendly: it has no UI types, no
//! settings-crate dependency, and keeps the security invariants that
//! matter for downloading model weights:
//!
//! - a compile-time known manifest of model names, SHA-256 values, and
//!   expected byte lengths;
//! - HTTPS-only URL templates with exactly one `{model}` placeholder;
//! - downloads written to a co-located `.part` file under `.partial/`;
//! - SHA-256 verification before an atomic rename to the final
//!   `ggml-<model>.bin` path.

use std::collections::BTreeMap;
use std::fs::File as StdFile;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, RANGE};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::{self as tokio_fs, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::models;

const DEFAULT_BASE_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}.bin";
const MODEL_PLACEHOLDER: &str = "{model}";
const REQUIRED_SCHEME: &str = "https://";
const CONFIG_SUBDIR: &str = "zwhisper";
const CONFIG_FILENAME: &str = "models.toml";
const USER_AGENT: &str = concat!("zwhisper/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_INACTIVITY: Duration = Duration::from_secs(60);
const FLUSH_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
const PART_SUFFIX: &str = ".part";
const META_SUFFIX: &str = ".part.meta.json";
const PARTIAL_SUBDIR: &str = ".partial";
const RETRY_AFTER_MAX_SECS: u64 = 10 * 60;
const ALLOWED_CONTENT_TYPES: &[&str] = &[
    "application/octet-stream",
    "application/x-binary",
    "binary/octet-stream",
];

const EMBEDDED_MANIFEST: &str = r#"
[tiny]
sha256 = "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21"
size_bytes = 77691713

[base]
sha256 = "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe"
size_bytes = 147951465

[small]
sha256 = "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b"
size_bytes = 487601967

[medium]
sha256 = "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208"
size_bytes = 1533763059

[large-v3]
sha256 = "ad82bf6a9043ceed055076d0fd39f5f186ff8062ea4f4a09e96eaee9e6f74dde"
size_bytes = 3094623691
"#;

#[derive(Debug, Error)]
pub enum ModelManagementError {
    #[error("unknown model `{model}`; known models: {known}")]
    UnknownModel { model: String, known: String },

    #[error("model manifest parse failed: {0}")]
    Manifest(String),

    #[error("models config: {0}")]
    Config(String),

    #[error("model download: {0}")]
    Download(String),

    #[error("model path: {0}")]
    Path(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModelEntry {
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ModelManifest(BTreeMap<String, ModelEntry>);

impl ModelManifest {
    pub fn parse(toml_text: &str) -> Result<Self, ModelManagementError> {
        let map: BTreeMap<String, ModelEntry> = toml_edit::de::from_str(toml_text)
            .map_err(|e| ModelManagementError::Manifest(e.to_string()))?;
        Ok(Self(map))
    }

    pub fn embedded() -> &'static Self {
        static INSTANCE: OnceLock<ModelManifest> = OnceLock::new();
        INSTANCE.get_or_init(|| match Self::parse(EMBEDDED_MANIFEST) {
            Ok(manifest) => manifest,
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "embedded model manifest failed to parse; no models are available"
                );
                Self(BTreeMap::new())
            }
        })
    }

    pub fn lookup(&self, model_name: &str) -> Option<&ModelEntry> {
        self.0.get(model_name)
    }

    pub fn known_models(&self) -> impl Iterator<Item = (&str, &ModelEntry)> {
        self.0.iter().map(|(name, entry)| (name.as_str(), entry))
    }

    fn known_model_list(&self) -> String {
        self.0.keys().cloned().collect::<Vec<_>>().join(", ")
    }

    fn require(&self, model_name: &str) -> Result<&ModelEntry, ModelManagementError> {
        self.lookup(model_name)
            .ok_or_else(|| ModelManagementError::UnknownModel {
                model: model_name.to_owned(),
                known: self.known_model_list(),
            })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModelSourceConfig {
    pub base_url: String,
}

impl Default for ModelSourceConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
        }
    }
}

impl ModelSourceConfig {
    pub fn default_path() -> Option<PathBuf> {
        Some(
            dirs::config_dir()?
                .join(CONFIG_SUBDIR)
                .join(CONFIG_FILENAME),
        )
    }

    pub fn load_or_default(path: Option<&Path>) -> Result<Self, ModelManagementError> {
        let resolved = match path {
            Some(path) => path.to_path_buf(),
            None => match Self::default_path() {
                Some(path) => path,
                None => return Ok(Self::default()),
            },
        };

        if !resolved.is_file() {
            return Ok(Self::default());
        }

        let body = std::fs::read_to_string(&resolved).map_err(|e| {
            ModelManagementError::Config(format!("reading {}: {e}", resolved.display()))
        })?;
        let parsed: Self = toml_edit::de::from_str(&body).map_err(|e| {
            ModelManagementError::Config(format!("parsing {}: {e}", resolved.display()))
        })?;
        parsed.validate_base_url()?;
        Ok(parsed)
    }

    pub fn resolve_url(&self, model_name: &str) -> Result<String, ModelManagementError> {
        if model_name.is_empty() {
            return Err(ModelManagementError::Config(
                "model name must not be empty".to_owned(),
            ));
        }
        self.validate_base_url()?;
        Ok(self.base_url.replace(MODEL_PLACEHOLDER, model_name))
    }

    fn validate_base_url(&self) -> Result<(), ModelManagementError> {
        let url = &self.base_url;
        if !url.starts_with(REQUIRED_SCHEME) {
            return Err(ModelManagementError::Config(format!(
                "base_url must start with {REQUIRED_SCHEME}; got {url:?}"
            )));
        }

        let placeholder_count = url.matches(MODEL_PLACEHOLDER).count();
        if placeholder_count == 0 {
            return Err(ModelManagementError::Config(format!(
                "base_url must contain {MODEL_PLACEHOLDER} placeholder"
            )));
        }
        if placeholder_count > 1 {
            return Err(ModelManagementError::Config(format!(
                "base_url must contain {MODEL_PLACEHOLDER} exactly once \
                 (found {placeholder_count} occurrences)"
            )));
        }

        let stripped = url.replace(MODEL_PLACEHOLDER, "");
        if stripped.contains('{') || stripped.contains('}') {
            return Err(ModelManagementError::Config(format!(
                "base_url contains placeholders other than {MODEL_PLACEHOLDER}; only one is allowed"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadState {
    Resolving,
    Fetching { bytes_done: u64, total: u64 },
    Verifying,
    Installed,
    Failed { reason: FailReason },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    ContentTypeMismatch(String),
    ContentLengthMismatch { expected: u64, actual: u64 },
    Http(u16),
    RateLimited { retry_after_secs: u64 },
    Network(String),
    ChecksumMismatch,
    Io(String),
}

#[derive(Debug)]
pub struct ModelDownloader {
    model_name: String,
    entry: ModelEntry,
    url: String,
    final_path: PathBuf,
    part_path: PathBuf,
    meta_path: PathBuf,
    cancel: CancellationToken,
    client: Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelVerification {
    pub path: PathBuf,
    pub expected_sha256: String,
    pub actual_sha256: String,
    pub expected_size_bytes: u64,
    pub actual_size_bytes: u64,
}

impl ModelVerification {
    pub fn is_match(&self) -> bool {
        self.expected_sha256
            .eq_ignore_ascii_case(&self.actual_sha256)
            && self.expected_size_bytes == self.actual_size_bytes
    }
}

impl ModelDownloader {
    pub fn new(
        model_name: String,
        config: &ModelSourceConfig,
        manifest: &ModelManifest,
        models_dir: PathBuf,
        cancel: CancellationToken,
    ) -> Result<Self, ModelManagementError> {
        Self::build(model_name, config, manifest, models_dir, cancel, true)
    }

    fn build(
        model_name: String,
        config: &ModelSourceConfig,
        manifest: &ModelManifest,
        models_dir: PathBuf,
        cancel: CancellationToken,
        require_https: bool,
    ) -> Result<Self, ModelManagementError> {
        let entry = manifest.require(&model_name)?.clone();
        if require_https {
            config.validate_base_url()?;
        }
        let url = config.resolve_url(&model_name)?;

        if models_dir.as_os_str().is_empty() || !models_dir.is_absolute() {
            return Err(ModelManagementError::Path(format!(
                "refusing to download into non-absolute models_dir {:?}",
                models_dir
            )));
        }

        let file_name = format!("ggml-{model_name}.bin");
        let final_path = models_dir.join(&file_name);
        let partial_dir = models_dir.join(PARTIAL_SUBDIR);
        let part_path = partial_dir.join(format!("{file_name}{PART_SUFFIX}"));
        let meta_path = partial_dir.join(format!("{file_name}{META_SUFFIX}"));

        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| ModelManagementError::Download(format!("http client build: {e}")))?;

        Ok(Self {
            model_name,
            entry,
            url,
            final_path,
            part_path,
            meta_path,
            cancel,
            client,
        })
    }

    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    pub async fn run(
        &mut self,
        tx: UnboundedSender<DownloadState>,
    ) -> Result<DownloadState, ModelManagementError> {
        tracing::debug!(model = %self.model_name, "model install requested");
        if self.final_path.is_file() {
            let installed = DownloadState::Installed;
            send_state(&tx, installed.clone());
            return Ok(installed);
        }

        send_state(&tx, DownloadState::Resolving);
        if let Err(reason) = self.head_validate().await {
            return Ok(send_failed(&tx, reason));
        }

        if self.cancel.is_cancelled() {
            send_state(&tx, DownloadState::Cancelled);
            return Ok(DownloadState::Cancelled);
        }

        if let Err(e) = self.ensure_partial_dir().await {
            return Ok(send_failed(&tx, FailReason::Io(e.to_string())));
        }

        let (mut hasher, mut bytes_done) = match self.prepare_resume().await {
            Ok(prepared) => prepared,
            Err(reason) => return Ok(send_failed(&tx, reason)),
        };

        let total = self.entry.size_bytes;
        send_state(&tx, DownloadState::Fetching { bytes_done, total });

        match self.stream_body(&mut hasher, &mut bytes_done, &tx).await {
            Ok(StreamOutcome::Completed) => {}
            Ok(StreamOutcome::Cancelled) => {
                send_state(&tx, DownloadState::Cancelled);
                return Ok(DownloadState::Cancelled);
            }
            Err(reason) => return Ok(send_failed(&tx, reason)),
        }

        send_state(&tx, DownloadState::Verifying);
        let actual = hex_lower(&hasher.finalize());
        if actual != self.entry.sha256.to_ascii_lowercase() || bytes_done != total {
            let _ = tokio_fs::remove_file(&self.part_path).await;
            let _ = tokio_fs::remove_file(&self.meta_path).await;
            return Ok(send_failed(&tx, FailReason::ChecksumMismatch));
        }

        if let Err(e) = self.finalise_rename().await {
            return Ok(send_failed(&tx, FailReason::Io(e.to_string())));
        }

        let _ = tokio_fs::remove_file(&self.meta_path).await;
        send_state(&tx, DownloadState::Installed);
        Ok(DownloadState::Installed)
    }

    async fn head_validate(&self) -> Result<(), FailReason> {
        let resp = self
            .client
            .head(&self.url)
            .header(ACCEPT, HeaderValue::from_static("*/*"))
            .timeout(HEAD_TIMEOUT)
            .send()
            .await
            .map_err(|e| FailReason::Network(format!("HEAD: {e}")))?;

        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(FailReason::RateLimited {
                retry_after_secs: retry_after_secs(resp.headers()),
            });
        }
        if !status.is_success() {
            return Err(FailReason::Http(status.as_u16()));
        }

        validate_content_type(resp.headers())?;
        validate_content_length(resp.headers(), self.entry.size_bytes)?;
        Ok(())
    }

    async fn ensure_partial_dir(&self) -> Result<(), std::io::Error> {
        if let Some(parent) = self.part_path.parent() {
            tokio_fs::create_dir_all(parent).await?;
        }
        Ok(())
    }

    async fn prepare_resume(&self) -> Result<(Sha256, u64), FailReason> {
        let mut hasher = Sha256::new();
        let mut bytes_done = 0_u64;

        if !self.part_path.is_file() {
            return Ok((hasher, bytes_done));
        }

        let mut file = File::open(&self.part_path)
            .await
            .map_err(|e| FailReason::Io(format!("open .part: {e}")))?;

        let buf_size = usize::try_from(FLUSH_CHUNK_BYTES).unwrap_or(64 * 1024);
        let mut buf = vec![0_u8; buf_size];
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| FailReason::Io(format!("read .part: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            bytes_done = bytes_done.saturating_add(n as u64);
        }

        Ok((hasher, bytes_done))
    }

    async fn stream_body(
        &self,
        hasher: &mut Sha256,
        bytes_done: &mut u64,
        tx: &UnboundedSender<DownloadState>,
    ) -> Result<StreamOutcome, FailReason> {
        let total = self.entry.size_bytes;
        if *bytes_done >= total {
            return Ok(StreamOutcome::Completed);
        }

        let mut req = self.client.get(&self.url);
        if *bytes_done > 0 {
            req = req.header(RANGE, format!("bytes={bytes_done}-"));
        }
        let mut resp = req
            .send()
            .await
            .map_err(|e| FailReason::Network(format!("GET: {e}")))?;

        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(FailReason::RateLimited {
                retry_after_secs: retry_after_secs(resp.headers()),
            });
        }
        if !status.is_success() {
            return Err(FailReason::Http(status.as_u16()));
        }
        validate_content_type(resp.headers())?;

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&self.part_path)
            .await
            .map_err(|e| FailReason::Io(format!("open .part for append: {e}")))?;

        let mut bytes_since_flush = 0_u64;
        loop {
            if self.cancel.is_cancelled() {
                file.flush()
                    .await
                    .map_err(|e| FailReason::Io(format!("flush on cancel: {e}")))?;
                file.sync_data()
                    .await
                    .map_err(|e| FailReason::Io(format!("sync_data on cancel: {e}")))?;
                self.persist_meta(*bytes_done).await.ok();
                return Ok(StreamOutcome::Cancelled);
            }

            let next = tokio::select! {
                biased;
                () = self.cancel.cancelled() => None,
                chunk = tokio::time::timeout(STREAM_INACTIVITY, resp.chunk()) => match chunk {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => return Err(FailReason::Network(format!("chunk: {e}"))),
                    Err(_) => {
                        return Err(FailReason::Network(
                            "stream stalled past inactivity timeout".to_owned(),
                        ));
                    }
                },
            };

            let Some(bytes) = next else {
                if self.cancel.is_cancelled() {
                    self.persist_meta(*bytes_done).await.ok();
                    return Ok(StreamOutcome::Cancelled);
                }
                break;
            };
            if bytes.is_empty() {
                continue;
            }

            file.write_all(&bytes)
                .await
                .map_err(|e| FailReason::Io(format!("write .part: {e}")))?;
            hasher.update(&bytes);
            *bytes_done = bytes_done.saturating_add(bytes.len() as u64);
            bytes_since_flush = bytes_since_flush.saturating_add(bytes.len() as u64);

            if bytes_since_flush >= FLUSH_CHUNK_BYTES {
                file.flush()
                    .await
                    .map_err(|e| FailReason::Io(format!("flush: {e}")))?;
                self.persist_meta(*bytes_done).await.ok();
                bytes_since_flush = 0;
                send_state(
                    tx,
                    DownloadState::Fetching {
                        bytes_done: *bytes_done,
                        total,
                    },
                );
            }
        }

        file.flush()
            .await
            .map_err(|e| FailReason::Io(format!("final flush: {e}")))?;
        file.sync_data()
            .await
            .map_err(|e| FailReason::Io(format!("final sync_data: {e}")))?;
        self.persist_meta(*bytes_done).await.ok();
        send_state(
            tx,
            DownloadState::Fetching {
                bytes_done: *bytes_done,
                total,
            },
        );
        Ok(StreamOutcome::Completed)
    }

    async fn persist_meta(&self, bytes_committed: u64) -> Result<(), ModelManagementError> {
        let json = format!(r#"{{"bytes_committed":{bytes_committed}}}"#);
        if let Err(e) = tokio_fs::write(&self.meta_path, json.as_bytes()).await {
            tracing::warn!(
                error = %e,
                path = %self.meta_path.display(),
                "failed to persist model .part metadata"
            );
        }
        Ok(())
    }

    async fn finalise_rename(&self) -> Result<(), std::io::Error> {
        if let Some(parent) = self.final_path.parent() {
            tokio_fs::create_dir_all(parent).await?;
        }
        tokio_fs::rename(&self.part_path, &self.final_path).await
    }
}

enum StreamOutcome {
    Completed,
    Cancelled,
}

pub fn model_path(model_name: &str) -> Result<PathBuf, ModelManagementError> {
    let manifest = ModelManifest::embedded();
    manifest.require(model_name)?;
    let dir = models::models_dir().map_err(|e| ModelManagementError::Path(e.to_string()))?;
    Ok(dir.join(format!("ggml-{model_name}.bin")))
}

pub fn models_dir() -> Result<PathBuf, ModelManagementError> {
    models::models_dir().map_err(|e| ModelManagementError::Path(e.to_string()))
}

pub fn verify_model(model_name: &str) -> Result<ModelVerification, ModelManagementError> {
    let manifest = ModelManifest::embedded();
    let entry = manifest.require(model_name)?;
    let path = model_path(model_name)?;
    let mut file = StdFile::open(&path).map_err(|e| {
        ModelManagementError::Path(format!("failed to open {}: {e}", path.display()))
    })?;
    let actual_size_bytes = file
        .metadata()
        .map_err(|e| ModelManagementError::Path(format!("failed to stat {}: {e}", path.display())))?
        .len();

    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| {
            ModelManagementError::Path(format!("failed to read {}: {e}", path.display()))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(ModelVerification {
        path,
        expected_sha256: entry.sha256.clone(),
        actual_sha256: hex_lower(&hasher.finalize()),
        expected_size_bytes: entry.size_bytes,
        actual_size_bytes,
    })
}

fn send_state(tx: &UnboundedSender<DownloadState>, state: DownloadState) {
    if tx.send(state).is_err() {
        tracing::debug!("model download state receiver was dropped");
    }
}

fn send_failed(tx: &UnboundedSender<DownloadState>, reason: FailReason) -> DownloadState {
    let state = DownloadState::Failed { reason };
    send_state(tx, state.clone());
    state
}

fn validate_content_type(headers: &HeaderMap) -> Result<(), FailReason> {
    let raw = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let bare = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if ALLOWED_CONTENT_TYPES.iter().any(|allowed| bare == *allowed) {
        Ok(())
    } else {
        Err(FailReason::ContentTypeMismatch(raw.to_owned()))
    }
}

fn validate_content_length(headers: &HeaderMap, expected: u64) -> Result<(), FailReason> {
    let actual = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    match actual {
        Some(len) if len == expected => Ok(()),
        Some(len) => Err(FailReason::ContentLengthMismatch {
            expected,
            actual: len,
        }),
        None => {
            tracing::warn!(
                expected,
                "server omitted Content-Length; relying on SHA-256 to catch truncation"
            );
            Ok(())
        }
    }
}

fn retry_after_secs(headers: &HeaderMap) -> u64 {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|s| s.min(RETRY_AFTER_MAX_SECS))
        .unwrap_or(0)
}

fn hex_lower(digest: &[u8]) -> String {
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn manifest_for(model: &str, body: &[u8]) -> ModelManifest {
        let sha = hex_lower(&Sha256::digest(body));
        let toml_text = format!(
            "[{model}]\nsha256 = \"{sha}\"\nsize_bytes = {}\n",
            body.len()
        );
        ModelManifest::parse(&toml_text).unwrap()
    }

    fn config_for(base_url: String) -> ModelSourceConfig {
        ModelSourceConfig { base_url }
    }

    impl ModelDownloader {
        fn new_for_test(
            model_name: String,
            config: &ModelSourceConfig,
            manifest: &ModelManifest,
            models_dir: PathBuf,
            cancel: CancellationToken,
        ) -> Result<Self, ModelManagementError> {
            let entry = manifest.require(&model_name)?.clone();
            if !config.base_url.contains(MODEL_PLACEHOLDER) {
                return Err(ModelManagementError::Config(format!(
                    "base_url must contain {MODEL_PLACEHOLDER} placeholder"
                )));
            }
            let url = config.base_url.replace(MODEL_PLACEHOLDER, &model_name);

            let file_name = format!("ggml-{model_name}.bin");
            let final_path = models_dir.join(&file_name);
            let partial_dir = models_dir.join(PARTIAL_SUBDIR);
            let part_path = partial_dir.join(format!("{file_name}{PART_SUFFIX}"));
            let meta_path = partial_dir.join(format!("{file_name}{META_SUFFIX}"));
            let client = Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .map_err(|e| ModelManagementError::Download(format!("http client build: {e}")))?;

            Ok(Self {
                model_name,
                entry,
                url,
                final_path,
                part_path,
                meta_path,
                cancel,
                client,
            })
        }
    }

    #[test]
    fn embedded_manifest_lists_known_models() {
        let names: Vec<&str> = ModelManifest::embedded()
            .known_models()
            .map(|(name, _entry)| name)
            .collect();
        assert_eq!(names, vec!["base", "large-v3", "medium", "small", "tiny"]);
    }

    #[test]
    fn config_rejects_non_https_base_url() {
        let cfg = ModelSourceConfig {
            base_url: "http://example.com/ggml-{model}.bin".to_owned(),
        };
        let err = cfg.resolve_url("tiny").unwrap_err();
        assert!(matches!(err, ModelManagementError::Config(msg) if msg.contains("https://")));
    }

    #[test]
    fn config_rejects_extra_placeholder() {
        let cfg = ModelSourceConfig {
            base_url: "https://example.com/{tenant}/ggml-{model}.bin".to_owned(),
        };
        let err = cfg.resolve_url("tiny").unwrap_err();
        assert!(matches!(err, ModelManagementError::Config(msg) if msg.contains("placeholders")));
    }

    #[test]
    fn verification_requires_hash_and_size_match() {
        let verification = ModelVerification {
            path: PathBuf::from("/tmp/ggml-tiny.bin"),
            expected_sha256: "abc".to_owned(),
            actual_sha256: "ABC".to_owned(),
            expected_size_bytes: 3,
            actual_size_bytes: 3,
        };
        assert!(verification.is_match());

        let wrong_size = ModelVerification {
            actual_size_bytes: 4,
            ..verification
        };
        assert!(!wrong_size.is_match());
    }

    #[tokio::test]
    async fn happy_path_fetches_verifies_and_installs() {
        let body = b"tiny-model-bytes".to_vec();
        let manifest = manifest_for("tiny", &body);
        let server = MockServer::start().await;
        let url_path = "/ggml-tiny.bin";
        Mock::given(method("HEAD"))
            .and(path(url_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Length", body.len().to_string()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(url_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/octet-stream")
                    .set_body_bytes(body.clone()),
            )
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let cfg = config_for(format!("{}/ggml-{{model}}.bin", server.uri()));
        let mut downloader = ModelDownloader::new_for_test(
            "tiny".to_owned(),
            &cfg,
            &manifest,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let final_state = downloader.run(tx).await.unwrap();

        assert_eq!(final_state, DownloadState::Installed);
        assert_eq!(fs::read(dir.path().join("ggml-tiny.bin")).unwrap(), body);
        let mut saw_verifying = false;
        while let Ok(state) = rx.try_recv() {
            saw_verifying |= state == DownloadState::Verifying;
        }
        assert!(saw_verifying, "expected Verifying state");
    }

    #[tokio::test]
    async fn html_head_rejects_before_part_file() {
        let body = b"not-used".to_vec();
        let manifest = manifest_for("tiny", &body);
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/ggml-tiny.bin"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .insert_header("Content-Length", body.len().to_string()),
            )
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let cfg = config_for(format!("{}/ggml-{{model}}.bin", server.uri()));
        let mut downloader = ModelDownloader::new_for_test(
            "tiny".to_owned(),
            &cfg,
            &manifest,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .unwrap();

        let (tx, _rx) = mpsc::unbounded_channel();
        let final_state = downloader.run(tx).await.unwrap();

        assert!(matches!(
            final_state,
            DownloadState::Failed {
                reason: FailReason::ContentTypeMismatch(_)
            }
        ));
        assert!(
            !dir.path()
                .join(".partial")
                .join("ggml-tiny.bin.part")
                .exists()
        );
    }

    #[tokio::test]
    async fn checksum_mismatch_removes_poisoned_part() {
        let manifest = manifest_for("tiny", b"expected");
        let body = b"different".to_vec();
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/ggml-tiny.bin"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Length", b"expected".len().to_string()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ggml-tiny.bin"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/octet-stream")
                    .set_body_bytes(body),
            )
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let cfg = config_for(format!("{}/ggml-{{model}}.bin", server.uri()));
        let mut downloader = ModelDownloader::new_for_test(
            "tiny".to_owned(),
            &cfg,
            &manifest,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .unwrap();

        let (tx, _rx) = mpsc::unbounded_channel();
        let final_state = downloader.run(tx).await.unwrap();

        assert_eq!(
            final_state,
            DownloadState::Failed {
                reason: FailReason::ChecksumMismatch
            }
        );
        assert!(
            !dir.path()
                .join(".partial")
                .join("ggml-tiny.bin.part")
                .exists()
        );
    }
}
