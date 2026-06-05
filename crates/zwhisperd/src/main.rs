//! `zwhisperd` — zwhisper recording daemon.
//!
//! Binary entry point for the M3 split: tokio current-thread
//! runtime, zbus connection registered as `cz.zajca.Zwhisper1`,
//! `Recorder1` + `Profiles1` interfaces hosted at
//! `/cz/zajca/Zwhisper1`, lifecycle task driving `GStreamer` through
//! `tokio::task::spawn_blocking`, signal emission for the three M3
//! signals, clean shutdown on `SIGINT`/`SIGTERM`.
//!
//! The daemon does **not** initialise `GStreamer` at startup
//! (correction C7); the first `StartRecording` call performs the
//! init lazily so a missing `libgstreamer-1.0` does not prevent the
//! bus name from being claimed.

use std::sync::Arc;

use color_eyre::eyre::eyre;
use futures_util::StreamExt;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook_tokio::Signals;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{error, info, warn};
use zwhisper_core::audio::state::StopReason;
use zwhisper_ipc::{BUS_NAME, OBJECT_PATH};

mod active_profile;
mod active_session;
mod config;
mod history;
mod history_service;
mod jobs;
mod jobs_service;
mod last_session;
mod lifecycle;
mod orphan_recovery;
mod profiles_service;
mod recorder_service;
mod session;
mod tracing_init;

use std::sync::OnceLock;

use crate::config::{INFLIGHT_START_DRAIN_TIMEOUT, SHUTDOWN_DRAIN_TIMEOUT};

use crate::history_service::HistoryInterface;
use crate::jobs::JobQueue;
use crate::jobs_service::JobsInterface;
use crate::profiles_service::ProfilesInterface;
use crate::recorder_service::RecorderInterface;
use crate::session::SessionManager;

#[tokio::main(flavor = "current_thread")]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let _log_guard = tracing_init::init(0);
    info!(version = env!("CARGO_PKG_VERSION"), "zwhisperd starting");

    let sessions = Arc::new(SessionManager::new());
    let active_profile = Arc::new(AsyncMutex::new(active_profile::load().unwrap_or_default()));

    // RFC-daemon-role: the single durable history writer (F2.2) and the
    // sibling transcription job queue (F1.3). The queue needs the bus
    // connection to emit `Jobs1` signals, but the connection is built
    // *after* the interfaces are registered — so it is delivered to the
    // queue via a `OnceLock` set immediately after `build()`.
    let conn_cell: Arc<OnceLock<zbus::Connection>> = Arc::new(OnceLock::new());
    let (history_handle, _history_task) = history::spawn_writer();
    let queue = JobQueue::new(
        Arc::clone(&conn_cell),
        history_handle.clone(),
        config::job_concurrency(),
    );

    let recorder_iface = RecorderInterface::new(
        Arc::clone(&sessions),
        Arc::clone(&active_profile),
        queue.clone(),
        history_handle.clone(),
    );
    let profiles_iface = ProfilesInterface::new(Arc::clone(&active_profile));
    let jobs_iface = JobsInterface::new(queue.clone(), history_handle.clone());
    let history_iface = HistoryInterface::new(history_handle.clone());

    // zbus 5.15 connection builder pattern (per the
    // `connection::Builder` docs): register every interface at the
    // same path, then claim the well-known name. Multiple
    // `serve_at()` calls on the same path stack interfaces — that
    // is the supported form for the multi-interface single-object
    // case we need. `Recorder1`/`Profiles1` stay frozen; `Jobs1`/
    // `History1` are the new RFC-daemon-role surface.
    let connection = zbus::connection::Builder::session()?
        .serve_at(OBJECT_PATH, recorder_iface)?
        .serve_at(OBJECT_PATH, profiles_iface)?
        .serve_at(OBJECT_PATH, jobs_iface)?
        .serve_at(OBJECT_PATH, history_iface)?
        .name(BUS_NAME)?
        .build()
        .await
        .map_err(|e| eyre!("failed to register on session bus as {BUS_NAME}: {e}"))?;

    // Hand the queue the live connection so its detached job tasks can
    // emit `Jobs1` signals. Set exactly once; any job only runs after
    // this point.
    if conn_cell.set(connection.clone()).is_err() {
        warn!("connection cell was already set; Jobs1 signal emission may be impaired");
    }

    // Reap a recording orphaned by a previous daemon exit. Any
    // active-session.json present now is definitionally stale (an
    // in-flight recording cannot survive a restart): preserve its audio,
    // enqueue a recovery transcription, and clear the state. Runs after
    // the connection is live so the enqueued job can emit Jobs1 signals;
    // best-effort, never aborts startup.
    orphan_recovery::recover(&queue).await;

    info!(
        bus_name = BUS_NAME,
        object_path = OBJECT_PATH,
        "daemon ready"
    );

    // Install POSIX signal handlers via signal-hook-tokio. We do NOT
    // call `tokio::signal::ctrl_c()` anywhere in the daemon — POSIX
    // allows only one handler per signal per process and the
    // recorder library used to install one (M3 stress-test C2). The
    // recorder side now defers signal policy to the caller, and the
    // caller (this main) routes both SIGINT and SIGTERM through the
    // same stream.
    let mut signals = Signals::new([SIGINT, SIGTERM])
        .map_err(|e| eyre!("failed to install signal handlers: {e}"))?;

    // Wait for the first signal. signal-hook-tokio's stream is the
    // canonical source so we get accurate signal numbers for log
    // messages.
    if let Some(sig) = signals.next().await {
        info!(signal = sig, "received shutdown signal");
    } else {
        warn!("signal stream closed without delivering a signal");
    }

    shutdown(&connection, &sessions, &queue).await;

    info!("zwhisperd exiting");
    Ok(())
}

/// Drive a clean shutdown: stop any in-flight recording, await every
/// lifecycle task (including its post-release transcribe step), then
/// drop the connection so the bus name is released.
///
/// Awaiting `lifecycle_tasks` rather than just polling
/// `sessions.snapshot()` is the fix for a regression spotted after
/// initial M3 ship: per C5 the lifecycle task releases the session
/// slot **before** running auto-transcribe, so a snapshot-only loop
/// would treat the daemon as drained the moment recording stops and
/// drop the connection while transcribe is still awaiting
/// `whisper-cli`. The CLI would then never see `TranscriptComplete`
/// or terminal `StateChanged "idle"` and would hang forever.
async fn shutdown(connection: &zbus::Connection, sessions: &SessionManager, queue: &JobQueue) {
    // Wait for any in-flight `start_recording` to finish so its
    // lifecycle handle is registered before we drain. Without this
    // a SIGTERM landing in the brief await-heavy window between
    // `try_reserve` and `spawn_lifecycle` would skip the lifecycle
    // entirely (the JoinHandle has not been pushed yet) and the
    // daemon would tear down a freshly-started recorder without
    // emitting a terminal signal.
    let inflight_deadline = tokio::time::Instant::now() + INFLIGHT_START_DRAIN_TIMEOUT;
    while sessions.inflight_starts() > 0 {
        if tokio::time::Instant::now() >= inflight_deadline {
            warn!(
                inflight = sessions.inflight_starts(),
                "in-flight StartRecording calls did not finish within {:?}; \
                 lifecycle handles may not be registered",
                INFLIGHT_START_DRAIN_TIMEOUT,
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    if sessions.snapshot().is_some() {
        info!("active session detected on shutdown; requesting drain");
        if !sessions.request_stop_active(StopReason::UserRequested) {
            warn!("active session has no stop hook installed; cannot signal drain");
        }
    }

    let pending = sessions.take_lifecycle_tasks();
    if !pending.is_empty() {
        info!(
            tasks = pending.len(),
            "awaiting in-flight lifecycle tasks (recording finalisation + transcribe)"
        );
        let drain = async {
            for handle in pending {
                if let Err(e) = handle.await {
                    warn!(error = %e, "lifecycle task join failed");
                }
            }
        };
        if tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, drain)
            .await
            .is_ok()
        {
            info!("lifecycle tasks drained cleanly");
        } else {
            error!(
                "lifecycle tasks did not drain within {:?}; exiting anyway",
                SHUTDOWN_DRAIN_TIMEOUT
            );
        }
    }

    // RFC-daemon-role F1.3: stop accepting new transcription jobs and
    // await any standalone (`Jobs1.TranscribeFile`) jobs still running.
    // Auto-transcribe jobs are already covered by the lifecycle drain
    // above (the lifecycle task awaits the job result), so this mainly
    // catches detached jobs. The kill_on_drop armed in the backend means
    // anything still running past the timeout is torn down on exit.
    queue.shutdown(SHUTDOWN_DRAIN_TIMEOUT).await;

    // Dropping the connection releases the bus name. zbus does this
    // implicitly when the last `Connection` clone is dropped; calling
    // `release_name` first makes the intent explicit and surfaces
    // any error.
    if let Err(e) = connection.release_name(BUS_NAME).await {
        warn!(error = %e, "failed to release bus name");
    }
}
