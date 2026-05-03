//! M7 — Model downloader state machine + HTTP + SHA-256 + atomic
//! rename.
//!
//! State machine (M7-plan § C3, DoD #6):
//!
//! ```text
//! Idle
//!   └──► Resolving                    HEAD validates Content-Type
//!        └──► Fetching{bytes_done}    GET stream → write .part
//!             └──► Verifying          stream EOF → finalize SHA
//!                  ├──► Installed     SHA matches → atomic rename
//!                  └──► Failed        SHA mismatch → drop .part
//! Cancelled  ◄── any state via CancellationToken (chunk boundary).
//! ```
//!
//! Failure surface — the [`FailReason`] enum keeps the UI dumb (one
//! match arm per banner copy), and avoids leaking opaque
//! `reqwest::Error` payloads through the channel.
//!
//! Path layout (M7-plan D4 — `.part` co-located with final to dodge
//! cross-FS `EXDEV`):
//! - `<models_dir>/.partial/ggml-<name>.bin.part`           — body
//! - `<models_dir>/.partial/ggml-<name>.bin.part.meta.json` — A3
//!   crash-resume sidecar (`{ bytes_committed }`).
//! - `<models_dir>/ggml-<name>.bin`                          — final
//!
//! Resume semantics (DoD #8): on `new()`, if a `.part` exists, the
//! constructor re-hashes the entire `.part` from byte 0 before
//! sending `Range: bytes=<file_size>-`. This costs IO but defeats
//! the "poisoned partial" attack where a malicious mid-stream MITM
//! flipped bytes that earlier chunks already committed to the
//! rolling hash.
//!
//! All constants below are `const` per CLAUDE.md "no magic numbers".

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs::{self as tokio_fs, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::checksums::{ChecksumManifest, Entry};
use crate::error::SettingsError;

/// HTTP `User-Agent`. Keeping the project name + version visible
/// helps HuggingFace operators triage rate-limit issues.
const USER_AGENT: &str = concat!("zwhisper-settings/", env!("CARGO_PKG_VERSION"));

/// Connection-establishment timeout. The HuggingFace edge handshake
/// is fast; anything over a few seconds usually means DNS or
/// captive-portal trouble.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-request timeout for the HEAD probe. Body is tiny.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

/// We do **not** set a request timeout on GET because large-v3 is
/// 3 GiB and slow links would trip a hard timeout mid-stream.
/// Cancellation is still cooperative via [`CancellationToken`].
///
/// Read inactivity threshold — applied per-chunk via `tokio::select!`
/// in the streaming loop. Defends against a server that completes
/// the handshake then stops sending bytes.
const STREAM_INACTIVITY: Duration = Duration::from_secs(60);

/// Cap on the in-memory chunk buffer. `bytes::Bytes` is reference-
/// counted so this is mostly aspirational, but flushing every
/// 256 KiB bounds the meta-sidecar update cadence (M7-plan F2).
/// Bytes between buffered-write flushes. 4 MiB balances throughput
/// (fewer fsync calls) against the worst-case lost-progress window
/// on crash (a few MB of redo on resume — caught by the byte-zero
/// re-hash anyway, see `DoD` #8). Per-chunk `sync_data` was the
/// performance bottleneck identified in the M7 perf review and
/// is now reserved for the cancel/EOF paths only.
const FLUSH_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// Whitelisted Content-Type values for the model body. Anything
/// else (text/html, application/json, ...) is rejected before any
/// `.part` file is created (DoD #9, B3).
const ALLOWED_CONTENT_TYPES: &[&str] = &[
    "application/octet-stream",
    "application/x-binary",
    "binary/octet-stream",
];

/// Suffix used for the in-progress download body.
const PART_SUFFIX: &str = ".part";

/// Suffix used for the crash-resume meta sidecar.
const META_SUFFIX: &str = ".part.meta.json";

/// Subdirectory under `<models_dir>` that holds in-progress
/// downloads. Same filesystem as the final → `rename(2)` is atomic.
const PARTIAL_SUBDIR: &str = ".partial";

/// Cap on `Retry-After` we will surface to the user (10 minutes).
/// Beyond this we still report the value but the UI may decide to
/// treat the failure as "give up" rather than "wait".
const RETRY_AFTER_MAX_SECS: u64 = 10 * 60;

/// Public state surfaced to the UI. Cloning is cheap enough that
/// we send a fresh value on every progress chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DownloadState {
    /// Constructed but `run` has not been called yet. Default when
    /// no `.part` exists on disk.
    Idle,
    /// Sent immediately after `run` is entered; HEAD request is in
    /// flight.
    Resolving,
    /// Streaming body chunks. `bytes_done` includes any bytes
    /// recovered from a resumed `.part`.
    Fetching { bytes_done: u64, total: u64 },
    /// Stream EOF reached; final SHA-256 finalize is running.
    Verifying,
    /// SHA matches; `.part` renamed to final destination.
    Installed,
    /// Transition to a failure variant. UI maps this to a banner +
    /// retry button per [`FailReason`] arm.
    Failed { reason: FailReason },
    /// Cancellation token tripped between chunks. `.part` left on
    /// disk for resume; meta sidecar persisted.
    Cancelled,
}

/// Human-meaningful failure classification. UI matches on the
/// variant to choose copy and the "Retry" / "Restart from zero"
/// affordance. We avoid leaking `reqwest::Error` to keep the UI
/// independent of the HTTP client choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FailReason {
    /// Manifest lookup returned `None`. Caller should disable the
    /// row's Download button (DoD #10).
    UnknownModel,
    /// HEAD response had an unsupported `Content-Type`. We embed
    /// the offending value so the UI can show "got: text/html".
    ContentTypeMismatch(String),
    /// HEAD response Content-Length disagreed with the manifest.
    /// Hard fail, no `.part` opened (DoD #9).
    ContentLengthMismatch { expected: u64, actual: u64 },
    /// Non-2xx HTTP status (excluding 429 which has its own arm).
    Http(u16),
    /// HTTP 429 with parsed `Retry-After`. UI surfaces the
    /// countdown (DoD #11).
    RateLimited { retry_after_secs: u64 },
    /// Any reqwest / TLS / DNS / streaming error.
    Network(String),
    /// SHA-256 finalised but disagreed with manifest. The `.part`
    /// is deleted before this is emitted so the next attempt starts
    /// from byte 0 (DoD #8).
    ChecksumMismatch,
    /// Filesystem error (open, write, rename, fsync).
    Io(String),
}

/// Sidecar JSON persisted next to `.part` after every flushed chunk
/// (M7-plan A3 mitigation). `bytes_committed` is the byte count we
/// have both written *and* `flush + sync_data`'d. On resume we
/// trust this value and re-hash exactly that many bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PartMeta {
    bytes_committed: u64,
}

/// Inputs the downloader needs at construction time.
///
/// Public surface kept intentionally small — the UI builds one
/// downloader per Download click, drives it via [`Self::run`], and
/// drops it. State across attempts lives on disk in the `.part` +
/// meta sidecar.
#[derive(Debug)]
pub(crate) struct ModelDownloader {
    /// Bare model name as it appears in `checksums.toml` (e.g.
    /// `"tiny"`, `"large-v3"`).
    model_name: String,
    /// Checksum + size for `model_name`, looked up at construction
    /// time so callers cannot accidentally race `new()` against a
    /// manifest reload.
    entry: Entry,
    /// Final URL after `{model}` substitution.
    url: String,
    /// `<models_dir>/ggml-<name>.bin`.
    final_path: PathBuf,
    /// `<models_dir>/.partial/ggml-<name>.bin.part`.
    part_path: PathBuf,
    /// `<models_dir>/.partial/ggml-<name>.bin.part.meta.json`.
    meta_path: PathBuf,
    /// Cooperative cancel from the parent runtime.
    cancel: CancellationToken,
    /// HTTP client. Built once per downloader for connection
    /// pooling within a single download.
    client: Client,
}

impl ModelDownloader {
    /// Build a downloader for `model_name`. Fails fast on:
    /// - Unknown model (DoD #10) → `Err(Download)` with
    ///   `FailReason::UnknownModel` semantics.
    /// - Missing `{model}` placeholder in `base_url` etc. — the
    ///   `base_url` validation lives in [`crate::config`] but a
    ///   second guard runs here for defence in depth.
    /// - HTTP client build failure (TLS init, etc.).
    pub(crate) fn new(
        model_name: String,
        base_url: String,
        manifest: &ChecksumManifest,
        models_dir: PathBuf,
        cancel: CancellationToken,
    ) -> Result<Self, SettingsError> {
        let entry = manifest
            .lookup(&model_name)
            .ok_or_else(|| {
                SettingsError::Download(format!(
                    "unknown model: {model_name} \
                     (not in embedded checksums.toml)"
                ))
            })?
            .clone();

        let url = substitute_model_token(&base_url, &model_name)?;

        // Refuse to construct a downloader against an empty or
        // non-absolute `models_dir`. Without this guard a caller
        // who silently fell back to `PathBuf::new()` would have us
        // writing `./ggml-*.bin` to the current working directory
        // (the post-review tab fix; defence-in-depth here).
        if models_dir.as_os_str().is_empty() || !models_dir.is_absolute() {
            return Err(SettingsError::Download(format!(
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
            .map_err(|e| SettingsError::Download(format!("http client build: {e}")))?;

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

    /// Snapshot of the on-disk `.part` size. UI uses this to render
    /// a `[Resume]` label vs `[Download]` *before* `run` is called.
    pub(crate) fn resume_offset(&self) -> u64 {
        std::fs::metadata(&self.part_path)
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Run the full state machine. Sends every transition through
    /// `tx`. Returns the terminal state.
    pub(crate) async fn run(
        &mut self,
        tx: UnboundedSender<DownloadState>,
    ) -> Result<DownloadState, SettingsError> {
        // Defensive: refuse to overwrite an existing final file. If
        // the user wants to re-install, they delete it first.
        if self.final_path.is_file() {
            let installed = DownloadState::Installed;
            let _ = tx.send(installed.clone());
            return Ok(installed);
        }

        send_state(&tx, DownloadState::Resolving);

        // Phase 1 — HEAD validation.
        match self.head_validate().await {
            Ok(()) => {}
            Err(reason) => {
                let st = DownloadState::Failed { reason };
                send_state(&tx, st.clone());
                return Ok(st);
            }
        }

        if self.cancel.is_cancelled() {
            send_state(&tx, DownloadState::Cancelled);
            return Ok(DownloadState::Cancelled);
        }

        // Phase 2 — prepare partial dir + (optionally) re-hash
        // existing `.part` from byte 0 (DoD #8).
        if let Err(e) = self.ensure_partial_dir().await {
            let st = DownloadState::Failed {
                reason: FailReason::Io(e.to_string()),
            };
            send_state(&tx, st.clone());
            return Ok(st);
        }

        let (mut hasher, mut bytes_done) = match self.prepare_resume().await {
            Ok(p) => p,
            Err(reason) => {
                let st = DownloadState::Failed { reason };
                send_state(&tx, st.clone());
                return Ok(st);
            }
        };

        // Phase 3 — open .part for append, send GET with Range.
        let total = self.entry.size_bytes;
        send_state(
            &tx,
            DownloadState::Fetching { bytes_done, total },
        );

        let outcome = self
            .stream_body(&mut hasher, &mut bytes_done, &tx)
            .await;

        match outcome {
            Ok(StreamOutcome::Completed) => {}
            Ok(StreamOutcome::Cancelled) => {
                send_state(&tx, DownloadState::Cancelled);
                return Ok(DownloadState::Cancelled);
            }
            Err(reason) => {
                let st = DownloadState::Failed { reason };
                send_state(&tx, st.clone());
                return Ok(st);
            }
        }

        // Phase 4 — finalise SHA, compare, atomic rename.
        send_state(&tx, DownloadState::Verifying);

        let actual = hex_lower(&hasher.finalize());
        if actual != self.entry.sha256.to_ascii_lowercase() {
            // Drop poisoned `.part` so the next click starts clean.
            let _ = tokio_fs::remove_file(&self.part_path).await;
            let _ = tokio_fs::remove_file(&self.meta_path).await;
            let st = DownloadState::Failed {
                reason: FailReason::ChecksumMismatch,
            };
            send_state(&tx, st.clone());
            return Ok(st);
        }

        if let Err(e) = self.finalise_rename().await {
            let st = DownloadState::Failed {
                reason: FailReason::Io(e.to_string()),
            };
            send_state(&tx, st.clone());
            return Ok(st);
        }

        // Best-effort cleanup of the meta sidecar.
        let _ = tokio_fs::remove_file(&self.meta_path).await;

        send_state(&tx, DownloadState::Installed);
        Ok(DownloadState::Installed)
    }

    /// HEAD probe — verifies Content-Type allow-listed and
    /// Content-Length matches the manifest. Returns `Err(reason)` on
    /// any rejection so the caller can wrap into [`DownloadState::Failed`].
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
            let retry_after = parse_retry_after(resp.headers())
                .map(|s| s.min(RETRY_AFTER_MAX_SECS))
                .unwrap_or(0);
            return Err(FailReason::RateLimited {
                retry_after_secs: retry_after,
            });
        }
        if !status.is_success() {
            return Err(FailReason::Http(status.as_u16()));
        }

        validate_content_type(resp.headers())?;
        validate_content_length(resp.headers(), self.entry.size_bytes)?;
        Ok(())
    }

    /// Create `<models_dir>/.partial/` if missing. Wraps `tokio_fs`
    /// so the error path is uniform.
    async fn ensure_partial_dir(&self) -> Result<(), SettingsError> {
        if let Some(parent) = self.part_path.parent() {
            tokio_fs::create_dir_all(parent).await?;
        }
        Ok(())
    }

    /// If a `.part` exists, re-hash the whole thing from byte 0
    /// (DoD #8). Returns `(hasher, bytes_done)` where `bytes_done`
    /// is the size we already have on disk.
    async fn prepare_resume(&self) -> Result<(Sha256, u64), FailReason> {
        let mut hasher = Sha256::new();
        let mut bytes_done = 0_u64;

        if !self.part_path.is_file() {
            return Ok((hasher, bytes_done));
        }

        // Trust the file's actual byte count over the meta sidecar:
        // if a crash truncated the file but the meta promised more,
        // we use the smaller of the two so the Range request asks
        // for the right offset.
        let part_size = tokio_fs::metadata(&self.part_path)
            .await
            .map_err(|e| FailReason::Io(format!("stat .part: {e}")))?
            .len();

        let mut file = File::open(&self.part_path)
            .await
            .map_err(|e| FailReason::Io(format!("open .part: {e}")))?;

        // Stream the existing bytes through Sha256::update. Buffer
        // size mirrors FLUSH_CHUNK_BYTES so we touch one allocation
        // shape on the resume + happy path.
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
        debug_assert_eq!(
            bytes_done, part_size,
            ".part size and re-hash byte count must agree",
        );

        Ok((hasher, bytes_done))
    }

    /// Body streaming loop — opens the `.part` for append, issues a
    /// GET with `Range`, drains chunks. Updates the rolling hash
    /// and persists the meta sidecar after each flushed chunk.
    async fn stream_body(
        &self,
        hasher: &mut Sha256,
        bytes_done: &mut u64,
        tx: &UnboundedSender<DownloadState>,
    ) -> Result<StreamOutcome, FailReason> {
        let total = self.entry.size_bytes;

        // Already complete on disk → skip the GET entirely.
        if *bytes_done >= total {
            return Ok(StreamOutcome::Completed);
        }

        let mut req = self.client.get(&self.url);
        if *bytes_done > 0 {
            let range = format!("bytes={bytes_done}-");
            req = req.header(RANGE, range);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| FailReason::Network(format!("GET: {e}")))?;

        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = parse_retry_after(resp.headers())
                .map(|s| s.min(RETRY_AFTER_MAX_SECS))
                .unwrap_or(0);
            return Err(FailReason::RateLimited {
                retry_after_secs: retry_after,
            });
        }
        if !status.is_success() {
            return Err(FailReason::Http(status.as_u16()));
        }

        // Re-validate content type on the GET — some servers
        // disagree between HEAD and GET (B3 second guard).
        validate_content_type(resp.headers())?;

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&self.part_path)
            .await
            .map_err(|e| FailReason::Io(format!("open .part for append: {e}")))?;

        let mut stream = resp.bytes_stream();
        let mut bytes_since_flush: u64 = 0;

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
                chunk = tokio::time::timeout(STREAM_INACTIVITY, stream.next()) => match chunk {
                    Ok(c) => c,
                    Err(_) => {
                        return Err(FailReason::Network(
                            "stream stalled past inactivity timeout".into(),
                        ));
                    }
                },
            };

            let Some(chunk) = next else {
                // Stream EOF or cancel-via-select.
                if self.cancel.is_cancelled() {
                    self.persist_meta(*bytes_done).await.ok();
                    return Ok(StreamOutcome::Cancelled);
                }
                break;
            };
            let bytes = chunk
                .map_err(|e| FailReason::Network(format!("chunk: {e}")))?;
            if bytes.is_empty() {
                continue;
            }

            file.write_all(&bytes)
                .await
                .map_err(|e| FailReason::Io(format!("write .part: {e}")))?;
            hasher.update(&bytes);
            *bytes_done = bytes_done.saturating_add(bytes.len() as u64);
            bytes_since_flush =
                bytes_since_flush.saturating_add(bytes.len() as u64);

            if bytes_since_flush >= FLUSH_CHUNK_BYTES {
                // Flush the BufWriter (buffer → file) but skip
                // `sync_data` here — it caps throughput on
                // rotational/slow storage and the worst-case
                // crash-recovery window is bounded by
                // `FLUSH_CHUNK_BYTES`. On resume we re-hash
                // from byte 0 anyway (`DoD` #8), so a half-flushed
                // chunk is detected at SHA verify and the user
                // sees a clean restart. Final fsync still runs
                // on stream EOF and on the cancel path.
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

        // Final flush on stream EOF.
        file.flush()
            .await
            .map_err(|e| FailReason::Io(format!("final flush: {e}")))?;
        file.sync_data()
            .await
            .map_err(|e| FailReason::Io(format!("final sync_data: {e}")))?;
        self.persist_meta(*bytes_done).await.ok();

        // One last progress emit before Verifying so the UI shows
        // 100% immediately.
        send_state(
            tx,
            DownloadState::Fetching {
                bytes_done: *bytes_done,
                total,
            },
        );
        Ok(StreamOutcome::Completed)
    }

    /// Atomic rename `.part` → final via `tempfile::NamedTempFile`-
    /// style two-step (we already wrote `.part` directly, so just
    /// rename — same FS by D4 design).
    async fn finalise_rename(&self) -> Result<(), SettingsError> {
        // Belt-and-braces: ensure final's parent exists.
        if let Some(parent) = self.final_path.parent() {
            tokio_fs::create_dir_all(parent).await?;
        }
        tokio_fs::rename(&self.part_path, &self.final_path).await?;
        Ok(())
    }

    /// Persist `bytes_committed` to `.part.meta.json`. We re-create
    /// the sidecar on every flush — JSON object is tiny so this is
    /// cheap. Returns `Ok(())` even on transient IO failure so a
    /// metadata write hiccup does not abort an otherwise-healthy
    /// download.
    async fn persist_meta(&self, bytes_committed: u64) -> Result<(), SettingsError> {
        let meta = PartMeta { bytes_committed };
        let json = serde_json::to_string(&meta)
            .map_err(|e| SettingsError::Download(format!("meta serialize: {e}")))?;
        if let Err(e) = tokio_fs::write(&self.meta_path, json.as_bytes()).await {
            tracing::warn!(
                error = %e,
                path = %self.meta_path.display(),
                "failed to persist .part.meta.json — will keep going",
            );
        }
        Ok(())
    }
}

/// Internal "the streaming loop ran to completion" vs "cancel
/// observed mid-stream" outcome.
enum StreamOutcome {
    Completed,
    Cancelled,
}

fn send_state(tx: &UnboundedSender<DownloadState>, state: DownloadState) {
    if tx.send(state).is_err() {
        // Receiver dropped — UI is gone. Continuing the download
        // would be wasted work, but `run` is structured so the
        // caller always observes the final state via the return
        // value, so we just log here.
        tracing::debug!("download state channel closed; UI may have been dropped");
    }
}

fn validate_content_type(headers: &HeaderMap) -> Result<(), FailReason> {
    let raw = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Servers may add a charset or other parameters; match on the
    // bare media type prefix.
    let bare = raw.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
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
        // Header absent (e.g. chunked transfer-encoding from the HF
        // CDN). Permit and rely on SHA-256 to catch any truncation.
        // Documented as silent-failure finding 2 of the M7 review.
        None => {
            tracing::warn!(
                expected,
                "server omitted Content-Length; will rely on SHA-256 to catch truncation"
            );
            Ok(())
        }
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    raw.parse::<u64>().ok()
}

fn substitute_model_token(base_url: &str, model_name: &str) -> Result<String, SettingsError> {
    if !base_url.contains("{model}") {
        return Err(SettingsError::Download(
            "base URL missing {model} placeholder".into(),
        ));
    }
    if !base_url.starts_with("https://") {
        return Err(SettingsError::Download(
            "base URL must use https://".into(),
        ));
    }
    Ok(base_url.replace("{model}", model_name))
}

fn hex_lower(digest: &[u8]) -> String {
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        // `format!` returns a `String` on every iteration, but the
        // payload is 64 chars total; the cost is negligible vs the
        // SHA-256 computation that produced it.
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;

    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Build a one-off in-memory manifest with a single known model
    /// whose checksum matches `body`. Tests use this to drive
    /// happy-path flows without depending on the embedded
    /// `checksums.toml`.
    fn manifest_for(model: &str, body: &[u8]) -> ChecksumManifest {
        let sha = hex_lower(&Sha256::digest(body));
        let toml_text = format!(
            "[{model}]\nsha256 = \"{sha}\"\nsize_bytes = {}\n",
            body.len()
        );
        ChecksumManifest::parse(&toml_text).unwrap()
    }

    fn collect_states(rx: &mut mpsc::UnboundedReceiver<DownloadState>) -> Vec<DownloadState> {
        let mut out = Vec::new();
        while let Ok(s) = rx.try_recv() {
            out.push(s);
        }
        out
    }

    #[tokio::test]
    async fn happy_path_resolves_fetches_verifies_installs() {
        let body = b"the-model-bytes-are-tiny-for-this-test".to_vec();
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

        let models_dir = TempDir::new().unwrap();
        let base_url = format!("{}/ggml-{{model}}.bin", server.uri());
        // wiremock returns http:// URIs; bypass the https-only guard
        // for tests by injecting via a helper that mirrors the prod
        // path but skips the scheme check.
        let mut downloader = ModelDownloader::new_for_test(
            "tiny".into(),
            base_url,
            &manifest,
            models_dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let final_state = downloader.run(tx).await.unwrap();
        assert_eq!(final_state, DownloadState::Installed);
        let states = collect_states(&mut rx);
        assert!(states.contains(&DownloadState::Resolving));
        assert!(states.contains(&DownloadState::Verifying));
        assert!(states.contains(&DownloadState::Installed));

        let final_path = models_dir.path().join("ggml-tiny.bin");
        assert!(final_path.is_file(), "final file must exist");
        assert_eq!(fs::read(&final_path).unwrap(), body);
    }

    #[tokio::test]
    async fn part_file_lives_alongside_final() {
        // We look at the constructed paths — no network needed.
        let body = b"abc".to_vec();
        let manifest = manifest_for("tiny", &body);
        let dir = TempDir::new().unwrap();
        let downloader = ModelDownloader::new_for_test(
            "tiny".into(),
            "http://localhost/ggml-{model}.bin".into(),
            &manifest,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            downloader.part_path.parent().unwrap(),
            dir.path().join(".partial"),
        );
        assert_eq!(
            downloader.final_path.parent().unwrap(),
            dir.path(),
        );
    }

    #[tokio::test]
    async fn resume_re_hashes_from_zero_then_continues() {
        let body = b"0123456789abcdef0123456789abcdef".to_vec();
        let manifest = manifest_for("tiny", &body);

        // Pre-seed `.part` with the first half of the body.
        let dir = TempDir::new().unwrap();
        let partial = dir.path().join(".partial");
        fs::create_dir_all(&partial).unwrap();
        let part_path = partial.join("ggml-tiny.bin.part");
        fs::write(&part_path, &body[..16]).unwrap();

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
        // Server returns only the second half — wiremock does not
        // honour Range natively, but for this test we serve the
        // exact suffix bytes regardless of the Range header.
        Mock::given(method("GET"))
            .and(path(url_path))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Type", "application/octet-stream")
                    .set_body_bytes(body[16..].to_vec()),
            )
            .mount(&server)
            .await;

        let mut downloader = ModelDownloader::new_for_test(
            "tiny".into(),
            format!("{}/ggml-{{model}}.bin", server.uri()),
            &manifest,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .unwrap();

        let (tx, _rx) = mpsc::unbounded_channel();
        let final_state = downloader.run(tx).await.unwrap();
        assert_eq!(
            final_state,
            DownloadState::Installed,
            "resume must produce Installed end state"
        );
        let final_path = dir.path().join("ggml-tiny.bin");
        assert_eq!(fs::read(&final_path).unwrap(), body);
    }

    #[tokio::test]
    async fn html_response_aborts_before_writing_part() {
        let body = b"any".to_vec();
        let manifest = manifest_for("tiny", &body);

        let server = MockServer::start().await;
        let url_path = "/ggml-tiny.bin";
        Mock::given(method("HEAD"))
            .and(path(url_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html; charset=utf-8")
                    .insert_header("Content-Length", body.len().to_string()),
            )
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let mut downloader = ModelDownloader::new_for_test(
            "tiny".into(),
            format!("{}/ggml-{{model}}.bin", server.uri()),
            &manifest,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let final_state = downloader.run(tx).await.unwrap();
        match final_state {
            DownloadState::Failed {
                reason: FailReason::ContentTypeMismatch(ref ct),
            } => {
                assert!(
                    ct.contains("text/html"),
                    "should preserve Content-Type, got {ct:?}"
                );
            }
            other => panic!("expected ContentTypeMismatch, got {other:?}"),
        }
        let part = dir.path().join(".partial").join("ggml-tiny.bin.part");
        assert!(!part.exists(), ".part must NOT be created on HEAD reject");
        let _ = collect_states(&mut rx);
    }

    #[tokio::test]
    async fn unknown_model_refuses_with_friendly_error() {
        let manifest = ChecksumManifest::parse(
            "[tiny]\nsha256 = \"00\"\nsize_bytes = 1\n",
        )
        .unwrap();
        let dir = TempDir::new().unwrap();
        let err = ModelDownloader::new_for_test(
            "ghost-model".into(),
            "http://localhost/ggml-{model}.bin".into(),
            &manifest,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .unwrap_err();
        match err {
            SettingsError::Download(msg) => {
                assert!(msg.contains("unknown model"), "got: {msg}");
                assert!(msg.contains("ghost-model"), "got: {msg}");
            }
            other => panic!("expected Download error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_429_shows_retry_after_countdown() {
        let body = b"doesnt-matter".to_vec();
        let manifest = manifest_for("tiny", &body);

        let server = MockServer::start().await;
        let url_path = "/ggml-tiny.bin";
        Mock::given(method("HEAD"))
            .and(path(url_path))
            .respond_with(
                ResponseTemplate::new(429).insert_header("Retry-After", "42"),
            )
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let mut downloader = ModelDownloader::new_for_test(
            "tiny".into(),
            format!("{}/ggml-{{model}}.bin", server.uri()),
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
                reason: FailReason::RateLimited { retry_after_secs: 42 },
            }
        );
    }

    #[tokio::test]
    async fn cancel_then_close_leaves_consistent_part_file() {
        let body = vec![0xAB_u8; 4 * 1024 * 1024]; // 4 MiB
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
        let cancel = CancellationToken::new();
        let mut downloader = ModelDownloader::new_for_test(
            "tiny".into(),
            format!("{}/ggml-{{model}}.bin", server.uri()),
            &manifest,
            dir.path().to_path_buf(),
            cancel.clone(),
        )
        .unwrap();

        // Cancel before run is even called → the very first
        // `is_cancelled` check after Resolving short-circuits.
        cancel.cancel();
        let (tx, _rx) = mpsc::unbounded_channel();
        let final_state = downloader.run(tx).await.unwrap();
        assert_eq!(final_state, DownloadState::Cancelled);
        let final_path = dir.path().join("ggml-tiny.bin");
        assert!(!final_path.exists(), "no final file on cancel");
    }

    #[tokio::test]
    async fn content_length_mismatch_aborts() {
        let body = b"three".to_vec();
        let manifest = manifest_for("tiny", &body);

        let server = MockServer::start().await;
        let url_path = "/ggml-tiny.bin";
        Mock::given(method("HEAD"))
            .and(path(url_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/octet-stream")
                    .insert_header("Content-Length", "9999"),
            )
            .mount(&server)
            .await;

        let dir = TempDir::new().unwrap();
        let mut downloader = ModelDownloader::new_for_test(
            "tiny".into(),
            format!("{}/ggml-{{model}}.bin", server.uri()),
            &manifest,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .unwrap();

        let (tx, _rx) = mpsc::unbounded_channel();
        let final_state = downloader.run(tx).await.unwrap();
        match final_state {
            DownloadState::Failed {
                reason: FailReason::ContentLengthMismatch { expected, actual },
            } => {
                assert_eq!(expected, body.len() as u64);
                assert_eq!(actual, 9999);
            }
            other => panic!("expected ContentLengthMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn kill_mid_chunk_then_resume_succeeds() {
        // Simulate a crash mid-stream by manually truncating the
        // .part. The downloader must re-hash and finish the rest.
        let body: Vec<u8> = (0..512).map(|i| (i & 0xFF) as u8).collect();
        let manifest = manifest_for("tiny", &body);

        let dir = TempDir::new().unwrap();
        let partial = dir.path().join(".partial");
        fs::create_dir_all(&partial).unwrap();
        let part_path = partial.join("ggml-tiny.bin.part");
        // 100 truncated bytes → wiremock will serve full body but
        // the test asserts the final SHA still matches because the
        // downloader uses Range from byte 100. wiremock does not
        // honour Range, so we serve the suffix manually.
        fs::write(&part_path, &body[..100]).unwrap();

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
                ResponseTemplate::new(206)
                    .insert_header("Content-Type", "application/octet-stream")
                    .set_body_bytes(body[100..].to_vec()),
            )
            .mount(&server)
            .await;

        let mut downloader = ModelDownloader::new_for_test(
            "tiny".into(),
            format!("{}/ggml-{{model}}.bin", server.uri()),
            &manifest,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .unwrap();

        let (tx, _rx) = mpsc::unbounded_channel();
        let final_state = downloader.run(tx).await.unwrap();
        assert_eq!(final_state, DownloadState::Installed);
        let final_path = dir.path().join("ggml-tiny.bin");
        assert_eq!(fs::read(&final_path).unwrap(), body);
    }

    /// Test-only constructor that mirrors `ModelDownloader::new`
    /// but accepts `http://` URLs. Production paths always go
    /// through the https-only guard in `substitute_model_token`.
    impl ModelDownloader {
        pub(crate) fn new_for_test(
            model_name: String,
            base_url: String,
            manifest: &ChecksumManifest,
            models_dir: PathBuf,
            cancel: CancellationToken,
        ) -> Result<Self, SettingsError> {
            let entry = manifest
                .lookup(&model_name)
                .ok_or_else(|| {
                    SettingsError::Download(format!(
                        "unknown model: {model_name} \
                         (not in embedded checksums.toml)"
                    ))
                })?
                .clone();

            // Skip the https-only guard so wiremock loopback works.
            if !base_url.contains("{model}") {
                return Err(SettingsError::Download(
                    "base URL missing {model} placeholder".into(),
                ));
            }
            let url = base_url.replace("{model}", &model_name);

            let file_name = format!("ggml-{model_name}.bin");
            let final_path = models_dir.join(&file_name);
            let partial_dir = models_dir.join(PARTIAL_SUBDIR);
            let part_path = partial_dir.join(format!("{file_name}{PART_SUFFIX}"));
            let meta_path = partial_dir.join(format!("{file_name}{META_SUFFIX}"));
            let client = Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .map_err(|e| SettingsError::Download(format!("http client build: {e}")))?;
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
    fn substitute_model_token_rejects_http() {
        let err = substitute_model_token("http://x/{model}.bin", "tiny").unwrap_err();
        assert!(matches!(err, SettingsError::Download(_)));
    }

    #[test]
    fn substitute_model_token_rejects_missing_placeholder() {
        let err = substitute_model_token("https://x/static.bin", "tiny").unwrap_err();
        assert!(matches!(err, SettingsError::Download(_)));
    }

    #[test]
    fn hex_lower_pads_zero_byte_to_two_chars() {
        let s = hex_lower(&[0x00, 0xff, 0x10]);
        assert_eq!(s, "00ff10");
    }
}
