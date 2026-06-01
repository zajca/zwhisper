//! `zwhisper record --profile <name>` — D-Bus client over `zwhisperd`.
//!
//! ## Lifecycle (signal-driven)
//!
//! ```text
//!   ┌──────────┐  receive_state_changed         ┌──────────┐
//!   │ CLI      │ ─── (subscribe FIRST) ──────►  │ zwhisperd│
//!   │          │     receive_recording_complete │          │
//!   │          │     receive_transcript_complete│          │
//!   │          │ ──── StartRecording(profile) ──►          │
//!   │          │ ◄──── session_id (UUID v4) ────           │
//!   │          │                                            │
//!   │          │ ◄── StateChanged "starting"                │
//!   │          │ ◄── StateChanged "recording"               │
//!   │          │ ◄── StateChanged "stopping" (on stop)      │
//!   │          │ ◄── RecordingComplete{audio_path}          │
//!   │          │ ◄── TranscriptComplete (if auto)           │
//!   │          │ ◄── StateChanged "idle"  (TERMINAL — C3)   │
//!   │          │   or                                       │
//!   │          │ ◄── StateChanged "failed" (TERMINAL)       │
//!   └──────────┘                                            └──────────┘
//! ```
//!
//! Why subscribe first: the daemon emits `StateChanged "starting"`
//! synchronously inside `StartRecording` before returning the
//! `session_id`. If we subscribed only after the call returned we
//! would lose that first signal — the missed-signal race called out
//! as risk #2 in `docs/M3-plan.md`.
//!
//! ## C4 — `session_id` filtering
//!
//! Every signal carries a `session_id`. We compare against the id
//! returned by `StartRecording` and drop mismatched signals (a stale
//! signal from a previous session that the bus daemon happened to
//! deliver to us).
//!
//! ## C3 — `StateChanged "idle"` is the terminal signal
//!
//! Regardless of whether the profile has `transcription.auto = true`
//! (in which case `TranscriptComplete` arrives before idle) or
//! `false` (no transcript at all), `StateChanged "idle"` is the
//! single terminal signal. The CLI exits as soon as it sees it.
//!
//! ## Exit codes (frozen for M3+)
//!
//! - `0` — clean stop, optional transcript delivered
//! - `1` — `StateChanged "failed"` or typed `RpcError::RecordingFailed`
//! - `2` — user-facing protocol error (daemon down, profile missing,
//!   session in use, M3 narrow violation)
//! - `3` — IPC failure (transport / disconnect)

use futures_util::StreamExt;
use tracing::{debug, info, warn};
use zwhisper_core::profile;
use zwhisper_ipc::Recorder1Proxy;

use crate::cli::RecordArgs;

use super::{
    DAEMON_DOWN_HINT, EXIT_IPC_FAILURE, EXIT_OK, EXIT_PROTOCOL_ERROR, EXIT_RECORDING_FAILED,
    build_runtime, classify_error, is_daemon_down,
};

/// M3 narrow message — surfaced as exit 2 when the bare-flag form is
/// used at runtime. The clap surface still parses the flags so the
/// existing tests stay green; the runtime gate lives here.
pub(crate) const M3_NARROW_HINT: &str = "M3 narrowed `record` to require --profile. Use `--profile default` for the M0/M1 invocation shape.";

pub(crate) fn run(args: &RecordArgs) -> color_eyre::Result<()> {
    let Some(profile_name) = args.profile.clone() else {
        // Bare-flag form: clap accepted it (kept for back-compat with
        // M2's regression-net tests), but M3 narrowed `record` to
        // `--profile`. Exit 2 with the actionable hint.
        eprintln_via_tracing(M3_NARROW_HINT);
        std::process::exit(EXIT_PROTOCOL_ERROR);
    };

    let rt = build_runtime()?;
    let code = rt.block_on(run_async(&profile_name));
    if code == EXIT_OK {
        Ok(())
    } else {
        std::process::exit(code);
    }
}

/// Print to stderr without bypassing the workspace's `print_stderr`
/// lint. We funnel via `eprintln!` directly inside an `#[allow]` so
/// the rest of the file stays under the global lint umbrella.
#[allow(clippy::print_stderr)]
fn eprintln_via_tracing(msg: &str) {
    eprintln!("{msg}");
}

#[allow(clippy::print_stderr, clippy::too_many_lines)]
async fn run_async(profile_name: &str) -> i32 {
    // 1. Connect to the session bus.
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("{DAEMON_DOWN_HINT}");
            eprintln!("failed to connect to session bus: {err}");
            return EXIT_PROTOCOL_ERROR;
        }
    };

    // 2. Build the proxy.
    let proxy = match Recorder1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            if is_daemon_down(&err) {
                eprintln!("{DAEMON_DOWN_HINT}");
                return EXIT_PROTOCOL_ERROR;
            }
            eprintln!("failed to build Recorder1 proxy: {err}");
            return EXIT_IPC_FAILURE;
        }
    };

    // 2b. M8 pre-flight handshake. A version mismatch must abort
    //     before we subscribe to signals, otherwise we would print a
    //     "starting" event from a daemon we cannot trust.
    match super::verify_protocol(&proxy).await {
        super::HandshakeOutcome::Match | super::HandshakeOutcome::DaemonDown => {}
        super::HandshakeOutcome::Mismatch(err) => return super::report_protocol_mismatch(&err),
    }

    // 3. SUBSCRIBE FIRST (race-fix). The daemon emits
    //    `StateChanged "starting"` *inside* StartRecording; if we
    //    subscribed only after the call returned, we would miss it.
    let mut state_stream = match proxy.receive_state_changed().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to subscribe to StateChanged: {err}");
            return EXIT_IPC_FAILURE;
        }
    };
    let mut recording_complete_stream = match proxy.receive_recording_complete().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to subscribe to RecordingComplete: {err}");
            return EXIT_IPC_FAILURE;
        }
    };
    let mut transcript_complete_stream = match proxy.receive_transcript_complete().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to subscribe to TranscriptComplete: {err}");
            return EXIT_IPC_FAILURE;
        }
    };

    // 4. NOW call StartRecording.
    let session_id = match proxy.start_recording(profile_name).await {
        Ok(id) => id,
        Err(err) => {
            if is_daemon_down(&err) {
                eprintln!("{DAEMON_DOWN_HINT}");
                return EXIT_PROTOCOL_ERROR;
            }
            eprintln!("StartRecording failed: {err}");
            return classify_error(&err);
        }
    };
    info!(session_id = %session_id, profile = %profile_name, "StartRecording accepted");

    // 5. Look up `transcription.auto` locally so we know whether the
    //    daemon will emit a TranscriptComplete before idle. The
    //    terminal signal is *always* `StateChanged "idle"` (C3) — the
    //    `auto` flag only controls which paths we print.
    let auto_transcribe = match profile::load(profile_name) {
        Ok(p) => p.transcription.auto,
        Err(err) => {
            // The daemon already loaded it successfully (we got a
            // session_id back), so a local mismatch here is unusual
            // but non-fatal: assume auto = false and continue.
            warn!(error = %err, "could not load profile locally to read transcription.auto; assuming false");
            false
        }
    };
    info!(
        auto_transcribe,
        "profile loaded for transcript-aware printing"
    );

    // 6. Lifecycle loop.
    let mut audio_path: Option<String> = None;
    let mut transcript: Option<TranscriptInfo> = None;
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    let mut sent_stop = false;
    // One flag per signal stream so a stream that yielded `None`
    // (broker closed, daemon disconnected) stops being polled.
    // tokio::select! disables a branch whose `if` guard is false, so
    // setting these to `true` removes the dead arm; the
    // `state_done && recording_done && transcript_done` check at
    // the top of the loop turns the all-streams-dead state into a
    // clean EXIT_IPC_FAILURE return instead of a panic.
    let mut state_done = false;
    let mut recording_done = false;
    let mut transcript_done = false;

    loop {
        // Daemon-disconnect detection. When zwhisperd disappears
        // without emitting a terminal `StateChanged`, all three
        // signal streams resolve to `None` and stay disabled in
        // `tokio::select!`. Without this guard the loop would
        // either spin forever (with `ctrl_c` still pending) or
        // panic ("all branches are disabled and there is no else
        // branch") once the user pressed Ctrl+C and `sent_stop`
        // disabled the last live arm. Detect the all-streams-dead
        // condition explicitly and bail with EXIT_IPC_FAILURE.
        if state_done && recording_done && transcript_done {
            eprintln!(
                "daemon disconnected before terminal StateChanged for session {session_id}; \
                 audio file (if any) is at the path the daemon last reported"
            );
            print_artifacts(audio_path.as_ref(), transcript.as_ref(), auto_transcribe);
            return EXIT_IPC_FAILURE;
        }

        tokio::select! {
            // Branch 1 — StateChanged
            maybe_signal = state_stream.next(), if !state_done => {
                let Some(signal) = maybe_signal else {
                    debug!("StateChanged stream closed");
                    state_done = true;
                    continue;
                };
                let args_res = signal.args();
                let Ok(args) = args_res else {
                    debug!("StateChanged with malformed args, dropping");
                    continue;
                };
                if args.session_id != session_id {
                    debug!(
                        got = %args.session_id,
                        expected = %session_id,
                        "StateChanged for a different session, dropping (C4)"
                    );
                    continue;
                }
                info!(state = %args.new_state, "StateChanged");
                match args.new_state {
                    "idle" => {
                        // Terminal — clean exit.
                        print_artifacts(audio_path.as_ref(), transcript.as_ref(), auto_transcribe);
                        return EXIT_OK;
                    }
                    "failed" => {
                        // Terminal — recording failure.
                        print_artifacts(audio_path.as_ref(), transcript.as_ref(), auto_transcribe);
                        eprintln!("recording failed (StateChanged \"failed\")");
                        return EXIT_RECORDING_FAILED;
                    }
                    other => {
                        // starting / recording / stopping — just log.
                        debug!(state = %other, "non-terminal state");
                    }
                }
            }

            // Branch 2 — RecordingComplete
            maybe_signal = recording_complete_stream.next(), if !recording_done => {
                let Some(signal) = maybe_signal else {
                    debug!("RecordingComplete stream closed");
                    recording_done = true;
                    continue;
                };
                let Ok(args) = signal.args() else {
                    debug!("RecordingComplete with malformed args, dropping");
                    continue;
                };
                if args.session_id != session_id {
                    debug!(
                        got = %args.session_id,
                        expected = %session_id,
                        "RecordingComplete for a different session, dropping (C4)"
                    );
                    continue;
                }
                info!(audio_path = %args.audio_path, "RecordingComplete");
                audio_path = Some(args.audio_path.to_owned());
            }

            // Branch 3 — TranscriptComplete
            maybe_signal = transcript_complete_stream.next(), if !transcript_done => {
                let Some(signal) = maybe_signal else {
                    debug!("TranscriptComplete stream closed");
                    transcript_done = true;
                    continue;
                };
                let Ok(args) = signal.args() else {
                    debug!("TranscriptComplete with malformed args, dropping");
                    continue;
                };
                if args.session_id != session_id {
                    debug!(
                        got = %args.session_id,
                        expected = %session_id,
                        "TranscriptComplete for a different session, dropping (C4)"
                    );
                    continue;
                }
                info!(
                    transcript_path = %args.transcript_path,
                    bytes = args.bytes,
                    backend = %args.backend,
                    "TranscriptComplete"
                );
                transcript = Some(TranscriptInfo {
                    path: args.transcript_path.to_owned(),
                    bytes: args.bytes,
                    backend: args.backend.to_owned(),
                });
            }

            // Branch 4 — Ctrl+C: politely stop, keep waiting for
            // the terminal StateChanged. Idempotent — second Ctrl+C
            // is a no-op so the user cannot accidentally double-stop.
            res = &mut ctrl_c, if !sent_stop => {
                if let Err(err) = res {
                    warn!(error = %err, "ctrl_c handler failed");
                    continue;
                }
                info!("Ctrl+C received, calling StopRecording");
                if let Err(err) = proxy.stop_recording(&session_id).await {
                    eprintln!("StopRecording failed: {err}");
                    // Do not bail — the daemon may still emit a
                    // terminal StateChanged.
                }
                sent_stop = true;
            }

            // Defensive: tokio::select! requires an `else` arm when
            // every branch can become disabled (all three streams
            // closed AND `sent_stop = true`). Without it the macro
            // panics with "all branches are disabled and there is
            // no else branch". The check at the top of the loop
            // turns the all-streams-dead state into a clean
            // EXIT_IPC_FAILURE return, so this arm just yields
            // control once to break out of the macro's poll cycle.
            else => {
                tokio::task::yield_now().await;
            }
        }
    }
}

#[derive(Debug)]
struct TranscriptInfo {
    path: String,
    bytes: u64,
    backend: String,
}

#[allow(clippy::print_stderr)]
fn print_artifacts(audio: Option<&String>, transcript: Option<&TranscriptInfo>, auto: bool) {
    if let Some(audio) = audio {
        println!("audio: {audio}");
    } else {
        eprintln!("(no RecordingComplete arrived before terminal signal)");
    }
    if auto {
        if let Some(t) = transcript {
            println!(
                "transcript: {} ({} bytes, backend={})",
                t.path, t.bytes, t.backend
            );
        } else {
            // auto = true but no TranscriptComplete arrived — daemon
            // emitted "failed" before the transcribe step finished.
            eprintln!("(transcription.auto = true but no TranscriptComplete arrived)");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn m3_narrow_hint_mentions_profile() {
        assert!(M3_NARROW_HINT.contains("--profile"));
        assert!(M3_NARROW_HINT.contains("default"));
    }

    /// Sanity test for the documented signal-subscription order in
    /// `run_async`. The order matters for the missed-signal race
    /// (risk #2). We grep our own source file (compiled-in via
    /// `include_str!`) and assert that the three `receive_*` calls
    /// appear before the `start_recording` call. This protects against
    /// a refactor that reorders them.
    #[test]
    fn signal_subscriptions_happen_before_start_recording() {
        let source = include_str!("record.rs");
        let pos_state = source
            .find("proxy.receive_state_changed()")
            .expect("subscribe to state_changed");
        let pos_rec = source
            .find("proxy.receive_recording_complete()")
            .expect("subscribe to recording_complete");
        let pos_tr = source
            .find("proxy.receive_transcript_complete()")
            .expect("subscribe to transcript_complete");
        let pos_start = source
            .find("proxy.start_recording(profile_name)")
            .expect("call start_recording");
        assert!(
            pos_state < pos_start,
            "state_changed must subscribe BEFORE StartRecording"
        );
        assert!(
            pos_rec < pos_start,
            "recording_complete must subscribe BEFORE StartRecording"
        );
        assert!(
            pos_tr < pos_start,
            "transcript_complete must subscribe BEFORE StartRecording"
        );
    }
}
