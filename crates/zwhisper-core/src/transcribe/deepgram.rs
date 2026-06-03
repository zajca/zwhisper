//! Deepgram batch REST backend.
//!
//! Uploads a FLAC file to `POST /v1/listen`, parses the JSON response,
//! groups word-level speaker labels into [`SpeakerSegment`]s, and
//! writes `<audio>.txt` + `<audio>.json` next to the input audio.
//!
//! M5 ships **batch** only — streaming WS is M9+. The `Transcriber`
//! trait is unchanged: a single `transcribe_file(&Path)` call.
//!
//! ## Threading model
//!
//! One [`reqwest::Client`] lives behind a `OnceLock` per
//! [`DeepgramBatch`] instance (M5-plan § C2). The daemon caches a
//! single instance across sessions; the CLI builds a fresh one per
//! invocation. No background tasks are spawned — the retry loop
//! lives inline in `DeepgramBatch::transcribe_file`.
//!
//! ## Retry policy
//!
//! Exponential backoff with jitter on transient failures (5xx,
//! 408, 429, connect errors). Bounded BOTH by attempt count
//! ([`DeepgramSettings::max_retries`]) AND by total wall-clock
//! ([`DeepgramSettings::retry_total_budget_s`]) — see M5-plan § C4.
//! 4xx responses other than 408/429 are NOT retried.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Client, ClientBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::fs::File as TokioFile;
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

use crate::profile::schema::DeepgramSettings;
use crate::secrets::{ResolveSource, SecretString, SecretsError, resolve_api_key};

use super::audio_source::AudioSource;
use super::error::TranscribeError;
use super::model::ModelArtifact;
use super::speakers::SpeakerSegment;
use super::{
    AudioInputKind, BackendCapabilities, TranscribeOpts, Transcriber, TranscriptArtifacts,
};

const BACKEND_ID: &str = "deepgram";
const DEFAULT_BASE_URL: &str = "https://api.deepgram.com";
const LISTEN_PATH: &str = "/v1/listen";
const MAX_BODY_EXCERPT: usize = 1024;
/// Initial exponential-backoff delay, in milliseconds. Doubles per
/// attempt up to [`BACKOFF_CAP_MS`].
const BACKOFF_INITIAL_MS: u64 = 500;
/// Cap on the per-attempt backoff delay regardless of attempt count.
/// Wall-clock budget (per profile) gates the *total* time spent in
/// retries; this just stops a runaway grow inside a single iteration.
const BACKOFF_CAP_MS: u64 = 8_000;
/// Cap on a server-supplied `Retry-After` header. The server is not
/// allowed to make us wait arbitrarily long — past 30 s the budget
/// gate would normally fire anyway, but capping here keeps logs
/// predictable.
const RETRY_AFTER_MAX_S: u64 = 30;

/// Constructed from a [`DeepgramSettings`] block. Holds the retry
/// budget and the base URL so the test harness can swap in a
/// `wiremock`-served URL without touching production code paths.
#[derive(Debug, Clone)]
pub struct DeepgramBatch {
    settings: DeepgramSettings,
    base_url: String,
    client_cell: OnceLock<Client>,
}

impl DeepgramBatch {
    /// Production constructor: targets `https://api.deepgram.com`
    /// with the supplied per-profile settings.
    pub fn new(settings: DeepgramSettings) -> Self {
        Self {
            settings,
            base_url: DEFAULT_BASE_URL.to_owned(),
            client_cell: OnceLock::new(),
        }
    }

    /// Test-only constructor that retargets the base URL. Production
    /// callers MUST go through [`Self::new`]. Gated behind the
    /// `test-utils` feature so it is never reachable from a production
    /// build of `zwhisperd` or `zwhisper-cli` (security review #2).
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn with_base_url(settings: DeepgramSettings, base_url: String) -> Self {
        Self {
            settings,
            base_url,
            client_cell: OnceLock::new(),
        }
    }

    /// Test-only entry point that bypasses the environment / TOML
    /// resolver. Used by integration tests to avoid mutating
    /// process-global state. The supplied [`SecretString`] is sent
    /// in the `Authorization: Token <key>` header verbatim. Gated
    /// behind `test-utils` (security review #2, 2026-05-02).
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub async fn transcribe_file_with_key(
        &self,
        audio: &Path,
        opts: &TranscribeOpts,
        key: SecretString,
    ) -> Result<TranscriptArtifacts, TranscribeError> {
        self.do_transcribe_with_key(audio, opts, key).await
    }

    async fn do_transcribe_with_key(
        &self,
        audio: &Path,
        opts: &TranscribeOpts,
        key: SecretString,
    ) -> Result<TranscriptArtifacts, TranscribeError> {
        let started_at = Instant::now();

        let audio_meta =
            tokio::fs::metadata(audio)
                .await
                .map_err(|source| TranscribeError::InputAudio {
                    path: audio.to_path_buf(),
                    source,
                })?;
        if !audio_meta.is_file() {
            return Err(TranscribeError::InputAudio {
                path: audio.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "audio path is not a regular file",
                ),
            });
        }
        let audio_size = audio_meta.len();

        let url = self.build_url(opts)?;
        let headers = build_headers(&key)?;
        // `post_with_retry` reads the body inside its budget cap and
        // returns the bytes directly; the response object never
        // escapes that scope (user feedback #1, 2026-05-02).
        let body_bytes = self
            .post_with_retry(&url, &headers, audio, audio_size)
            .await?;
        let dg_response: DeepgramResponse =
            serde_json::from_slice(&body_bytes).map_err(|source| {
                TranscribeError::BackendJsonShape {
                    backend: BACKEND_ID,
                    source,
                }
            })?;

        let speakers = group_speakers(&dg_response);
        let transcript_text = extract_transcript_text(&dg_response);
        let audio_duration = extract_audio_duration(&dg_response);
        // `TranscriptArtifacts.language` is documented as the
        // *resolved* language — what the backend actually used —
        // not the request's input. When autodetect ran, prefer the
        // detected_language echoed back by Deepgram; fall back to
        // the request value when the field is absent or empty
        // (e.g., diarize-only responses). User feedback #3,
        // 2026-05-02.
        let resolved_language =
            extract_detected_language(&dg_response).unwrap_or_else(|| opts.language.clone());

        let txt_target = append_extension(audio, ".txt");
        let json_target = append_extension(audio, ".json");
        write_text(&txt_target, &transcript_text).await?;

        let resolved_model = self.resolved_model(opts);
        let envelope = TranscriptJsonEnvelope {
            backend: BACKEND_ID,
            model: resolved_model,
            language: &resolved_language,
            audio_duration_s: audio_duration.as_secs_f64(),
            speakers: if speakers.is_empty() {
                None
            } else {
                Some(speakers.as_slice())
            },
            raw: serde_json::from_slice::<serde_json::Value>(&body_bytes).ok(),
        };
        write_json(&json_target, &envelope).await?;

        let duration = started_at.elapsed();
        info!(
            target: "zwhisper_core::transcribe::deepgram",
            backend = BACKEND_ID,
            txt = %txt_target.display(),
            json = %json_target.display(),
            audio_duration_ms = u64::try_from(audio_duration.as_millis()).unwrap_or(u64::MAX),
            wall_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            speaker_segments = speakers.len(),
            language = %resolved_language,
            "transcribe ok",
        );

        Ok(TranscriptArtifacts {
            txt_path: txt_target,
            json_path: json_target,
            duration,
            audio_duration,
            language: resolved_language,
            model: resolved_model.to_owned(),
            speakers: if speakers.is_empty() {
                None
            } else {
                Some(speakers)
            },
        })
    }

    fn client(&self) -> Result<&Client, TranscribeError> {
        if let Some(c) = self.client_cell.get() {
            return Ok(c);
        }
        let timeout = Duration::from_secs(self.settings.timeout_s);
        // `https_only` would block loopback test URLs; the
        // production guarantee comes from
        // [`is_acceptable_base_url`] gating the URL builder before
        // any request is sent. `use_rustls_tls()` keeps the binary
        // free of `native-tls`/OpenSSL linkage (DoD #11).
        let client = ClientBuilder::new()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(self.settings.connect_timeout_s))
            .use_rustls_tls()
            .build()
            .map_err(|source| TranscribeError::BackendNetwork {
                backend: BACKEND_ID,
                source: Box::new(source),
            })?;
        // OnceLock::get_or_init can't propagate Result; do the build
        // out-of-line and ignore the race — at most we discard one
        // freshly-built client if two threads collide on first use.
        let _ = self.client_cell.set(client);
        // Safe: we just set or someone else did; either way the cell
        // is populated.
        self.client_cell
            .get()
            .ok_or_else(|| TranscribeError::BackendConfig {
                backend: BACKEND_ID,
                message: "client OnceLock failed to initialize".to_owned(),
            })
    }

    /// Resolve the model to send: callers can override the
    /// per-profile default by passing a non-empty
    /// [`TranscribeOpts::model`] (e.g., `--model nova-3-large` on the
    /// CLI). Falls back to `[transcription.deepgram].model` from the
    /// profile, which itself defaults to `"nova-3"`. Empty `opts.model`
    /// keeps the legacy "use settings model" behaviour.
    fn resolved_model<'a>(&'a self, opts: &'a TranscribeOpts) -> &'a str {
        if opts.model.is_empty() {
            &self.settings.model
        } else {
            &opts.model
        }
    }

    fn build_url(&self, opts: &TranscribeOpts) -> Result<String, TranscribeError> {
        if !is_acceptable_base_url(&self.base_url) {
            return Err(TranscribeError::BackendConfig {
                backend: BACKEND_ID,
                message: format!("non-https base URL `{}` rejected", self.base_url),
            });
        }
        let mut url = format!("{}{LISTEN_PATH}", self.base_url.trim_end_matches('/'));
        let mut params: Vec<(String, String)> = Vec::with_capacity(8);
        params.push(("model".to_owned(), self.resolved_model(opts).to_owned()));
        // Language / detect_language: honour both the legacy
        // `language == "auto"` shortcut and the explicit
        // `[transcription.deepgram].language_detection = true` knob.
        // Either ON → ask Deepgram to detect; otherwise pin the
        // language so we don't burn an autodetect on a known input.
        let detect = opts.language == "auto" || self.settings.language_detection;
        if detect {
            params.push(("detect_language".to_owned(), "true".to_owned()));
        } else {
            params.push(("language".to_owned(), opts.language.clone()));
        }
        if self.settings.diarize {
            params.push(("diarize".to_owned(), "true".to_owned()));
        }
        params.push(("smart_format".to_owned(), "true".to_owned()));
        params.push(("paragraphs".to_owned(), "true".to_owned()));
        if let Some(tier) = &self.settings.tier {
            params.push(("tier".to_owned(), tier.clone()));
        }
        // Hand-rolled urlencode keeps the dep set narrow. The only
        // non-trivial value users can inject is `tier`; alphanumerics
        // and `-` cover every documented value, so percent-encoding
        // is theoretical defense-in-depth here.
        url.push('?');
        for (i, (k, v)) in params.iter().enumerate() {
            if i > 0 {
                url.push('&');
            }
            url.push_str(k);
            url.push('=');
            url.push_str(&percent_encode(v));
        }
        Ok(url)
    }
}

/// `https://…` is always allowed. `http://127.0.0.1:…` and
/// `http://localhost:…` are allowed too — `wiremock` binds to a
/// loopback URL like `http://127.0.0.1:46791` and tests must be able
/// to point the backend at it.
///
/// **Important:** the loopback exception checks for a host *boundary*
/// after the literal hostname (`:`, `/`, or end-of-string). A bare
/// prefix match would let an attacker register a subdomain like
/// `localhost.evil.com` and bypass the allowlist (security review
/// finding #1, 2026-05-02). Any other plaintext base URL is rejected
/// before any network I/O (`DoD #11`).
fn is_acceptable_base_url(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    is_loopback_http(url, "127.0.0.1") || is_loopback_http(url, "localhost")
}

/// Returns true when `url` starts with `http://<host>` and the
/// character following the host is a port separator (`:`), a path
/// separator (`/`), or the end of the string. Anything else (e.g.
/// `localhost.evil.com`) is rejected.
fn is_loopback_http(url: &str, host: &str) -> bool {
    let prefix = "http://";
    let Some(rest) = url.strip_prefix(prefix) else {
        return false;
    };
    let Some(after_host) = rest.strip_prefix(host) else {
        return false;
    };
    matches!(after_host.chars().next(), None | Some(':' | '/'))
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if safe {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            // `unwrap` on String formatting cannot fail.
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[async_trait]
impl Transcriber for DeepgramBatch {
    fn id(&self) -> &'static str {
        BACKEND_ID
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            // Deepgram batch uploads the encoded FLAC body directly.
            preferred_input: AudioInputKind::EncodedFile,
            accepted_inputs: vec![AudioInputKind::EncodedFile],
            // The model lives behind the provider API — no local artifact.
            accepted_model_kinds: vec![super::ModelKindTag::Remote],
            supports_streaming: false,
            supports_true_diarization: self.settings.diarize,
            languages: vec!["auto"],
        }
    }

    async fn transcribe(
        &self,
        audio: &AudioSource,
        model: &ModelArtifact,
        opts: &TranscribeOpts,
    ) -> Result<TranscriptArtifacts, TranscribeError> {
        // Deepgram is a remote backend; the coordinator already checked
        // the kind, but defend against a non-remote artifact. The model
        // id itself is read from `opts.model` (with the per-profile
        // settings fallback) inside `build_url`.
        if !matches!(model, ModelArtifact::Remote { .. }) {
            return Err(TranscribeError::UnsupportedModelKind {
                backend: BACKEND_ID,
                kind: match model {
                    ModelArtifact::File(_) => "single-file",
                    ModelArtifact::Directory(_) => "directory-bundle",
                    ModelArtifact::Remote { .. } => "remote",
                },
            });
        }
        let (key, source) =
            resolve_api_key(BACKEND_ID).map_err(|source| TranscribeError::BackendKeyMissing {
                backend: BACKEND_ID,
                source,
            })?;
        debug!(
            target: "zwhisper_core::transcribe::deepgram",
            backend = BACKEND_ID,
            source = %source_label(&source),
            "API key resolved",
        );
        self.do_transcribe_with_key(audio.artifact_path(), opts, key)
            .await
    }
}

/// One attempt's wire result, captured fully (status + headers we
/// care about + body bytes) inside the per-attempt budget cap. Body
/// is read inside the same `tokio::time::timeout(attempt_cap, …)`
/// scope as the request itself so a slow body cannot extend wall-
/// clock time past the budget (user feedback #1, 2026-05-02).
struct AttemptOutcome {
    status: StatusCode,
    retry_after: Option<u64>,
    body: Vec<u8>,
}

impl DeepgramBatch {
    // Retry orchestration is intentionally inline rather than split
    // into per-status helpers — every branch reads stack-local state
    // (`attempt`, `started`, `budget`, `client`, `headers`) that
    // would just become arguments. M5 user feedback #1 split off
    // body-read into the timeout scope; the loop is the simplest
    // single source of truth.
    #[allow(clippy::too_many_lines)]
    async fn post_with_retry(
        &self,
        url: &str,
        headers: &HeaderMap,
        audio: &Path,
        audio_size: u64,
    ) -> Result<Vec<u8>, TranscribeError> {
        let client = self.client()?;
        let budget = Duration::from_secs(self.settings.retry_total_budget_s);
        let started = Instant::now();

        let mut attempt: u32 = 0;
        let max_attempts = self.settings.max_retries.saturating_add(1);

        loop {
            attempt += 1;
            let elapsed = started.elapsed();
            if elapsed >= budget {
                return Err(TranscribeError::BackendTimeout {
                    backend: BACKEND_ID,
                    timeout_s: self.settings.retry_total_budget_s,
                });
            }

            // Body has to be re-built per attempt because the stream
            // is consumed on send.
            let body = open_body_stream(audio, audio_size).await?;
            let req = client
                .post(url)
                .headers(headers.clone())
                .body(body)
                .build()
                .map_err(|source| TranscribeError::BackendNetwork {
                    backend: BACKEND_ID,
                    source: Box::new(source),
                })?;

            // Cap each attempt — including the body read — by the
            // *remaining* wall-clock budget. The previous version
            // bounded only `client.execute(req)` and let the body
            // read run unbounded, so a slow body would silently
            // overshoot the budget (user feedback #1, 2026-05-02).
            let attempt_cap = budget.saturating_sub(started.elapsed());
            let attempt_result = tokio::time::timeout(attempt_cap, async {
                let resp = client.execute(req).await?;
                let status = resp.status();
                let retry_after = parse_retry_after(resp.headers());
                let bytes = resp.bytes().await?;
                Ok::<_, reqwest::Error>(AttemptOutcome {
                    status,
                    retry_after,
                    body: bytes.to_vec(),
                })
            })
            .await;

            let Ok(send_result) = attempt_result else {
                return Err(TranscribeError::BackendTimeout {
                    backend: BACKEND_ID,
                    timeout_s: self.settings.retry_total_budget_s,
                });
            };
            match send_result {
                Ok(outcome) => {
                    if outcome.status.is_success() {
                        return Ok(outcome.body);
                    }
                    if !status_is_retryable(outcome.status) || attempt >= max_attempts {
                        return Err(classify_http_error(
                            outcome.status,
                            &outcome.body,
                            outcome.retry_after,
                        ));
                    }
                    let delay = backoff_delay(attempt, outcome.retry_after);
                    let remaining = budget.checked_sub(started.elapsed()).unwrap_or_default();
                    if delay >= remaining {
                        // Budget exhausted while we still hold the
                        // server response — classify it (BackendQuota
                        // for 429/402, BackendBadResponse for 5xx)
                        // instead of falsely surfacing a local
                        // timeout. Honors the typed-error contract:
                        // a quota error is what the user sees.
                        return Err(classify_http_error(
                            outcome.status,
                            &outcome.body,
                            outcome.retry_after,
                        ));
                    }
                    warn!(
                        target: "zwhisper_core::transcribe::deepgram",
                        attempt,
                        max_attempts,
                        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        status = outcome.status.as_u16(),
                        "retrying after transient HTTP failure",
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(err) => {
                    let retryable = err.is_connect() || err.is_timeout();
                    if !retryable || attempt >= max_attempts {
                        if err.is_timeout() {
                            return Err(TranscribeError::BackendTimeout {
                                backend: BACKEND_ID,
                                timeout_s: self.settings.timeout_s,
                            });
                        }
                        return Err(TranscribeError::BackendNetwork {
                            backend: BACKEND_ID,
                            source: Box::new(err),
                        });
                    }
                    let delay = backoff_delay(attempt, None);
                    let remaining = budget.checked_sub(started.elapsed()).unwrap_or_default();
                    if delay >= remaining {
                        return Err(TranscribeError::BackendTimeout {
                            backend: BACKEND_ID,
                            timeout_s: self.settings.retry_total_budget_s,
                        });
                    }
                    warn!(
                        target: "zwhisper_core::transcribe::deepgram",
                        attempt,
                        max_attempts,
                        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        is_connect = err.is_connect(),
                        is_timeout = err.is_timeout(),
                        "retrying after transient transport failure",
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

fn build_headers(key: &SecretString) -> Result<HeaderMap, TranscribeError> {
    let mut headers = HeaderMap::with_capacity(2);
    let mut auth =
        HeaderValue::from_str(&format!("Token {}", key.expose_secret())).map_err(|_| {
            TranscribeError::BackendConfig {
                backend: BACKEND_ID,
                message: "API key contains characters not valid in an HTTP header".to_owned(),
            }
        })?;
    auth.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("audio/flac"));
    Ok(headers)
}

async fn open_body_stream(path: &Path, _size: u64) -> Result<reqwest::Body, TranscribeError> {
    let file = TokioFile::open(path)
        .await
        .map_err(|source| TranscribeError::InputAudio {
            path: path.to_path_buf(),
            source,
        })?;
    let stream = ReaderStream::new(file);
    Ok(reqwest::Body::wrap_stream(stream))
}

fn status_is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429) || status.is_server_error()
}

fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    raw.parse::<u64>().ok()
}

fn backoff_delay(attempt: u32, retry_after: Option<u64>) -> Duration {
    if let Some(s) = retry_after {
        return Duration::from_secs(s.min(RETRY_AFTER_MAX_S));
    }
    let base_ms = BACKOFF_INITIAL_MS
        .saturating_mul(1u64 << attempt.min(5).saturating_sub(1))
        .min(BACKOFF_CAP_MS);
    let jitter = jitter_ms(base_ms);
    Duration::from_millis(base_ms.saturating_add(jitter))
}

fn jitter_ms(base_ms: u64) -> u64 {
    // Cheap deterministic-ish jitter from clock nanos. Not crypto.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0u64, |d| u64::from(d.subsec_nanos()));
    nanos % (base_ms / 4 + 1)
}

/// Classify a non-2xx response into a typed error. Pure-sync because
/// the body has already been read inside the budget-bounded attempt
/// scope (user feedback #1, 2026-05-02). Previously this awaited
/// `resp.bytes()` outside the budget cap, which let a slow error
/// body extend wall-clock time past `retry_total_budget_s`.
fn classify_http_error(
    status: StatusCode,
    body_bytes: &[u8],
    retry_after: Option<u64>,
) -> TranscribeError {
    let body = String::from_utf8_lossy(body_bytes).into_owned();
    let excerpt = truncate_body(&body);

    match status.as_u16() {
        401 | 403 => TranscribeError::BackendAuth {
            backend: BACKEND_ID,
            status: status.as_u16(),
        },
        402 | 429 => TranscribeError::BackendQuota {
            backend: BACKEND_ID,
            status: status.as_u16(),
            retry_after_s: retry_after,
            message: excerpt,
        },
        _ => TranscribeError::BackendBadResponse {
            backend: BACKEND_ID,
            status: status.as_u16(),
            body_excerpt: excerpt,
        },
    }
}

fn truncate_body(body: &str) -> String {
    if body.len() <= MAX_BODY_EXCERPT {
        return body.to_owned();
    }
    let mut cut = MAX_BODY_EXCERPT;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &body[..cut])
}

fn append_extension(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

async fn write_text(path: &Path, text: &str) -> Result<(), TranscribeError> {
    tokio::fs::write(path, text)
        .await
        .map_err(|source| TranscribeError::ArtifactWrite {
            backend: BACKEND_ID,
            path: path.to_path_buf(),
            source,
        })
}

async fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), TranscribeError> {
    let body =
        serde_json::to_vec_pretty(value).map_err(|source| TranscribeError::BackendJsonShape {
            backend: BACKEND_ID,
            source,
        })?;
    tokio::fs::write(path, body)
        .await
        .map_err(|source| TranscribeError::ArtifactWrite {
            backend: BACKEND_ID,
            path: path.to_path_buf(),
            source,
        })
}

fn source_label(src: &ResolveSource) -> &'static str {
    match src {
        ResolveSource::Env(_) => "env",
        ResolveSource::File(_) => "file",
    }
}

// ---------- response shape ----------

#[derive(Debug, Deserialize)]
struct DeepgramResponse {
    #[serde(default)]
    metadata: Option<DeepgramMetadata>,
    #[serde(default)]
    results: Option<DeepgramResults>,
}

#[derive(Debug, Deserialize)]
struct DeepgramMetadata {
    /// Recording duration in seconds, server-reported.
    #[serde(default)]
    duration: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DeepgramResults {
    #[serde(default)]
    channels: Vec<DeepgramChannel>,
}

#[derive(Debug, Deserialize)]
struct DeepgramChannel {
    #[serde(default)]
    alternatives: Vec<DeepgramAlternative>,
    /// Per-channel detected language (BCP-47-ish), present when the
    /// request set `detect_language=true`. Used to fill in
    /// `TranscriptArtifacts.language` after autodetect (user
    /// feedback #3, 2026-05-02). Falls back to `metadata` or
    /// `opts.language` when absent.
    #[serde(default)]
    detected_language: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeepgramAlternative {
    #[serde(default)]
    transcript: Option<String>,
    #[serde(default)]
    words: Vec<DeepgramWord>,
}

/// A single word from the Deepgram response. `speaker` is `Option`
/// because (a) older models intermittently omit it (M5-plan § C1),
/// (b) `diarize=false` requests skip it entirely, (c) some languages
/// have known coverage gaps. Missing values are treated as
/// "unattributed" (sentinel speaker id `u32::MAX`).
///
/// `punctuated_word` is preferred over `word` when present (Deepgram
/// returns the smart-format-friendly version there); we fall back to
/// the lowercase `word` field if the response was rendered without
/// `smart_format=true`.
#[derive(Debug, Deserialize)]
struct DeepgramWord {
    word: String,
    #[serde(default)]
    punctuated_word: Option<String>,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    end: f64,
    #[serde(default)]
    speaker: Option<u32>,
}

impl DeepgramWord {
    fn display_text(&self) -> &str {
        self.punctuated_word.as_deref().unwrap_or(&self.word)
    }
}

const UNATTRIBUTED: u32 = u32::MAX;

fn group_speakers(resp: &DeepgramResponse) -> Vec<SpeakerSegment> {
    let Some(results) = &resp.results else {
        return Vec::new();
    };
    let Some(channel) = results.channels.first() else {
        return Vec::new();
    };
    let Some(alt) = channel.alternatives.first() else {
        return Vec::new();
    };

    let mut segments: Vec<SpeakerSegment> = Vec::new();
    for word in &alt.words {
        let speaker_id = word.speaker.unwrap_or(UNATTRIBUTED);
        let text = word.display_text();
        match segments.last_mut() {
            Some(seg) if seg.speaker_id == speaker_id => {
                seg.end_s = word.end;
                if !seg.text.is_empty() {
                    seg.text.push(' ');
                }
                seg.text.push_str(text);
            }
            _ => segments.push(SpeakerSegment {
                speaker_id,
                start_s: word.start,
                end_s: word.end,
                text: text.to_owned(),
            }),
        }
    }
    // Silent-failure review #1 (2026-05-02): if EVERY word came back
    // without a `speaker` field (older models, certain languages),
    // the loop produced one giant sentinel segment. That is a
    // false claim of "diarization ran". Drop the segment list so
    // the JSON envelope omits the `speakers` array and
    // `TranscriptArtifacts.speakers = None`.
    if segments.iter().all(|s| s.speaker_id == UNATTRIBUTED) {
        return Vec::new();
    }
    segments
}

/// Pull the resolved language code out of the Deepgram response.
/// `Option` because not every request asks for autodetect, and the
/// field is sometimes absent when diarization runs without
/// `detect_language=true`. Empty strings are treated as absent so
/// we don't overwrite `opts.language` with a useless empty value.
fn extract_detected_language(resp: &DeepgramResponse) -> Option<String> {
    let lang = resp
        .results
        .as_ref()?
        .channels
        .first()?
        .detected_language
        .as_ref()?;
    if lang.is_empty() {
        return None;
    }
    Some(lang.clone())
}

fn extract_transcript_text(resp: &DeepgramResponse) -> String {
    resp.results
        .as_ref()
        .and_then(|r| r.channels.first())
        .and_then(|c| c.alternatives.first())
        .and_then(|a| a.transcript.clone())
        .unwrap_or_default()
}

fn extract_audio_duration(resp: &DeepgramResponse) -> Duration {
    let secs = resp
        .metadata
        .as_ref()
        .and_then(|m| m.duration)
        .unwrap_or(0.0)
        .max(0.0);
    Duration::from_secs_f64(secs)
}

#[derive(Debug, Serialize)]
struct TranscriptJsonEnvelope<'a> {
    backend: &'static str,
    model: &'a str,
    language: &'a str,
    audio_duration_s: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    speakers: Option<&'a [SpeakerSegment]>,
    /// Original Deepgram body (untouched). `Option` so a parse-only
    /// failure does not block the artifact write — the structured
    /// fields above still capture the useful info.
    #[serde(skip_serializing_if = "Option::is_none")]
    raw: Option<serde_json::Value>,
}

// Stop-gap: SecretsError wrapped inside TranscribeError needs to be
// `Send + Sync` — `thiserror`'s #[source] handles that automatically
// because SecretsError already is.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SecretsError>();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::secrets::ResolverConfig;

    fn settings() -> DeepgramSettings {
        DeepgramSettings {
            model: "nova-3".to_owned(),
            diarize: true,
            language_detection: false,
            tier: None,
            timeout_s: 30,
            connect_timeout_s: 5,
            max_retries: 2,
            retry_total_budget_s: 5,
        }
    }

    #[test]
    fn build_url_includes_model_and_diarize() {
        let backend = DeepgramBatch::new(settings());
        let opts = TranscribeOpts {
            backend: crate::profile::schema::Backend::Deepgram,
            model: "nova-3".to_owned(),
            language: "en".to_owned(),
            ..Default::default()
        };
        let url = backend.build_url(&opts).unwrap();
        assert!(url.contains("model=nova-3"), "{url}");
        assert!(url.contains("language=en"), "{url}");
        assert!(url.contains("diarize=true"), "{url}");
        assert!(url.contains("smart_format=true"), "{url}");
        assert!(url.contains("paragraphs=true"), "{url}");
        assert!(!url.contains("detect_language"), "{url}");
    }

    #[test]
    fn opts_model_overrides_settings_model() {
        // User-feedback fix #1 (2026-05-02): `--model` on the CLI
        // (or any caller-supplied non-empty model) MUST be honoured.
        // The previous build silently used `settings.model` always.
        let backend = DeepgramBatch::new(settings());
        let opts = TranscribeOpts {
            backend: crate::profile::schema::Backend::Deepgram,
            model: "nova-3-general".to_owned(),
            language: "en".to_owned(),
            ..Default::default()
        };
        let url = backend.build_url(&opts).unwrap();
        assert!(url.contains("model=nova-3-general"), "{url}");
        assert!(
            !url.contains("model=nova-3&"),
            "settings.model leaked: {url}"
        );
    }

    #[test]
    fn empty_opts_model_falls_back_to_settings() {
        let backend = DeepgramBatch::new(settings());
        let opts = TranscribeOpts {
            backend: crate::profile::schema::Backend::Deepgram,
            model: String::new(),
            language: "en".to_owned(),
            ..Default::default()
        };
        let url = backend.build_url(&opts).unwrap();
        assert!(url.contains("model=nova-3"), "{url}");
    }

    #[test]
    fn language_detection_setting_forces_detect_language() {
        // User-feedback fix #1 (2026-05-02): the
        // `[transcription.deepgram].language_detection = true` knob
        // MUST be honoured even when `opts.language` is a concrete
        // ISO code. Previously the field was schema-only.
        let mut s = settings();
        s.language_detection = true;
        let backend = DeepgramBatch::new(s);
        let opts = TranscribeOpts {
            backend: crate::profile::schema::Backend::Deepgram,
            model: "nova-3".to_owned(),
            language: "en".to_owned(),
            ..Default::default()
        };
        let url = backend.build_url(&opts).unwrap();
        assert!(url.contains("detect_language=true"), "{url}");
        assert!(
            !url.contains("&language=en") && !url.contains("?language=en"),
            "{url}"
        );
    }

    #[test]
    fn build_url_emits_detect_language_for_auto() {
        let backend = DeepgramBatch::new(settings());
        let opts = TranscribeOpts {
            backend: crate::profile::schema::Backend::Deepgram,
            model: "nova-3".to_owned(),
            language: "auto".to_owned(),
            ..Default::default()
        };
        let url = backend.build_url(&opts).unwrap();
        assert!(url.contains("detect_language=true"), "{url}");
        // No `&language=…` or `?language=…` segment when auto-detect.
        assert!(
            !url.contains("&language=") && !url.contains("?language="),
            "{url}"
        );
    }

    #[test]
    fn build_url_skips_diarize_when_disabled() {
        let mut s = settings();
        s.diarize = false;
        let backend = DeepgramBatch::new(s);
        let opts = TranscribeOpts {
            backend: crate::profile::schema::Backend::Deepgram,
            model: "nova-3".to_owned(),
            language: "en".to_owned(),
            ..Default::default()
        };
        let url = backend.build_url(&opts).unwrap();
        assert!(!url.contains("diarize"), "{url}");
    }

    #[test]
    fn build_url_includes_tier_when_set() {
        let mut s = settings();
        s.tier = Some("enhanced".to_owned());
        let backend = DeepgramBatch::new(s);
        let opts = TranscribeOpts {
            backend: crate::profile::schema::Backend::Deepgram,
            model: "nova-3".to_owned(),
            language: "en".to_owned(),
            ..Default::default()
        };
        let url = backend.build_url(&opts).unwrap();
        assert!(url.contains("tier=enhanced"), "{url}");
    }

    #[test]
    fn rejects_non_https_non_loopback_base_url() {
        let opts = TranscribeOpts {
            backend: crate::profile::schema::Backend::Deepgram,
            model: "nova-3".to_owned(),
            language: "en".to_owned(),
            ..Default::default()
        };
        // Plaintext public host → reject (DoD #11).
        let evil = DeepgramBatch::with_base_url(settings(), "http://evil.example/".to_owned());
        let err = evil.build_url(&opts).unwrap_err();
        assert!(matches!(err, TranscribeError::BackendConfig { .. }));
        assert!(err.to_string().contains("non-https"));
        // ftp:// → reject.
        let ftp = DeepgramBatch::with_base_url(settings(), "ftp://evil.example/".to_owned());
        assert!(ftp.build_url(&opts).is_err());
        // Loopback → accept (test fixture).
        let loop_ok = DeepgramBatch::with_base_url(settings(), "http://127.0.0.1:46791".to_owned());
        assert!(loop_ok.build_url(&opts).is_ok());
    }

    #[test]
    fn is_acceptable_base_url_truth_table() {
        assert!(is_acceptable_base_url("https://api.deepgram.com"));
        assert!(is_acceptable_base_url("https://example.com:443/x"));
        assert!(is_acceptable_base_url("http://127.0.0.1:46791"));
        assert!(is_acceptable_base_url("http://127.0.0.1/path"));
        assert!(is_acceptable_base_url("http://127.0.0.1"));
        assert!(is_acceptable_base_url("http://localhost:8080"));
        assert!(is_acceptable_base_url("http://localhost"));
        assert!(!is_acceptable_base_url("http://api.deepgram.com"));
        assert!(!is_acceptable_base_url("ftp://api.deepgram.com"));
        assert!(!is_acceptable_base_url(""));
        // Security review #1 (2026-05-02): bare-prefix bypass via
        // attacker-controlled subdomain.
        assert!(!is_acceptable_base_url("http://localhost.evil.com"));
        assert!(!is_acceptable_base_url("http://localhost.attacker.com:80"));
        assert!(!is_acceptable_base_url("http://127.0.0.1.evil.com"));
        assert!(!is_acceptable_base_url("http://127.0.0.10:8080"));
    }

    #[test]
    fn group_speakers_collapses_all_missing_to_empty() {
        // Silent-failure review #1: when every word lacks a speaker
        // field, we must NOT report a single giant sentinel segment.
        let resp: DeepgramResponse = serde_json::from_str(
            r#"{
              "results": {
                "channels": [{
                  "alternatives": [{
                    "transcript": "hello there",
                    "words": [
                      { "word": "hello", "start": 0.0, "end": 0.5 },
                      { "word": "there", "start": 0.5, "end": 1.0 }
                    ]
                  }]
                }]
              }
            }"#,
        )
        .unwrap();
        let segs = group_speakers(&resp);
        assert!(segs.is_empty(), "all-missing should collapse to empty Vec");
    }

    #[test]
    fn group_speakers_keeps_partial_attribution() {
        // First word labelled, second not — keep the labelled one,
        // do NOT drop the segment.
        let resp: DeepgramResponse = serde_json::from_str(
            r#"{
              "results": {
                "channels": [{
                  "alternatives": [{
                    "transcript": "hello there",
                    "words": [
                      { "word": "hello", "start": 0.0, "end": 0.5, "speaker": 0 },
                      { "word": "there", "start": 0.5, "end": 1.0 }
                    ]
                  }]
                }]
              }
            }"#,
        )
        .unwrap();
        let segs = group_speakers(&resp);
        // Two segments: speaker 0 (hello), sentinel (there). Not
        // empty, because at least one segment has a real speaker id.
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].speaker_id, 0);
        assert_eq!(segs[1].speaker_id, UNATTRIBUTED);
    }

    #[test]
    fn group_speakers_collapses_consecutive_same_speaker() {
        let resp: DeepgramResponse = serde_json::from_str(
            r#"{
              "results": {
                "channels": [{
                  "alternatives": [{
                    "transcript": "hi how are you",
                    "words": [
                      { "word": "hi",  "start": 0.0, "end": 0.4, "speaker": 0 },
                      { "word": "how", "start": 0.5, "end": 0.8, "speaker": 0 },
                      { "word": "are", "start": 1.0, "end": 1.3, "speaker": 1 },
                      { "word": "you", "start": 1.4, "end": 1.7, "speaker": 1 }
                    ]
                  }]
                }]
              }
            }"#,
        )
        .unwrap();
        let segs = group_speakers(&resp);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].speaker_id, 0);
        assert_eq!(segs[0].text, "hi how");
        // Compare floats via abs-diff: clippy::float_cmp.
        assert!((segs[0].start_s - 0.0_f64).abs() < f64::EPSILON);
        assert!((segs[0].end_s - 0.8_f64).abs() < 1e-9);
        assert_eq!(segs[1].speaker_id, 1);
        assert_eq!(segs[1].text, "are you");
    }

    #[test]
    fn extract_detected_language_picks_up_first_channel() {
        let resp: DeepgramResponse = serde_json::from_str(
            r#"{
              "results": {
                "channels": [{
                  "detected_language": "cs",
                  "alternatives": []
                }]
              }
            }"#,
        )
        .unwrap();
        assert_eq!(extract_detected_language(&resp).as_deref(), Some("cs"));
    }

    #[test]
    fn extract_detected_language_treats_empty_as_absent() {
        let resp: DeepgramResponse = serde_json::from_str(
            r#"{"results":{"channels":[{"detected_language":"","alternatives":[]}]}}"#,
        )
        .unwrap();
        assert!(extract_detected_language(&resp).is_none());
    }

    #[test]
    fn extract_detected_language_returns_none_when_absent() {
        let resp: DeepgramResponse =
            serde_json::from_str(r#"{"results":{"channels":[{"alternatives":[]}]}}"#).unwrap();
        assert!(extract_detected_language(&resp).is_none());
    }

    #[test]
    fn extract_audio_duration_handles_missing_metadata() {
        let resp: DeepgramResponse =
            serde_json::from_str(r#"{"results":{"channels":[]}}"#).unwrap();
        assert_eq!(extract_audio_duration(&resp), Duration::ZERO);
    }

    #[test]
    fn extract_audio_duration_clamps_negative() {
        let resp: DeepgramResponse =
            serde_json::from_str(r#"{"metadata":{"duration":-3.0}}"#).unwrap();
        assert_eq!(extract_audio_duration(&resp), Duration::ZERO);
    }

    #[test]
    fn status_retry_classification() {
        for s in [408u16, 429, 500, 502, 503, 504] {
            assert!(status_is_retryable(StatusCode::from_u16(s).unwrap()), "{s}");
        }
        for s in [400u16, 401, 402, 403, 404, 413] {
            assert!(
                !status_is_retryable(StatusCode::from_u16(s).unwrap()),
                "{s}"
            );
        }
    }

    #[test]
    fn backoff_caps_at_8_seconds() {
        for attempt in 1..=10 {
            let d = backoff_delay(attempt, None);
            assert!(d <= Duration::from_secs(10), "{attempt}: {d:?}");
        }
    }

    #[test]
    fn header_value_marked_sensitive() {
        let key = SecretString::new("sk-fixture-1234567890".to_owned());
        let h = build_headers(&key).unwrap();
        let auth = h.get(AUTHORIZATION).unwrap();
        assert!(
            auth.is_sensitive(),
            "Authorization header must be sensitive"
        );
        // Header value can still be exposed via to_str — that is the
        // single legitimate consumer (reqwest serializing the wire).
        // The protection is `set_sensitive` keeping it out of
        // `Debug` impls and tracing spans.
        let dbg = format!("{auth:?}");
        assert_eq!(dbg, "Sensitive");
    }

    #[test]
    fn append_extension_preserves_full_filename() {
        let p = Path::new("/tmp/clip.flac");
        let txt = append_extension(p, ".txt");
        assert_eq!(txt, PathBuf::from("/tmp/clip.flac.txt"));
    }

    #[test]
    fn capabilities_reports_diarize_setting() {
        let mut s = settings();
        s.diarize = false;
        let backend = DeepgramBatch::new(s);
        assert!(!backend.capabilities().supports_true_diarization);
        let backend2 = DeepgramBatch::new(settings());
        assert!(backend2.capabilities().supports_true_diarization);
        assert!(!backend2.capabilities().supports_streaming);
    }

    /// Touch the resolver path with a config that injects an empty
    /// env and a missing file. Confirms the `BackendKeyMissing` wrap
    /// happens (`DoD` #2 partial — rest is in the integration test).
    #[test]
    fn missing_key_classification_is_typed() {
        // This test does not call transcribe_file (which would do a
        // process-wide env read) — instead it exercises the same
        // error mapping at a smaller scale.
        let cfg = ResolverConfig {
            secrets_path: Some(PathBuf::from("/nonexistent/zwhisper-test/secrets.toml")),
            env: crate::secrets::resolver::EnvLookup::Fake(vec![]),
        };
        let err =
            crate::secrets::resolver::resolve_api_key_with_config(BACKEND_ID, &cfg).unwrap_err();
        let wrapped = TranscribeError::BackendKeyMissing {
            backend: BACKEND_ID,
            source: err,
        };
        assert!(wrapped.to_string().contains("deepgram"));
        assert!(wrapped.to_string().contains("ZWHISPER_DEEPGRAM_API_KEY"));
    }
}
