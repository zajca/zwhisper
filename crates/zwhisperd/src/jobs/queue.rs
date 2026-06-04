//! The transcription job worker (RFC-daemon-role F1.3).
//!
//! Serialized lane with **configurable** concurrency (default 1 —
//! whisper-cli is heavy). Per-backend parallel lanes are deferred
//! (YAGNI). Each submitted job is tracked in a registry (for `ListJobs`
//! / `Cancel`) and runs on its own task that first acquires a semaphore
//! permit — so "queued" = waiting for a permit, "running" = holding one.
//!
//! Signals (`JobProgress`/`JobCompleted`/`JobFailed`) are emitted from
//! the detached task by acquiring the `Jobs1` `InterfaceRef` from the
//! object server, exactly as `crate::lifecycle` does for `Recorder1`.

// `expect` is used only when re-locking this module's own std `Mutex`es
// (the job registry / running-set). A poisoned mutex means an earlier
// panic in our own code, the daemon is unrecoverable, and propagating
// via `expect` keeps the error type set small — same rationale as
// `crate::session`.
#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{Semaphore, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{error, info, warn};
use zwhisper_core::profile::schema::OutputDest;
use zwhisper_core::transcribe::{
    AudioSource, TranscribeOpts, TranscriptArtifacts, transcribe_file, transcribe_source,
};
use zwhisper_ipc::{OBJECT_PATH, RpcError};

use crate::history::{HistoryHandle, HistoryStatus};
use crate::jobs::{JobId, JobState, SubmitMode, encode_outputs};
// `JobsInterfaceSignals` is the zbus-generated trait that exposes the
// signal emit methods on `InterfaceRef<JobsInterface>` — same pattern as
// `RecorderInterfaceSignals` used by `crate::lifecycle`.
use crate::jobs_service::{JobsInterface, JobsInterfaceSignals};

/// What a job transcribes.
pub(crate) enum JobSource {
    /// A path on disk (`Jobs1.TranscribeFile`). The coordinator decodes
    /// PCM from the artifact as needed.
    File(PathBuf),
    /// A pre-built [`AudioSource`] (post-record auto-transcribe), which
    /// may carry live ASR PCM captured during recording.
    Prepared(Box<AudioSource>),
}

/// Everything needed to run one job.
pub(crate) struct JobSpec {
    pub(crate) session_id: String,
    pub(crate) source: JobSource,
    pub(crate) opts: TranscribeOpts,
    pub(crate) profile: String,
    pub(crate) outputs: Vec<OutputDest>,
    pub(crate) submit_mode: SubmitMode,
    pub(crate) label: String,
    /// Set for `Auto` jobs: the lifecycle task awaits this so it can
    /// emit the FROZEN `Recorder1` terminal signals from the result.
    pub(crate) done: Option<oneshot::Sender<Result<TranscriptArtifacts, String>>>,
}

struct JobRecord {
    session_id: String,
    state: JobState,
    label: String,
    submitted_ms: u64,
    abort: AbortHandle,
}

/// The job queue. Cheap to clone via the inner `Arc`s.
#[derive(Clone)]
pub(crate) struct JobQueue {
    inner: Arc<Inner>,
}

struct Inner {
    /// Filled by `main` immediately after the bus connection is built —
    /// the queue is constructed *before* the connection exists (the
    /// connection builder needs the interfaces first), so the connection
    /// arrives via this cell. Any job only runs after the daemon is up,
    /// by which point the cell is always set.
    conn: Arc<OnceLock<zbus::Connection>>,
    history: HistoryHandle,
    sem: Arc<Semaphore>,
    registry: Mutex<HashMap<JobId, JobRecord>>,
    running: Mutex<Vec<JoinHandle<()>>>,
    accepting: AtomicBool,
}

impl std::fmt::Debug for JobQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobQueue")
            .field("permits", &self.inner.sem.available_permits())
            .field(
                "tracked",
                &self.inner.registry.lock().map_or(0, |r| r.len()),
            )
            .field("accepting", &self.inner.accepting.load(Ordering::SeqCst))
            .finish()
    }
}

impl JobQueue {
    pub(crate) fn new(
        conn: Arc<OnceLock<zbus::Connection>>,
        history: HistoryHandle,
        concurrency: usize,
    ) -> Self {
        let concurrency = concurrency.max(1);
        Self {
            inner: Arc::new(Inner {
                conn,
                history,
                sem: Arc::new(Semaphore::new(concurrency)),
                registry: Mutex::new(HashMap::new()),
                running: Mutex::new(Vec::new()),
                accepting: AtomicBool::new(true),
            }),
        }
    }

    /// Enqueue a job. Returns the minted [`JobId`] or a typed error when
    /// the daemon is shutting down.
    ///
    /// On the daemon's current-thread runtime a freshly spawned task
    /// cannot run until the caller yields, so the synchronous
    /// `spawn` → `abort_handle` → registry-insert sequence below has no
    /// race: the record exists before the task's first poll.
    pub(crate) fn submit(&self, spec: JobSpec) -> Result<JobId, RpcError> {
        if !self.inner.accepting.load(Ordering::SeqCst) {
            // Honour the `done` channel so an Auto submitter is not left
            // waiting forever.
            if let Some(done) = spec.done {
                let _ = done.send(Err("daemon is shutting down".to_owned()));
            }
            return Err(RpcError::Transient {
                reason: "daemon is shutting down; not accepting new jobs".to_owned(),
            });
        }

        let job_id = JobId::new();
        let submitted_ms = crate::history::now_unix_ms();
        let session_id = spec.session_id.clone();
        let label = spec.label.clone();

        let queue = self.clone();
        let join = tokio::spawn(async move { run_job(queue, job_id, spec).await });
        let abort = join.abort_handle();

        {
            let mut reg = self.inner.registry.lock().expect("job registry poisoned");
            reg.insert(
                job_id,
                JobRecord {
                    session_id,
                    state: JobState::Queued,
                    label,
                    submitted_ms,
                    abort,
                },
            );
        }
        {
            let mut running = self.inner.running.lock().expect("running set poisoned");
            running.retain(|h| !h.is_finished());
            running.push(join);
        }
        Ok(job_id)
    }

    /// Submit a post-record auto-transcribe job and return the channel
    /// the lifecycle task awaits. On the result the lifecycle emits the
    /// FROZEN `Recorder1` terminal signals; the queue independently
    /// emits the new `Jobs1` signals and records history. If the daemon
    /// is shutting down, the channel resolves to `Err`.
    pub(crate) fn submit_auto(
        &self,
        session_id: String,
        source: AudioSource,
        opts: TranscribeOpts,
        profile: String,
        outputs: Vec<OutputDest>,
    ) -> oneshot::Receiver<Result<TranscriptArtifacts, String>> {
        let (tx, rx) = oneshot::channel();
        let label = format!("auto:{session_id}");
        let spec = JobSpec {
            session_id,
            source: JobSource::Prepared(Box::new(source)),
            opts,
            profile,
            outputs,
            submit_mode: SubmitMode::Auto,
            label,
            done: Some(tx),
        };
        // `submit` forwards a shutdown refusal onto the `done` channel,
        // so the receiver always resolves.
        let _ = self.submit(spec);
        rx
    }

    /// Best-effort cancel of a queued or running job. Aborting the task
    /// drops its future; for a running whisper-cli the `kill_on_drop`
    /// armed in `SystemRunner` tears the subprocess down (F1.3).
    pub(crate) fn cancel(&self, job_id: &JobId) -> Result<(), RpcError> {
        let mut reg = self.inner.registry.lock().expect("job registry poisoned");
        match reg.get_mut(job_id) {
            Some(rec) => {
                rec.abort.abort();
                rec.state = JobState::Cancelled;
                let session_id = rec.session_id.clone();
                drop(reg);
                // Reflect the cancellation in history (best-effort).
                let history = self.inner.history.clone();
                tokio::spawn(async move {
                    history
                        .set_status(
                            &session_id,
                            HistoryStatus::Failed,
                            Some("cancelled by user".to_owned()),
                        )
                        .await;
                });
                Ok(())
            }
            None => Err(RpcError::JobUnknown {
                id: job_id.to_string(),
            }),
        }
    }

    /// Snapshot of the active queue (`ListJobs`).
    pub(crate) fn list(&self) -> Vec<zwhisper_ipc::JobInfo> {
        let reg = self.inner.registry.lock().expect("job registry poisoned");
        let mut jobs: Vec<zwhisper_ipc::JobInfo> = reg
            .iter()
            .map(|(id, rec)| zwhisper_ipc::JobInfo {
                job_id: id.to_string(),
                state: rec.state.as_wire().to_owned(),
                label: rec.label.clone(),
                submitted_ms: rec.submitted_ms,
            })
            .collect();
        jobs.sort_by_key(|j| std::cmp::Reverse(j.submitted_ms));
        jobs
    }

    /// Stop accepting new jobs and await the in-flight ones, bounded by
    /// `timeout`. Called from daemon shutdown after the lifecycle drain.
    pub(crate) async fn shutdown(&self, timeout: std::time::Duration) {
        self.inner.accepting.store(false, Ordering::SeqCst);
        let pending: Vec<JoinHandle<()>> = {
            let mut running = self.inner.running.lock().expect("running set poisoned");
            std::mem::take(&mut *running)
        };
        if pending.is_empty() {
            return;
        }
        info!(
            jobs = pending.len(),
            "awaiting in-flight transcription jobs"
        );
        let drain = async {
            for h in pending {
                let _ = h.await;
            }
        };
        if tokio::time::timeout(timeout, drain).await.is_err() {
            warn!("transcription jobs did not drain within {timeout:?}; exiting anyway");
        }
    }

    fn set_state(&self, job_id: JobId, state: JobState) {
        if let Ok(mut reg) = self.inner.registry.lock() {
            if let Some(rec) = reg.get_mut(&job_id) {
                rec.state = state;
            }
        }
    }

    fn finish(&self, job_id: JobId) {
        if let Ok(mut reg) = self.inner.registry.lock() {
            reg.remove(&job_id);
        }
    }

    async fn jobs_iface(&self) -> Option<zbus::object_server::InterfaceRef<JobsInterface>> {
        let Some(conn) = self.inner.conn.get() else {
            warn!("Jobs1 connection not yet initialised; dropping signal");
            return None;
        };
        match conn.object_server().interface(OBJECT_PATH).await {
            Ok(r) => Some(r),
            Err(e) => {
                warn!(error = %e, "could not acquire Jobs1 interface ref for signal emission");
                None
            }
        }
    }

    async fn emit_progress(&self, job_id: &JobId, state: &str) {
        if let Some(iface) = self.jobs_iface().await {
            if let Err(e) = iface.job_progress(&job_id.to_string(), state).await {
                warn!(error = %e, "failed to emit JobProgress");
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // Mirrors the JobCompleted signal arity.
    async fn emit_completed(
        &self,
        job_id: &JobId,
        submit_mode: SubmitMode,
        profile: &str,
        outputs: &[OutputDest],
        transcript_path: &str,
        bytes: u64,
        backend: &str,
    ) {
        if let Some(iface) = self.jobs_iface().await {
            if let Err(e) = iface
                .job_completed(
                    &job_id.to_string(),
                    submit_mode.as_wire(),
                    profile,
                    encode_outputs(outputs),
                    transcript_path,
                    bytes,
                    backend,
                )
                .await
            {
                warn!(error = %e, "failed to emit JobCompleted");
            }
        }
    }

    async fn emit_failed(&self, job_id: &JobId, error: &str) {
        if let Some(iface) = self.jobs_iface().await {
            if let Err(e) = iface.job_failed(&job_id.to_string(), error).await {
                warn!(error = %e, "failed to emit JobFailed");
            }
        }
    }
}

/// The per-job task body.
async fn run_job(queue: JobQueue, job_id: JobId, spec: JobSpec) {
    // Queued until a permit frees (serialized lane, default 1).
    let Ok(permit) = queue.inner.sem.clone().acquire_owned().await else {
        // Semaphore closed — daemon tearing down.
        if let Some(done) = spec.done {
            let _ = done.send(Err("queue closed before job started".to_owned()));
        }
        queue.finish(job_id);
        return;
    };

    queue.set_state(job_id, JobState::Running);
    queue.emit_progress(&job_id, "running").await;
    queue
        .inner
        .history
        .set_status(&spec.session_id, HistoryStatus::Transcribing, None)
        .await;

    let backend_label = spec.opts.backend.as_str().to_owned();
    let result = match &spec.source {
        JobSource::File(path) => transcribe_file(path, &spec.opts).await,
        JobSource::Prepared(src) => transcribe_source(src, &spec.opts).await,
    };
    drop(permit);

    match result {
        Ok(art) => {
            let bytes = std::fs::metadata(&art.txt_path).map_or(0, |m| m.len());
            let transcript_path = art.txt_path.display().to_string();
            info!(
                job_id = %job_id,
                session_id = %spec.session_id,
                transcript_path = %transcript_path,
                bytes,
                backend = %backend_label,
                "job complete",
            );
            queue
                .inner
                .history
                .set_transcript(
                    &spec.session_id,
                    vec![
                        art.txt_path.display().to_string(),
                        art.json_path.display().to_string(),
                    ],
                    &backend_label,
                )
                .await;
            queue
                .emit_completed(
                    &job_id,
                    spec.submit_mode,
                    &spec.profile,
                    &spec.outputs,
                    &transcript_path,
                    bytes,
                    &backend_label,
                )
                .await;
            queue.set_state(job_id, JobState::Done);
            queue.emit_progress(&job_id, "done").await;
            if let Some(done) = spec.done {
                let _ = done.send(Ok(art));
            }
        }
        Err(e) => {
            let msg = e.to_string();
            error!(job_id = %job_id, session_id = %spec.session_id, error = %msg, "job failed");
            queue
                .inner
                .history
                .set_status(&spec.session_id, HistoryStatus::Failed, Some(msg.clone()))
                .await;
            queue.emit_failed(&job_id, &msg).await;
            queue.set_state(job_id, JobState::Failed);
            queue.emit_progress(&job_id, "failed").await;
            if let Some(done) = spec.done {
                let _ = done.send(Err(msg));
            }
        }
    }
    queue.finish(job_id);
}
