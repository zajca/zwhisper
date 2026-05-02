//! Task B — D-Bus signal pump.
//!
//! The pump is the only D-Bus client in the tray. It owns the
//! `Recorder1Proxy`, `Profiles1Proxy`, and the FDO `DBusProxy`
//! (used to track when the daemon comes and goes), and is the
//! single writer of the shared `TrayState` watch channel.
//!
//! ## Threading model (M4-plan § "Architecture for M4")
//!
//! Single-threaded tokio (`current_thread`) runtime. The pump is a
//! task; the renderer (P3) is another task; both communicate over
//! a `tokio::sync::watch::Sender<TrayState>`.
//!
//! ## Subscribe-then-snapshot ordering (M4-plan C2 + § "Late-start")
//!
//! 1. Subscribe to all signal streams FIRST.
//! 2. THEN call `GetStatus` / `List` / `GetActive` and read
//!    `last-session.json` to bootstrap.
//!
//! That ordering means a signal that fires between snapshot RPCs
//! does NOT get lost — the corresponding stream still holds the
//! pending message in its tokio MPSC buffer.
//!
//! ## Reconnect behaviour (M4-plan § "Daemon-offline transition")
//!
//! - On `NameOwnerChanged{ new_owner: "" }` → flip icon to
//!   `DaemonOffline`, drop everything, await `new_owner != ""`,
//!   reconnect from scratch.
//! - On any zbus error inside the multiplex → log warn, exit inner
//!   loop, sleep with backoff (250ms → 500ms → 1s → 2s → 5s cap),
//!   reconnect.
//! - Backoff resets after a successful inner loop iteration.

use std::path::PathBuf;
use std::time::Duration;

use color_eyre::eyre::Result;
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use zwhisper_ipc::{BUS_NAME, Profiles1Proxy, Recorder1Proxy};

use crate::config::{BACKOFF_SCHEDULE_MS, PROFILE_REFRESH_PERIOD};
use crate::dbus::{connect_session, read_active_session, read_last_session};
use crate::sink::dispatch::TranscriptJob;
use crate::state::{
    TrayState, apply_recording_complete, apply_state_changed, apply_transcript_complete,
};

/// Run the signal pump until `shutdown` is signalled.
///
/// On bus failures the pump backs off and reconnects forever.
/// `shutdown` is the only sentinel that ends the loop.
pub async fn run_pump(
    state_tx: watch::Sender<TrayState>,
    sink_tx: mpsc::Sender<TranscriptJob>,
    mut shutdown: watch::Receiver<()>,
) -> Result<()> {
    let mut backoff_idx: usize = 0;

    loop {
        match run_inner(&state_tx, &sink_tx, &mut shutdown).await {
            ConnectionExit::Shutdown => {
                info!("pump shutting down");
                return Ok(());
            }
            ConnectionExit::Reconnect(reason) => {
                let delay = backoff_for(backoff_idx);
                warn!(
                    error = %reason,
                    backoff_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    "tray pump disconnected — reconnecting after backoff",
                );
                state_tx.send_modify(|s| {
                    s.icon = crate::state::IconState::DaemonOffline;
                });
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    _ = shutdown.changed() => {
                        info!("pump shutdown signalled during backoff");
                        return Ok(());
                    }
                }
                backoff_idx = backoff_idx
                    .saturating_add(1)
                    .min(BACKOFF_SCHEDULE_MS.len() - 1);
            }
        }
    }
}

fn backoff_for(idx: usize) -> Duration {
    // `unwrap_or` is fine — the const schedule is non-empty by
    // construction, but the explicit fallback documents intent.
    let ms = BACKOFF_SCHEDULE_MS.get(idx).copied().unwrap_or(5000);
    Duration::from_millis(ms)
}

enum ConnectionExit {
    /// Shutdown channel fired — exit the outer loop.
    Shutdown,
    /// Disconnected for some reason — outer loop should backoff and
    /// retry.
    Reconnect(String),
}

// One inner D-Bus session, top to bottom: connect, build proxies,
// subscribe to all signal streams, snapshot, then multiplex until
// disconnect. Splitting this into several functions would force us
// to type-erase the four heterogeneous signal stream types and the
// proxies through trait objects, which buys nothing and obscures
// the ordering invariants documented in the module-level docs.
#[allow(clippy::too_many_lines)]
async fn run_inner(
    state_tx: &watch::Sender<TrayState>,
    sink_tx: &mpsc::Sender<TranscriptJob>,
    shutdown: &mut watch::Receiver<()>,
) -> ConnectionExit {
    // 1. Connect to the session bus.
    let conn = match connect_session().await {
        Ok(c) => c,
        Err(e) => return ConnectionExit::Reconnect(format!("connect_session: {e}")),
    };
    info!("connected to session bus");

    // 2. Build the proxies. `default_service` / `default_path` on
    //    the proxy declarations in `zwhisper-ipc` already resolve
    //    to `BUS_NAME` / `OBJECT_PATH`, so `new(&conn)` is the
    //    documented short form. The constants are still imported
    //    because the `NameOwnerChanged` match rule below filters by
    //    `BUS_NAME`.
    let recorder = match Recorder1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => return ConnectionExit::Reconnect(format!("Recorder1Proxy::new: {e}")),
    };
    let profiles_proxy = match Profiles1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => return ConnectionExit::Reconnect(format!("Profiles1Proxy::new: {e}")),
    };

    // 3. Subscribe FIRST to all four streams. The order matters
    //    only insofar as a signal that fires before its stream is
    //    open is lost; subscribing before the snapshot RPC ensures
    //    no transition is dropped.
    let mut state_changed = match recorder.receive_state_changed().await {
        Ok(s) => s,
        Err(e) => return ConnectionExit::Reconnect(format!("subscribe StateChanged: {e}")),
    };
    let mut recording_complete = match recorder.receive_recording_complete().await {
        Ok(s) => s,
        Err(e) => {
            return ConnectionExit::Reconnect(format!("subscribe RecordingComplete: {e}"));
        }
    };
    let mut transcript_complete = match recorder.receive_transcript_complete().await {
        Ok(s) => s,
        Err(e) => {
            return ConnectionExit::Reconnect(format!("subscribe TranscriptComplete: {e}"));
        }
    };

    let dbus_proxy = match zbus::fdo::DBusProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => return ConnectionExit::Reconnect(format!("DBusProxy::new: {e}")),
    };
    let mut owner_changed = match dbus_proxy
        .receive_name_owner_changed_with_args(&[(0_u8, BUS_NAME)])
        .await
    {
        Ok(s) => s,
        Err(e) => return ConnectionExit::Reconnect(format!("subscribe NameOwnerChanged: {e}")),
    };

    // 4. Snapshot. Errors during the snapshot mean the daemon is up
    //    on the bus but is not actually serving the interface — log
    //    and reconnect.
    if let Err(e) = snapshot(&recorder, &profiles_proxy, state_tx).await {
        return ConnectionExit::Reconnect(format!("snapshot: {e}"));
    }

    let mut profile_tick = tokio::time::interval(PROFILE_REFRESH_PERIOD);
    profile_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick fires immediately — consume it; the snapshot
    // already populated the profile list.
    profile_tick.tick().await;

    loop {
        tokio::select! {
            biased;

            _ = shutdown.changed() => {
                return ConnectionExit::Shutdown;
            }

            maybe_owner = owner_changed.next() => {
                match maybe_owner {
                    Some(sig) => {
                        let args = match sig.args() {
                            Ok(a) => a,
                            Err(e) => {
                                return ConnectionExit::Reconnect(
                                    format!("NameOwnerChanged args: {e}"),
                                );
                            }
                        };
                        let new_owner_empty = args
                            .new_owner()
                            .as_ref()
                            .is_none_or(|n| n.as_str().is_empty());
                        if new_owner_empty {
                            warn!("daemon bus name lost — reconnecting");
                            state_tx.send_modify(|s| {
                                s.icon = crate::state::IconState::DaemonOffline;
                                s.active_session_id = None;
                                s.recording_started_at = None;
                            });
                            return ConnectionExit::Reconnect(
                                "NameOwnerChanged: daemon left bus".to_owned(),
                            );
                        }
                        // Owner came back without us going through
                        // the disconnect branch — usually a
                        // restart that we caught straddling. Rebuild
                        // proxies cleanly.
                        debug!("NameOwnerChanged with new owner — refreshing");
                        return ConnectionExit::Reconnect(
                            "NameOwnerChanged: new owner, refresh".to_owned(),
                        );
                    }
                    None => {
                        return ConnectionExit::Reconnect(
                            "NameOwnerChanged stream closed".to_owned(),
                        );
                    }
                }
            }

            maybe_msg = state_changed.next() => {
                match maybe_msg {
                    Some(sig) => {
                        let args = match sig.args() {
                            Ok(a) => a,
                            Err(e) => {
                                warn!(error = %e, "decode StateChanged args");
                                continue;
                            }
                        };
                        state_tx.send_modify(|s| {
                            apply_state_changed(s, args.new_state(), args.session_id());
                        });
                    }
                    None => {
                        return ConnectionExit::Reconnect(
                            "StateChanged stream closed".to_owned(),
                        );
                    }
                }
            }

            maybe_msg = recording_complete.next() => {
                match maybe_msg {
                    Some(sig) => {
                        let args = match sig.args() {
                            Ok(a) => a,
                            Err(e) => {
                                warn!(error = %e, "decode RecordingComplete args");
                                continue;
                            }
                        };
                        let now = unix_ms_now();
                        state_tx.send_modify(|s| {
                            apply_recording_complete(
                                s,
                                args.session_id(),
                                args.audio_path(),
                                now,
                            );
                        });
                    }
                    None => {
                        return ConnectionExit::Reconnect(
                            "RecordingComplete stream closed".to_owned(),
                        );
                    }
                }
            }

            maybe_msg = transcript_complete.next() => {
                match maybe_msg {
                    Some(sig) => {
                        let args = match sig.args() {
                            Ok(a) => a,
                            Err(e) => {
                                warn!(error = %e, "decode TranscriptComplete args");
                                continue;
                            }
                        };
                        let now = unix_ms_now();
                        // Dispatch the sink job FIRST so the
                        // clipboard / notification fire even if a
                        // later writer of `state_tx` panics. The
                        // mpsc is bounded; on overflow we log and
                        // drop — the M4 sinks are best-effort, never
                        // a state-machine source-of-truth.
                        let session_id_str = (*args.session_id()).to_owned();
                        let transcript_path_str = (*args.transcript_path()).to_owned();
                        let backend_str = (*args.backend()).to_owned();

                        // Authoritative `audio_path` lookup: the C2
                        // invariant guarantees the daemon wrote
                        // `last-session.json` (with a matching
                        // `session_id`) BEFORE this signal fired. We
                        // re-read the file rather than trust the
                        // in-memory cache because a missed
                        // RecordingComplete (e.g. dropped during a
                        // reconnect/resubscribe window) leaves the
                        // cache stale or empty — the file is the
                        // single source of truth (IDEA.md § 5).
                        let audio_path_from_file = read_last_session()
                            .await
                            .filter(|s| s.session_id == session_id_str)
                            .map(|s| s.audio_path);

                        let job = TranscriptJob {
                            session_id: session_id_str.clone(),
                            transcript_path: PathBuf::from(transcript_path_str.as_str()),
                            bytes: *args.bytes(),
                            backend: backend_str.clone(),
                        };
                        if let Err(e) = sink_tx.try_send(job) {
                            warn!(
                                error = %e,
                                "sink_tx full or closed; dropping transcript sink job",
                            );
                        }
                        state_tx.send_modify(|s| {
                            apply_transcript_complete(
                                s,
                                &session_id_str,
                                audio_path_from_file,
                                &transcript_path_str,
                                &backend_str,
                                now,
                            );
                        });
                    }
                    None => {
                        return ConnectionExit::Reconnect(
                            "TranscriptComplete stream closed".to_owned(),
                        );
                    }
                }
            }

            _ = profile_tick.tick() => {
                if let Err(e) = refresh_profiles(&profiles_proxy, state_tx).await {
                    warn!(error = %e, "profile refresh tick failed");
                    return ConnectionExit::Reconnect(format!("profile tick: {e}"));
                }
            }
        }
    }
}

async fn snapshot(
    recorder: &Recorder1Proxy<'_>,
    profiles_proxy: &Profiles1Proxy<'_>,
    state_tx: &watch::Sender<TrayState>,
) -> zbus::Result<()> {
    let status = recorder.get_status().await?;
    let profiles = profiles_proxy.list().await?;
    let active = profiles_proxy.get_active().await?;
    let last_session = read_last_session().await;

    // Recover `active_session_id` for the in-flight session when we
    // bootstrap mid-recording (post-2026-05-02 review fix). The
    // wire-format `Status` does not carry the session id, so we
    // consult the daemon's `active-session.json` written before
    // every `StateChanged "recording"` (same C2 ordering pattern as
    // last-session.json). Without this, `Stop recording` from a
    // freshly-restarted tray would reach the dispatcher with no
    // session id and the daemon would correctly reject it.
    let icon = crate::state::IconState::from_wire(&status.state);
    let active_session = if matches!(
        icon,
        crate::state::IconState::Recording | crate::state::IconState::Stopping
    ) {
        read_active_session().await
    } else {
        None
    };

    state_tx.send_modify(|s| {
        s.icon = icon;
        s.active_profile = active;
        s.profiles = profiles;
        if matches!(icon, crate::state::IconState::Recording) {
            // We don't know the original start time; approximate as
            // (now - duration_ms). The active-session.json carries
            // an absolute `started_at_unix_ms` but the tray timer
            // ticks against `Instant`, which is monotonic — so we
            // keep the duration-based approximation as the cleanest
            // mapping.
            let approx = std::time::Instant::now()
                .checked_sub(Duration::from_millis(status.duration_ms))
                .unwrap_or_else(std::time::Instant::now);
            s.recording_started_at = Some(approx);
        }
        if let Some(active) = active_session {
            // Trust the file unconditionally when the daemon says
            // we are mid-recording. If the file's session_id is
            // empty (parser would have rejected it) we get None
            // here and the cached value (typically also None) wins.
            s.active_session_id = Some(active.session_id);
        } else if !matches!(
            icon,
            crate::state::IconState::Recording | crate::state::IconState::Stopping
        ) {
            // Daemon is idle/failed/etc — clear any stale id so
            // a future race cannot stop a session that no longer
            // exists.
            s.active_session_id = None;
        }
        if let Some(ls) = last_session {
            s.last_session = Some(ls);
        }
    });

    Ok(())
}

async fn refresh_profiles(
    profiles_proxy: &Profiles1Proxy<'_>,
    state_tx: &watch::Sender<TrayState>,
) -> zbus::Result<()> {
    let profiles = profiles_proxy.list().await?;
    let active = profiles_proxy.get_active().await?;
    state_tx.send_modify(|s| {
        s.profiles = profiles;
        s.active_profile = active;
    });
    Ok(())
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
