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

mod last_session;
mod lifecycle;
mod profiles_service;
mod recorder_service;
mod session;
mod tracing_init;

use crate::profiles_service::ProfilesInterface;
use crate::recorder_service::RecorderInterface;
use crate::session::SessionManager;

/// Maximum time the daemon waits for the in-flight session's
/// lifecycle task to finish draining after SIGTERM/SIGINT before
/// giving up and exiting anyway. Keeps shutdown responsive even
/// when the recorder is wedged.
const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Maximum time the daemon waits for in-flight `start_recording`
/// calls to finish (so their lifecycle handles get registered)
/// before draining lifecycle tasks. Short on purpose: the
/// synchronous prelude inside `start_recording` only takes a few
/// hundred milliseconds even on slow hardware.
const INFLIGHT_START_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[tokio::main(flavor = "current_thread")]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let _log_guard = tracing_init::init(0);
    info!(version = env!("CARGO_PKG_VERSION"), "zwhisperd starting");

    let sessions = Arc::new(SessionManager::new());
    let active_profile = Arc::new(AsyncMutex::new(String::new()));

    let recorder_iface = RecorderInterface::new(Arc::clone(&sessions), Arc::clone(&active_profile));
    let profiles_iface = ProfilesInterface::new(Arc::clone(&active_profile));

    // zbus 5.15 connection builder pattern (per the
    // `connection::Builder` docs): register both interfaces at the
    // same path, then claim the well-known name. Multiple
    // `serve_at()` calls on the same path stack interfaces — that
    // is the supported form for the multi-interface single-object
    // case we need.
    let connection = zbus::connection::Builder::session()?
        .serve_at(OBJECT_PATH, recorder_iface)?
        .serve_at(OBJECT_PATH, profiles_iface)?
        .name(BUS_NAME)?
        .build()
        .await
        .map_err(|e| eyre!("failed to register on session bus as {BUS_NAME}: {e}"))?;

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

    shutdown(&connection, &sessions).await;

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
async fn shutdown(connection: &zbus::Connection, sessions: &SessionManager) {
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

    // Dropping the connection releases the bus name. zbus does this
    // implicitly when the last `Connection` clone is dropped; calling
    // `release_name` first makes the intent explicit and surfaces
    // any error.
    if let Err(e) = connection.release_name(BUS_NAME).await {
        warn!(error = %e, "failed to release bus name");
    }
}
