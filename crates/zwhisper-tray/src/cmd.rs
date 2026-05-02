//! Task D — RPC dispatcher for menu commands.
//!
//! Owns one `Recorder1Proxy` and one `Profiles1Proxy` (each tied to a
//! single `zbus::Connection`) and consumes [`PendingCmd`]s emitted by
//! the menu callbacks in [`crate::tray`].
//!
//! ## Design
//!
//! - **Optimistic action lock (`DoD` #21).** Before firing an RPC that
//!   mutates daemon state, the dispatcher writes
//!   `state.pending_cmd = Some(...)`. The pump's reducer
//!   ([`crate::state::apply_state_changed`]) clears it once the
//!   matching `StateChanged` arrives. On RPC failure the dispatcher
//!   clears `pending_cmd` itself (the daemon never observed the
//!   request).
//! - **`xdg-open` is fire-and-forget.** Opening the last recording or
//!   transcript spawns a child process; we await the status in a
//!   detached task so the dispatcher loop stays responsive.
//! - **No panics.** The workspace clippy gate denies `unwrap_used` /
//!   `expect_used` / `panic`. Errors propagate via `?` from the
//!   public entry point and are logged in-line on the per-command
//!   path so a single bad RPC does not tear down the dispatcher.
//!
//! See `docs/M4-plan.md` § "Threading model" for the full task
//! taxonomy.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use color_eyre::eyre::Result;
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use zwhisper_ipc::{Profiles1Proxy, Recorder1Proxy};

use crate::state::{LastCompleted, PendingCmd, TrayState};

/// Run the dispatcher loop. Returns when `shutdown_rx` fires or when
/// `cmd_rx` is closed.
///
/// `state_tx` is the same `watch::Sender` the pump owns — the
/// dispatcher only writes the `pending_cmd` slot (and, on a
/// successful `SetActiveProfile`, the `profiles` / `active_profile`
/// slots). All other fields are reducer-driven from D-Bus signals.
pub async fn run_dispatcher(
    conn: zbus::Connection,
    mut cmd_rx: mpsc::Receiver<PendingCmd>,
    state_tx: watch::Sender<TrayState>,
    state_rx: watch::Receiver<TrayState>,
    mut shutdown_rx: watch::Receiver<()>,
) -> Result<()> {
    let recorder = Recorder1Proxy::new(&conn).await?;
    let profiles = Profiles1Proxy::new(&conn).await?;

    info!("dispatcher: ready, awaiting menu commands");

    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                info!("dispatcher: shutdown signal received");
                return Ok(());
            }
            maybe_cmd = cmd_rx.recv() => {
                let Some(cmd) = maybe_cmd else {
                    info!("dispatcher: cmd channel closed");
                    return Ok(());
                };
                handle_cmd(cmd, &recorder, &profiles, &state_tx, &state_rx).await;
            }
        }
    }
}

/// Dispatch a single `PendingCmd`. Logs and clears the action lock
/// on every error path so a single failure never wedges the menu.
async fn handle_cmd(
    cmd: PendingCmd,
    recorder: &Recorder1Proxy<'_>,
    profiles: &Profiles1Proxy<'_>,
    state_tx: &watch::Sender<TrayState>,
    state_rx: &watch::Receiver<TrayState>,
) {
    match cmd {
        PendingCmd::Start => dispatch_start(recorder, state_tx, state_rx).await,
        PendingCmd::Stop => dispatch_stop(recorder, state_tx, state_rx).await,
        PendingCmd::SetActiveProfile { name } => {
            dispatch_set_active_profile(profiles, state_tx, &name).await;
        }
        PendingCmd::OpenLastRecording => {
            let path = open_target_for_recording(state_rx.borrow().last_session.as_ref());
            spawn_xdg_open_or_log(path.as_deref(), "OpenLastRecording");
        }
        PendingCmd::OpenLastTranscript => {
            let path = open_target_for_transcript(state_rx.borrow().last_session.as_ref());
            spawn_xdg_open_or_log(path.as_deref(), "OpenLastTranscript");
        }
    }
}

async fn dispatch_start(
    recorder: &Recorder1Proxy<'_>,
    state_tx: &watch::Sender<TrayState>,
    state_rx: &watch::Receiver<TrayState>,
) {
    let active_profile = state_rx.borrow().active_profile.clone();
    state_tx.send_modify(|s| s.pending_cmd = Some(PendingCmd::Start));

    match recorder.start_recording(&active_profile).await {
        Ok(session_id) => {
            debug!(session_id, "dispatcher: StartRecording accepted by daemon");
        }
        Err(err) => {
            warn!(error = %err, "dispatcher: StartRecording failed");
            state_tx.send_modify(|s| s.pending_cmd = None);
        }
    }
}

async fn dispatch_stop(
    recorder: &Recorder1Proxy<'_>,
    state_tx: &watch::Sender<TrayState>,
    state_rx: &watch::Receiver<TrayState>,
) {
    let session_id = state_rx.borrow().active_session_id.clone();
    let Some(id) = session_id else {
        warn!("dispatcher: Stop ignored, no active session id");
        return;
    };

    state_tx.send_modify(|s| s.pending_cmd = Some(PendingCmd::Stop));

    match recorder.stop_recording(&id).await {
        Ok(returned_id) => {
            debug!(
                session_id = returned_id,
                "dispatcher: StopRecording accepted"
            );
        }
        Err(err) => {
            warn!(error = %err, "dispatcher: StopRecording failed");
            state_tx.send_modify(|s| s.pending_cmd = None);
        }
    }
}

async fn dispatch_set_active_profile(
    profiles: &Profiles1Proxy<'_>,
    state_tx: &watch::Sender<TrayState>,
    name: &str,
) {
    state_tx.send_modify(|s| {
        s.pending_cmd = Some(PendingCmd::SetActiveProfile {
            name: name.to_owned(),
        });
    });

    if let Err(err) = profiles.set_active(name).await {
        warn!(error = %err, profile = name, "dispatcher: SetActive failed");
        state_tx.send_modify(|s| s.pending_cmd = None);
        return;
    }

    // Refresh the cached profile list so the radio mark moves
    // immediately without waiting for the next pump tick. Uses the
    // M5 list_v2 surface with graceful fall-back so older daemons
    // do not break the dispatcher.
    match crate::pump::list_profiles_for_dispatcher(profiles).await {
        Ok(list) => {
            state_tx.send_modify(|s| {
                s.profiles = list;
                name.clone_into(&mut s.active_profile);
                s.pending_cmd = None;
            });
        }
        Err(err) => {
            // SetActive succeeded but list failed — still clear the
            // action lock; the next pump tick will heal the cached
            // list.
            warn!(error = %err, "dispatcher: Profiles.List after SetActive failed");
            state_tx.send_modify(|s| {
                name.clone_into(&mut s.active_profile);
                s.pending_cmd = None;
            });
        }
    }
}

/// Pure helper: which path should `xdg-open` receive for an
/// `OpenLastRecording` command? Returns `None` when there is nothing
/// to open.
#[must_use]
pub fn open_target_for_recording(last: Option<&LastCompleted>) -> Option<PathBuf> {
    last.map(|l| l.audio_path.clone())
}

/// Pure helper: which path should `xdg-open` receive for an
/// `OpenLastTranscript` command? Returns `None` when there is no
/// transcript yet (audio-only phase) or no `last_session` at all.
#[must_use]
pub fn open_target_for_transcript(last: Option<&LastCompleted>) -> Option<PathBuf> {
    last.and_then(|l| l.transcript_path.clone())
}

/// Spawn `xdg-open <path>` in a detached tokio task. Logs both the
/// spawn-side failure (e.g. xdg-open not installed) and the exit
/// status separately so an opaque non-zero exit is distinguishable
/// from an outright spawn refusal (M4 fix L2).
fn spawn_xdg_open_or_log(path: Option<&Path>, source: &'static str) {
    let Some(path) = path else {
        warn!(%source, "dispatcher: nothing to open");
        return;
    };
    let path = path.to_owned();

    tokio::spawn(async move {
        let mut child = match Command::new("xdg-open")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(err) => {
                warn!(
                    %source,
                    path = %path.display(),
                    error = %err,
                    "dispatcher: xdg-open spawn failed",
                );
                return;
            }
        };

        match child.wait().await {
            Ok(status) if status.success() => {
                debug!(%source, path = %path.display(), "xdg-open ok");
            }
            Ok(status) => {
                warn!(
                    %source,
                    path = %path.display(),
                    code = ?status.code(),
                    "xdg-open exited with non-zero status",
                );
            }
            Err(err) => {
                warn!(
                    %source,
                    path = %path.display(),
                    error = %err,
                    "xdg-open wait failed",
                );
            }
        }
    });
}

/// Outcome of evaluating a `Stop` command before firing the RPC.
/// Extracted as its own enum so the synchronous decision branch can
/// be unit-tested without touching D-Bus.
#[derive(Debug, PartialEq, Eq)]
pub enum StopDispatchPlan {
    /// No active session id — log and skip the RPC.
    Skip,
    /// Active session id present — fire `StopRecording` with this id.
    Fire(String),
}

/// Pure helper: decide what `Stop` should do given the current
/// snapshot.
#[must_use]
pub fn plan_stop(state: &TrayState) -> StopDispatchPlan {
    state
        .active_session_id
        .clone()
        .map_or(StopDispatchPlan::Skip, StopDispatchPlan::Fire)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::state::IconState;

    fn audio_only_session() -> LastCompleted {
        LastCompleted {
            session_id: "sid".to_owned(),
            audio_path: PathBuf::from("/tmp/a.flac"),
            transcript_path: None,
            backend: None,
            completed_at_unix_ms: 0,
        }
    }

    fn full_session() -> LastCompleted {
        LastCompleted {
            session_id: "sid".to_owned(),
            audio_path: PathBuf::from("/tmp/a.flac"),
            transcript_path: Some(PathBuf::from("/tmp/a.flac.txt")),
            backend: Some("whisper-cli".to_owned()),
            completed_at_unix_ms: 0,
        }
    }

    #[test]
    fn pending_cmd_dispatch_skip_stop_when_no_session_id() {
        let s = TrayState {
            icon: IconState::Recording,
            active_session_id: None,
            ..TrayState::default()
        };
        assert_eq!(plan_stop(&s), StopDispatchPlan::Skip);
    }

    #[test]
    fn pending_cmd_dispatch_fires_stop_when_session_id_present() {
        let s = TrayState {
            icon: IconState::Recording,
            active_session_id: Some("sid-xyz".to_owned()),
            ..TrayState::default()
        };
        assert_eq!(plan_stop(&s), StopDispatchPlan::Fire("sid-xyz".to_owned()),);
    }

    #[test]
    fn xdg_open_path_for_open_last_recording_uses_audio_path() {
        let session = audio_only_session();
        let path = open_target_for_recording(Some(&session));
        assert_eq!(path, Some(PathBuf::from("/tmp/a.flac")));
    }

    #[test]
    fn xdg_open_args_for_audio_only_session_returns_audio_path() {
        let session = audio_only_session();
        let recording = open_target_for_recording(Some(&session));
        let transcript = open_target_for_transcript(Some(&session));
        assert_eq!(recording, Some(PathBuf::from("/tmp/a.flac")));
        assert!(
            transcript.is_none(),
            "audio-only phase has no transcript yet",
        );
    }

    #[test]
    fn xdg_open_path_for_open_last_transcript_returns_none_when_audio_only() {
        let session = audio_only_session();
        assert!(open_target_for_transcript(Some(&session)).is_none());
    }

    #[test]
    fn xdg_open_path_for_open_last_transcript_returns_transcript_path_when_full() {
        let session = full_session();
        assert_eq!(
            open_target_for_transcript(Some(&session)),
            Some(PathBuf::from("/tmp/a.flac.txt")),
        );
    }

    #[test]
    fn xdg_open_args_for_full_session_returns_transcript_when_open_transcript_requested() {
        let session = full_session();
        // OpenLastRecording always points at the audio file, even
        // once the transcript has landed — they are independent
        // commands.
        assert_eq!(
            open_target_for_recording(Some(&session)),
            Some(PathBuf::from("/tmp/a.flac")),
        );
        assert_eq!(
            open_target_for_transcript(Some(&session)),
            Some(PathBuf::from("/tmp/a.flac.txt")),
        );
    }

    #[test]
    fn xdg_open_target_returns_none_when_no_last_session() {
        assert!(open_target_for_recording(None).is_none());
        assert!(open_target_for_transcript(None).is_none());
    }
}
