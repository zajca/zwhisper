//! Recording lifecycle task.
//!
//! Spawned by [`crate::recorder_service::RecorderInterface::start_recording`]
//! immediately after `Recorder::start` returns Ok. The task owns the
//! `Recorder` for the rest of its life, drives EOS finalisation on a
//! `tokio::task::spawn_blocking` (so the multi-hour wait does not
//! starve a runtime worker), then runs the post-record transcribe
//! step on the runtime itself, then emits the `TranscriptComplete`
//! and terminal `StateChanged` signals.
//!
//! ## Signal ordering (locked by C9 + the Phase 5 test)
//!
//! Success:
//! ```text
//! StateChanged "starting"   (in start_recording)
//! StateChanged "recording"  (in start_recording, before spawn)
//! StateChanged "stopping"   (this task, after blocking await_completion)
//! RecordingComplete         (this task, before slot release — C5)
//! release session slot      (C5: a follow-up StartRecording can now win)
//! TranscriptComplete        (this task, only if profile.transcription.auto)
//! StateChanged "idle"       (this task, terminal — C3)
//! ```
//!
//! Failure (recording-side):
//! ```text
//! StateChanged "starting"   (in start_recording)
//! StateChanged "recording"  (in start_recording)
//! [Recorder::await_completion returns Err]
//! release session slot
//! StateChanged "failed"     (this task, terminal)
//! ```
//!
//! Failure (transcribe-side): we still emit `RecordingComplete` and
//! release the slot, then log the transcribe failure and emit
//! `StateChanged "failed"` instead of `"idle"`. The audio file is
//! preserved on disk for the user to retry.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{error, info, warn};
use zbus::object_server::InterfaceRef;
use zwhisper_core::audio::recorder::Recorder;
use zwhisper_core::audio::state::{SessionId, StopReason};
use zwhisper_core::transcribe::{TranscribeOpts, transcribe_file};

use crate::last_session::{self, LastSession};
use crate::recorder_service::{RecorderInterface, RecorderInterfaceSignals};
use crate::session::SessionManager;

/// Inputs to the lifecycle task. Built by `start_recording` once the
/// recorder is up and the session slot is reserved.
pub(crate) struct LifecycleHooks {
    pub(crate) iface_ref: InterfaceRef<RecorderInterface>,
    pub(crate) sessions: Arc<SessionManager>,
    pub(crate) session_id: SessionId,
    pub(crate) audio_path: PathBuf,
    pub(crate) transcribe_auto: bool,
    pub(crate) transcribe_backend: String,
    pub(crate) transcribe_model: String,
    pub(crate) transcribe_language: String,
    /// M5 — typed routing for cloud backends. Default
    /// [`zwhisper_core::transcribe::BackendConfig::WhisperCpp`] keeps
    /// the legacy whisper-cpp flow intact.
    pub(crate) transcribe_backend_config: zwhisper_core::transcribe::BackendConfig,
}

/// Spawn the lifecycle task. Returns immediately; the spawned task
/// owns the recorder and emits the remaining lifecycle signals.
///
/// The function also installs the `Recorder::stop_handle` on the
/// session manager so `Recorder1.StopRecording` and the daemon's
/// SIGTERM handler can drive the drain.
pub(crate) fn spawn_lifecycle(recorder: Recorder, hooks: LifecycleHooks) {
    // The stop handle is cloneable and writes into the recorder's
    // own watch channel — installing it before the recorder moves
    // into spawn_blocking is safe because the channel exists from
    // `Recorder::start` onwards.
    let stop_handle = recorder.stop_handle();
    hooks.sessions.install_stop_hook(Arc::new(move |reason| {
        stop_handle.request_stop(reason);
    }));

    let sessions = Arc::clone(&hooks.sessions);
    let handle = tokio::spawn(run_lifecycle(recorder, hooks));
    // Register with the SessionManager so shutdown can await the
    // post-release transcribe step. Without this the daemon's
    // shutdown loop sees an empty session slot the moment
    // `release()` runs (C5) and may drop the connection mid-
    // transcribe, killing the lifecycle task before it emits the
    // terminal `StateChanged "idle"`.
    sessions.register_lifecycle(handle);
}

#[allow(clippy::too_many_lines)] // C2 wiring inflated past 100; the function is a single coherent state machine and splitting it obscures the signal-ordering invariant.
async fn run_lifecycle(recorder: Recorder, hooks: LifecycleHooks) {
    let session_id_str = hooks.session_id.to_string();
    info!(session_id = %session_id_str, "lifecycle task running");

    // Wait for an explicit stop request (or a bus-driven stop reason
    // such as DeviceLost / BusError / EosObserved) before invoking
    // `await_completion`. The recorder's `await_completion` always
    // sends EOS as its first step, so calling it before a stop has
    // been requested would terminate the recording immediately.
    // Phase 5 of the M3 milestone surfaced this regression via the
    // `recording_complete_arrives_before_state_changed_idle` test.
    let mut stop_rx = recorder.stop_subscriber();
    loop {
        if !matches!(*stop_rx.borrow_and_update(), StopReason::Running) {
            break;
        }
        if stop_rx.changed().await.is_err() {
            // Sender dropped — recorder went out of scope without a
            // stop reason. Treat as user-requested cancellation so
            // we can still drive a clean drain.
            warn!(
                session_id = %session_id_str,
                "stop watch channel closed without a reason; falling back to UserRequested",
            );
            break;
        }
    }

    // Offload the blocking EOS finalisation onto the blocking pool
    // so the runtime worker is free to dispatch other D-Bus calls
    // (StopRecording, GetStatus) while the recorder drains.
    let blocking = tokio::task::spawn_blocking(move || recorder.await_completion());
    let result = match blocking.await {
        Ok(r) => r,
        Err(join_err) => {
            error!(error = %join_err, "spawn_blocking panicked while draining recorder");
            // Best effort: clean up state so the daemon can accept a
            // new session. We have no `RecordingReport` so we cannot
            // emit `RecordingComplete`; emit `StateChanged "failed"`
            // and bail.
            hooks.sessions.release();
            emit_terminal_state(&hooks.iface_ref, "failed", &session_id_str).await;
            return;
        }
    };

    match result {
        Ok(report) => {
            info!(
                session_id = %session_id_str,
                duration_ms = u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX),
                samples_written = report.samples_written,
                underruns = report.underruns,
                warnings = report.warnings.len(),
                audio_path = %report.audio_path.display(),
                "recording complete (daemon)",
            );

            // C9: emit StateChanged "stopping" before RecordingComplete.
            emit_state_changed(&hooks.iface_ref, "stopping", &session_id_str).await;

            // C2 (M4): persist audio-only state-file BEFORE emitting
            // the signal so a tray that bootstraps inside the
            // signal-delivery window observes the just-completed
            // session.
            persist_last_session_audio_only(&session_id_str, &report.audio_path).await;

            emit_recording_complete(
                &hooks.iface_ref,
                &session_id_str,
                &report.audio_path.display().to_string(),
            )
            .await;

            // C5: release the slot BEFORE awaiting transcribe so a
            // concurrent StartRecording during the transcription
            // window succeeds.
            hooks.sessions.release();

            if hooks.transcribe_auto {
                let opts = TranscribeOpts {
                    backend: hooks.transcribe_backend.clone(),
                    model: hooks.transcribe_model.clone(),
                    language: hooks.transcribe_language.clone(),
                    backend_config: hooks.transcribe_backend_config.clone(),
                };
                match transcribe_file(&report.audio_path, &opts).await {
                    Ok(art) => {
                        let bytes = std::fs::metadata(&art.txt_path).map_or(0, |m| m.len());
                        info!(
                            session_id = %session_id_str,
                            transcript_path = %art.txt_path.display(),
                            bytes,
                            backend = %hooks.transcribe_backend,
                            "transcript complete (daemon)",
                        );

                        // C2 (M4): persist full state-file BEFORE
                        // emitting the signal — same race window as
                        // the audio-only phase above.
                        persist_last_session_with_transcript(
                            &session_id_str,
                            &report.audio_path,
                            &art.txt_path,
                            &hooks.transcribe_backend,
                        )
                        .await;

                        emit_transcript_complete(
                            &hooks.iface_ref,
                            &session_id_str,
                            &art.txt_path.display().to_string(),
                            bytes,
                            &hooks.transcribe_backend,
                        )
                        .await;
                        emit_terminal_state(&hooks.iface_ref, "idle", &session_id_str).await;
                    }
                    Err(e) => {
                        error!(
                            session_id = %session_id_str,
                            error = %e,
                            "transcribe step failed; audio preserved",
                        );
                        // Do NOT emit TranscriptComplete on failure —
                        // the wire contract is "fired iff transcript
                        // exists". Surface the failure as terminal
                        // state.
                        emit_terminal_state(&hooks.iface_ref, "failed", &session_id_str).await;
                    }
                }
            } else {
                // No auto-transcribe: terminal state is plain idle.
                emit_terminal_state(&hooks.iface_ref, "idle", &session_id_str).await;
            }
        }
        Err(rec_err) => {
            error!(session_id = %session_id_str, error = %rec_err, "recording failed");
            // Emit RecordingComplete with the on-disk path even on
            // failure — the user may still want the partial FLAC.
            // The audio path was reserved before recorder start, so
            // the file may or may not exist; emitting the path
            // gives the CLI something to inspect.
            persist_last_session_audio_only(&session_id_str, &hooks.audio_path).await;
            emit_recording_complete(
                &hooks.iface_ref,
                &session_id_str,
                &hooks.audio_path.display().to_string(),
            )
            .await;
            hooks.sessions.release();
            emit_terminal_state(&hooks.iface_ref, "failed", &session_id_str).await;
        }
    }
}

async fn emit_state_changed(
    iface_ref: &InterfaceRef<RecorderInterface>,
    new_state: &str,
    session_id: &str,
) {
    if let Err(e) = iface_ref.state_changed(new_state, session_id).await {
        warn!(error = %e, %new_state, %session_id, "failed to emit StateChanged");
    }
}

/// Emit a TERMINAL `StateChanged` (`"idle"` or `"failed"`) and
/// remove the daemon's `active-session.json` afterwards. Wrapping
/// the two operations in one helper makes it impossible to forget
/// the cleanup at any of the five terminal-emit sites — if a
/// future edit adds a sixth path it must go through this helper to
/// stay consistent. The clear runs on the blocking pool to keep
/// the lifecycle's tokio worker free; failure is logged inside
/// `clear_at` and never surfaces here (a stale file is only
/// consulted by the tray when state is `recording`/`stopping`,
/// which is no longer the case after this emit).
async fn emit_terminal_state(
    iface_ref: &InterfaceRef<RecorderInterface>,
    new_state: &str,
    session_id: &str,
) {
    debug_assert!(
        matches!(new_state, "idle" | "failed"),
        "emit_terminal_state should only be used for terminal states",
    );
    emit_state_changed(iface_ref, new_state, session_id).await;
    if let Err(je) = tokio::task::spawn_blocking(crate::active_session::clear).await {
        warn!(
            error = %je,
            %session_id,
            "spawn_blocking panicked while clearing active-session.json",
        );
    }
}

async fn emit_recording_complete(
    iface_ref: &InterfaceRef<RecorderInterface>,
    session_id: &str,
    audio_path: &str,
) {
    if let Err(e) = iface_ref.recording_complete(session_id, audio_path).await {
        warn!(error = %e, %session_id, "failed to emit RecordingComplete");
    }
}

async fn emit_transcript_complete(
    iface_ref: &InterfaceRef<RecorderInterface>,
    session_id: &str,
    transcript_path: &str,
    bytes: u64,
    backend: &str,
) {
    if let Err(e) = iface_ref
        .transcript_complete(session_id, transcript_path, bytes, backend)
        .await
    {
        warn!(error = %e, %session_id, "failed to emit TranscriptComplete");
    }
}

async fn persist_last_session_audio_only(session_id: &str, audio_path: &Path) {
    let state = LastSession::audio_only(session_id, audio_path);
    persist_last_session(state).await;
}

async fn persist_last_session_with_transcript(
    session_id: &str,
    audio_path: &Path,
    transcript_path: &Path,
    backend: &str,
) {
    let state = LastSession::with_transcript(session_id, audio_path, transcript_path, backend);
    persist_last_session(state).await;
}

/// Run the synchronous file-write on the blocking pool so the
/// runtime worker keeps dispatching D-Bus calls while the kernel
/// flushes the write. The C2 ordering is enforced by `await`-ing
/// here before any signal is emitted.
async fn persist_last_session(state: LastSession) {
    match tokio::task::spawn_blocking(move || last_session::write_atomic(&state)).await {
        Ok(Ok(_path)) => {}
        Ok(Err(e)) => {
            warn!(error = %e, "could not persist last-session.json");
        }
        Err(join_err) => {
            warn!(error = %join_err, "spawn_blocking panicked while writing last-session.json");
        }
    }
}

// Suppress dead-code warning on imports needed only for the
// stop-reason hook (StopReason is referenced via the closure type
// in `spawn_lifecycle`).
#[allow(dead_code)]
const _STOP_REASON_MARKER: Option<StopReason> = None;
