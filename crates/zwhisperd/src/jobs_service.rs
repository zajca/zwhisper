//! Server-side `cz.zajca.Zwhisper1.Jobs1` interface (RFC-daemon-role
//! Feature 1).
//!
//! Mirrors the proxy trait in `zwhisper-ipc::jobs` — same method set,
//! same signal set, same wire signatures. The mirror is mandatory
//! because zbus's `#[interface]` macro decorates an `impl` on a
//! server-owned struct, while `#[proxy]` decorates a free trait (see
//! `recorder_service.rs` for the full rationale).

// The zbus `#[interface]` macro expands `transcribe_file` into a dispatch
// trampoline that takes the five wire args plus injected connection /
// header / emitter handles, tripping `too_many_arguments` on generated
// code we do not control. The wire arity itself is deliberate.
#![allow(clippy::too_many_arguments)]

use std::path::{Path, PathBuf};

use tracing::info;
use zbus::object_server::SignalEmitter;
use zwhisper_core::audio::state::SessionId;
use zwhisper_core::profile::schema::{Backend, DeepgramSettings};
use zwhisper_core::transcribe::{AudioCodec, BackendSettings, TranscribeOpts};
use zwhisper_ipc::{JobInfo, RpcError};

use crate::history::{HistoryHandle, writer::new_entry};
use crate::jobs::queue::{JobSource, JobSpec};
use crate::jobs::{JobId, JobQueue, SubmitMode};

/// State held by the `Jobs1` interface impl. Cloned cheaply via the
/// inner `Arc`s held by [`JobQueue`] / [`HistoryHandle`].
#[derive(Debug)]
pub(crate) struct JobsInterface {
    queue: JobQueue,
    history: HistoryHandle,
}

impl JobsInterface {
    pub(crate) fn new(queue: JobQueue, history: HistoryHandle) -> Self {
        Self { queue, history }
    }
}

/// Validate an incoming `TranscribeFile` path (F1.4).
///
/// The daemon runs as the invoking user (no privilege boundary), so
/// validation is about rejecting nonsense, not sandboxing: the path must
/// resolve (canonicalize, which also collapses any `..` traversal), must
/// exist, and must be a *regular* file (no device nodes, FIFOs, dirs).
/// A missing path is `AudioNotFound`; anything else is `InvalidPath`.
fn validate_audio_path(raw: &str) -> Result<PathBuf, RpcError> {
    if raw.trim().is_empty() {
        return Err(RpcError::InvalidPath {
            reason: "empty path".to_owned(),
        });
    }
    let path = Path::new(raw);
    let meta = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RpcError::AudioNotFound {
                path: raw.to_owned(),
            }
        } else {
            RpcError::InvalidPath {
                reason: format!("cannot stat {raw}: {e}"),
            }
        }
    })?;
    if !meta.is_file() {
        return Err(RpcError::InvalidPath {
            reason: format!("{raw} is not a regular file"),
        });
    }
    // Canonicalize so a later decode sees an absolute, traversal-free
    // path; failure here is treated as invalid.
    std::fs::canonicalize(path).map_err(|e| RpcError::InvalidPath {
        reason: format!("cannot canonicalize {raw}: {e}"),
    })
}

#[zbus::interface(name = "cz.zajca.Zwhisper1.Jobs1")]
impl JobsInterface {
    /// Enqueue a standalone transcription; returns the `job_id`
    /// immediately. The CLI decides whether to wait (`--queue`) or
    /// return (`--detach`); the daemon treats an explicit
    /// `TranscribeFile` as `Detached` for the stale-clipboard guard
    /// (F3.3) — the safe default when intent is unknown.
    #[allow(clippy::too_many_arguments)] // Matches the Jobs1.TranscribeFile wire arity.
    async fn transcribe_file(
        &self,
        path: &str,
        backend: &str,
        model: &str,
        lang: &str,
        submit_mode: &str,
    ) -> zbus::fdo::Result<String> {
        info!(%path, %backend, %model, %lang, %submit_mode, "Jobs1.TranscribeFile");
        let audio = validate_audio_path(path).map_err(zbus::fdo::Error::from)?;
        // Intent for the stale-clipboard guard (F3.3). Unknown → the
        // safe default (Detached → notify-with-action, never a surprise
        // paste-bomb). `Auto` is daemon-internal and never sent here.
        let submit_mode = match submit_mode {
            "foreground" => SubmitMode::Foreground,
            _ => SubmitMode::Detached,
        };

        let backend = if backend.trim().is_empty() {
            Backend::WhisperCpp
        } else {
            Backend::from_id(backend).ok_or_else(|| {
                zbus::fdo::Error::from(RpcError::InvalidPath {
                    reason: format!(
                        "unknown backend `{backend}` (supported: whisper-cpp, deepgram, parakeet)"
                    ),
                })
            })?
        };
        let language = if lang.trim().is_empty() {
            "auto".to_owned()
        } else {
            lang.to_owned()
        };
        let settings = match backend {
            Backend::Deepgram => BackendSettings {
                deepgram: Some(DeepgramSettings::default()),
                ..Default::default()
            },
            _ => BackendSettings::default(),
        };
        let opts = TranscribeOpts {
            backend,
            model: model.to_owned(),
            language: language.clone(),
            settings,
        };

        let session_id = SessionId::new().to_string();
        let codec = AudioCodec::from_path(&audio)
            .map(|c| format!("{c:?}").to_lowercase())
            .unwrap_or_else(|| "unknown".to_owned());
        let label = audio
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("transcribe")
            .to_owned();

        // Record the entry BEFORE enqueue so a crash between here and
        // the job running still leaves a queryable, recoverable trace.
        self.history
            .upsert(new_entry(
                &session_id,
                "",
                &audio.display().to_string(),
                &codec,
                0,
                0,
                backend.as_str(),
                model,
                &language,
            ))
            .await;

        let job_id = self
            .queue
            .submit(JobSpec {
                session_id,
                source: JobSource::File(audio),
                opts,
                profile: String::new(),
                outputs: Vec::new(),
                submit_mode,
                label,
                done: None,
            })
            .map_err(zbus::fdo::Error::from)?;
        Ok(job_id.to_string())
    }

    /// Cancel a queued or running job (best-effort).
    #[allow(clippy::unused_async)] // zbus #[interface] requires async fn.
    async fn cancel(&self, job_id: &str) -> zbus::fdo::Result<()> {
        info!(%job_id, "Jobs1.Cancel");
        let Some(parsed) = JobId::parse(job_id) else {
            return Err(RpcError::JobUnknown {
                id: job_id.to_owned(),
            }
            .into());
        };
        self.queue.cancel(&parsed).map_err(zbus::fdo::Error::from)
    }

    /// Snapshot the active queue.
    #[allow(clippy::unused_async)] // zbus #[interface] requires async fn.
    async fn list_jobs(&self) -> zbus::fdo::Result<Vec<JobInfo>> {
        Ok(self.queue.list())
    }

    /// Per-interface protocol version (F4.1).
    #[zbus(property)]
    #[allow(clippy::unused_self, reason = "zbus property handlers must take &self")]
    fn protocol_version(&self) -> &'static str {
        zwhisper_ipc::PROTOCOL_VERSION
    }

    #[zbus(signal)]
    async fn job_completed(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        submit_mode: &str,
        profile: &str,
        outputs: Vec<Vec<String>>,
        transcript_path: &str,
        bytes: u64,
        backend: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn job_failed(emitter: &SignalEmitter<'_>, job_id: &str, error: &str)
    -> zbus::Result<()>;

    #[zbus(signal)]
    async fn job_progress(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        state: &str,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn validate_rejects_empty() {
        assert!(matches!(
            validate_audio_path(""),
            Err(RpcError::InvalidPath { .. })
        ));
    }

    #[test]
    fn validate_missing_is_audio_not_found() {
        assert!(matches!(
            validate_audio_path("/nonexistent/zzz.flac"),
            Err(RpcError::AudioNotFound { .. })
        ));
    }

    #[test]
    fn validate_dir_is_invalid() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            validate_audio_path(&dir.path().display().to_string()),
            Err(RpcError::InvalidPath { .. })
        ));
    }

    #[test]
    fn validate_regular_file_ok() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("clip.flac");
        std::fs::write(&f, b"x").unwrap();
        let got = validate_audio_path(&f.display().to_string()).unwrap();
        assert!(got.is_absolute());
    }
}
