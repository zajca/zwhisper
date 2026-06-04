//! `zwhisper deliver --listen` — the session-bound transcript delivery
//! consumer (RFC-daemon-role Feature 3).
//!
//! This process is normally launched by the auto-enabled
//! `zwhisper-deliver.service` user unit, bound to
//! `graphical-session.target`. It subscribes to `Jobs1.JobCompleted` and,
//! for each completed job, honours the job's RESOLVED `outputs` payload —
//! never re-reading the profile from disk (F3.1). The daemon already
//! decided what to deliver; we are a dumb, session-local executor of that
//! decision, which is the only component with access to the graphical
//! clipboard + notification daemon.
//!
//! ## Why this never errors out of the process
//!
//! The systemd unit runs with `Restart=on-failure`. A consumer that exits
//! non-zero on a missing Wayland session, a lost single-instance race, or
//! a daemon that is merely down would crash-loop noisily and spam the
//! journal. So every "we cannot usefully run right now" condition logs and
//! exits 0 (F3.4, F3.5): a clean exit is correct, the unit will be brought
//! back when its dependencies (graphical session, daemon) are ready.

pub(crate) mod sink;

use futures_util::StreamExt;
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::names::WellKnownName;
use zwhisper_ipc::{DELIVER_BUS_NAME, Jobs1Proxy};

use sink::{
    ClipboardDecision, ClipboardSink, TypeDecision, TypeSink, decide_clipboard, decide_type, notify,
};

use super::DAEMON_DOWN_HINT;

/// Synchronous entry point. Builds a one-shot current-thread runtime and
/// runs the async consumer loop. The consumer is designed never to return
/// an error to the caller (see module docs); it always resolves to a clean
/// exit, so `run` returns `Ok(())`.
pub(crate) fn run(args: &crate::cli::DeliverArgs) -> color_eyre::Result<()> {
    if !args.listen {
        // No other mode exists yet; the flag is mandatory in spirit. Make
        // the intent explicit rather than silently doing nothing.
        print_usage_note();
        return Ok(());
    }

    let rt = crate::commands::build_runtime()?;
    rt.block_on(run_async());
    Ok(())
}

#[allow(clippy::print_stdout)]
fn print_usage_note() {
    println!("deliver currently supports only --listen");
}

/// Outcome of the single-instance name claim, mirroring the tray's
/// `single_instance::claim` classification. Replicated here because we do
/// NOT depend on the excluded `zwhisper-tray` crate.
fn classify_name_reply(reply: &RequestNameReply) -> bool {
    match reply {
        // "we got it" and "we already had it" both mean we are primary.
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => true,
        // We pass `DoNotQueue`, so `InQueue` should not appear; treat it
        // (and `Exists`) as "another instance owns it" defensively.
        RequestNameReply::Exists | RequestNameReply::InQueue => false,
    }
}

/// Probe the graphical session the same way the tray does: a non-empty
/// `WAYLAND_DISPLAY`. Replicated locally (no tray dependency).
fn wayland_session_available() -> bool {
    std::env::var("WAYLAND_DISPLAY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

#[allow(clippy::print_stderr, clippy::print_stdout)]
async fn run_async() {
    // ---- F3.5: graphical-session probe ------------------------------
    // The unit uses `After=graphical-session.target` (not `Requisite=`)
    // so it may start even where the target never activates. Without a
    // Wayland session we have no clipboard and no notification daemon —
    // running would be useless. Exit 0 cleanly so the unit does not
    // crash-loop; it will be restarted when the session appears.
    if !wayland_session_available() {
        tracing::info!(
            "deliver: WAYLAND_DISPLAY unset/empty; no graphical session, exiting cleanly"
        );
        println!(
            "deliver: no Wayland session detected; nothing to do (will run under a graphical session)"
        );
        return;
    }

    // ---- Connect to the session bus --------------------------------
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            // A missing session bus is environmental, not a deliver fault.
            tracing::warn!(error = %err, "deliver: cannot connect to session bus; exiting cleanly");
            return;
        }
    };

    // ---- F3.4: single-instance via D-Bus name claim ----------------
    // Hold the name for the whole run. The name is released the instant
    // `conn` drops, so `conn` MUST stay alive for the entire loop below.
    match claim_single_instance(&conn).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(
                "deliver: another instance already owns {DELIVER_BUS_NAME}; exiting cleanly"
            );
            return;
        }
        Err(err) => {
            // A transient bus glitch during the claim should not crash the
            // process; without the name we might double-deliver, so we err
            // on the side of NOT running this instance. Exit 0 (the unit
            // restarts).
            tracing::warn!(error = %err, "deliver: name claim failed; exiting cleanly");
            return;
        }
    }

    // ---- Build the Jobs1 proxy + subscribe -------------------------
    let proxy = match Jobs1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "deliver: failed to build Jobs1 proxy; exiting cleanly");
            return;
        }
    };

    let mut signals = match proxy.receive_job_completed().await {
        Ok(s) => s,
        Err(err) => {
            // The daemon is likely down or predates Jobs1. Per the brief:
            // log a warning + surface the hint, but still exit 0 so the
            // systemd unit (Restart=on-failure) does not crash-loop noisily
            // on a daemon that is merely not up yet.
            tracing::warn!(error = %err, "deliver: cannot subscribe to Jobs1.JobCompleted; exiting cleanly");
            eprintln!("{DAEMON_DOWN_HINT}");
            return;
        }
    };

    let clipboard = ClipboardSink::new();
    let type_sink = TypeSink::new();
    tracing::info!("deliver: listening for Jobs1.JobCompleted");

    // ---- Consume signals until the stream ends ---------------------
    while let Some(signal) = signals.next().await {
        let args = match signal.args() {
            Ok(a) => a,
            Err(err) => {
                tracing::warn!(error = %err, "deliver: malformed JobCompleted payload; skipping");
                continue;
            }
        };
        handle_completed(&clipboard, &type_sink, &args).await;
    }

    // Stream ended (bus disconnected / daemon gone). Nothing more to do;
    // exit 0 and let systemd restart us when the daemon returns.
    tracing::info!("deliver: JobCompleted stream ended; exiting cleanly");
}

/// Claim `DELIVER_BUS_NAME` on `conn`. Returns `Ok(true)` when we are the
/// primary owner. Replicates the tray's `single_instance::claim`.
async fn claim_single_instance(conn: &zbus::Connection) -> Result<bool, zbus::Error> {
    let proxy = DBusProxy::new(conn).await?;
    let name = WellKnownName::try_from(DELIVER_BUS_NAME)
        .map_err(|e| zbus::Error::Failure(format!("invalid bus name {DELIVER_BUS_NAME}: {e}")))?;
    let reply = proxy
        .request_name(name, RequestNameFlags::DoNotQueue.into())
        .await?;
    Ok(classify_name_reply(&reply))
}

/// Act on a single `JobCompleted` payload. We trust the payload entirely
/// (F3.1) — `outputs` already encodes the daemon's resolved delivery plan;
/// we never look at the profile on disk.
async fn handle_completed(
    clipboard: &ClipboardSink,
    type_sink: &TypeSink,
    args: &zwhisper_ipc::jobs::JobCompletedArgs<'_>,
) {
    let submit_mode = args.submit_mode;
    let transcript_path = args.transcript_path;
    let bytes = args.bytes;

    tracing::info!(
        job_id = %args.job_id,
        submit_mode = %submit_mode,
        bytes = bytes,
        backend = %args.backend,
        outputs = args.outputs.len(),
        "deliver: handling completed job",
    );

    for entry in &args.outputs {
        match entry.first().map(String::as_str) {
            Some("notification") => {
                // Always-on notification output: surface where the
                // transcript landed. No size guard — it is just a path.
                notify(
                    "Transcript ready",
                    &format!("Transcript saved at: {transcript_path}"),
                )
                .await;
            }
            Some("clipboard") => {
                handle_clipboard(clipboard, submit_mode, transcript_path, bytes).await;
            }
            Some("type_at_cursor") => {
                handle_type(type_sink, clipboard, submit_mode, transcript_path, bytes).await;
            }
            // File delivery is the daemon's job (it writes the file before
            // emitting the signal). Any other / unknown tag: ignore.
            Some("file") | Some(_) | None => {}
        }
    }
}

/// Drive the F3.3 clipboard intent guard for a single `clipboard` output.
async fn handle_clipboard(
    clipboard: &ClipboardSink,
    submit_mode: &str,
    transcript_path: &str,
    bytes: u64,
) {
    match decide_clipboard(submit_mode, bytes, sink::CLIPBOARD_MAX_BYTES) {
        ClipboardDecision::Inject => {
            // Foreground + fits: read the transcript and inject it. On any
            // read/inject failure, fall back to a notification so the user
            // is not left with a silently empty clipboard.
            match tokio::fs::read_to_string(transcript_path).await {
                Ok(text) => {
                    if let Err(err) = clipboard.inject(&text).await {
                        tracing::warn!(error = %err, "deliver: clipboard injection failed; notifying instead");
                        notify(
                            "Transcript ready",
                            &format!(
                                "Could not copy to clipboard. Run `zwhisper output last --to clipboard` to retry. File: {transcript_path}"
                            ),
                        )
                        .await;
                    } else {
                        tracing::info!(
                            bytes = bytes,
                            "deliver: transcript injected into clipboard"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, path = %transcript_path, "deliver: transcript read failed; notifying instead");
                    notify(
                        "Transcript ready",
                        &format!("Could not read transcript file: {transcript_path}"),
                    )
                    .await;
                }
            }
        }
        ClipboardDecision::NotifyWithAction => {
            // Detached/auto (user not waiting) or too large: offer a manual
            // copy rather than hijacking the clipboard.
            notify(
                "Transcript ready",
                &format!(
                    "Run `zwhisper output last --to clipboard` to copy. File: {transcript_path}"
                ),
            )
            .await;
        }
        ClipboardDecision::Skip => {}
    }
}

/// Drive the F4 type-at-cursor intent guard for a single `type_at_cursor`
/// output (RFC-type-at-cursor F3 + F6). Typing injects keystrokes into the
/// focused window, so it is stricter than the clipboard path: we only type for
/// a foreground job that fits the (smaller) type ceiling, and every other
/// outcome degrades to a clipboard + notification fallback (OD4) or a
/// notify-with-action. No failure is ever swallowed — each path logs at WARN
/// and surfaces a notification.
async fn handle_type(
    type_sink: &TypeSink,
    clipboard: &ClipboardSink,
    submit_mode: &str,
    transcript_path: &str,
    bytes: u64,
) {
    // ---- F6 step 1: wtype presence gate -----------------------------
    // No `wtype` binary on $PATH (or a non-wlroots session): we cannot type
    // at all. Degrade to the clipboard + notification fallback so the user
    // still gets the transcript, enriching the message with the advisory
    // desktop hint when the session looks non-wlroots (GNOME/KWin).
    if !sink::wtype_present() {
        let hint = sink::desktop_hint(
            std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
            std::env::var("XDG_SESSION_DESKTOP").ok().as_deref(),
        );
        tracing::warn!(
            hint = hint.as_deref().unwrap_or("none"),
            "deliver: wtype not present; falling back to clipboard for type_at_cursor"
        );
        type_fallback_to_clipboard(clipboard, transcript_path, hint.as_deref()).await;
        return;
    }

    // ---- F4: intent guard -------------------------------------------
    match decide_type(submit_mode, bytes, sink::TYPE_MAX_BYTES) {
        TypeDecision::NotifyWithAction => {
            // Too large OR not foreground: do NOT type into whatever happens to
            // be focused, and do NOT hijack the clipboard either — just offer
            // the manual actions.
            notify(
                "Transcript ready",
                &format!(
                    "Run `zwhisper output last --to type` to type at the cursor, or `--to clipboard` to copy. File: {transcript_path}"
                ),
            )
            .await;
        }
        TypeDecision::Type => {
            // Foreground + fits: read the transcript and type it. A read
            // failure is observable (WARN + notify); a typing failure degrades
            // to the clipboard fallback (OD4: always, even on a type-only
            // profile, so the user is never left empty-handed).
            match tokio::fs::read_to_string(transcript_path).await {
                Ok(text) => match type_sink.type_text(&text).await {
                    Ok(()) => {
                        tracing::info!(bytes = bytes, "deliver: transcript typed at cursor");
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "deliver: wtype failed; falling back to clipboard");
                        // Best-effort clipboard inject; failures are logged
                        // inside the inject path, the notify always fires.
                        if let Err(inject_err) = clipboard.inject(&text).await {
                            tracing::warn!(error = %inject_err, "deliver: clipboard fallback after wtype failure also failed");
                        }
                        notify(
                            "Transcript ready",
                            "Typed delivery unavailable — transcript copied to clipboard (Ctrl+V), or run `zwhisper output last --to clipboard`.",
                        )
                        .await;
                    }
                },
                Err(err) => {
                    tracing::warn!(error = %err, path = %transcript_path, "deliver: transcript read failed; notifying instead");
                    notify(
                        "Transcript ready",
                        &format!("Could not read transcript file: {transcript_path}"),
                    )
                    .await;
                }
            }
        }
    }
}

/// Shared F6 fallback for the type-at-cursor path: best-effort copy the
/// transcript into the clipboard and raise a notification explaining that
/// typing was unavailable. Used when `wtype` is absent. The clipboard inject
/// is best-effort (its own failures are logged); the notification always
/// fires, optionally enriched with the advisory desktop `hint`.
async fn type_fallback_to_clipboard(
    clipboard: &ClipboardSink,
    transcript_path: &str,
    hint: Option<&str>,
) {
    // Read first so we can copy the actual transcript; a read failure still
    // notifies so the user is never left without feedback.
    let read = tokio::fs::read_to_string(transcript_path).await;
    match read {
        Ok(text) => {
            if let Err(inject_err) = clipboard.inject(&text).await {
                tracing::warn!(error = %inject_err, "deliver: clipboard fallback for type_at_cursor failed");
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, path = %transcript_path, "deliver: transcript read failed during type fallback");
        }
    }

    let body = match hint {
        Some(reason) => format!(
            "Typing unavailable ({reason}) — transcript copied to clipboard (Ctrl+V), or run `zwhisper output last --to clipboard`."
        ),
        None => "Typing unavailable (wtype not found) — transcript copied to clipboard (Ctrl+V), or run `zwhisper output last --to clipboard`.".to_owned(),
    };
    notify("Transcript ready", &body).await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn classify_primary_owner_is_true() {
        assert!(classify_name_reply(&RequestNameReply::PrimaryOwner));
    }

    #[test]
    fn classify_already_owner_is_true() {
        assert!(classify_name_reply(&RequestNameReply::AlreadyOwner));
    }

    #[test]
    fn classify_exists_is_false() {
        assert!(!classify_name_reply(&RequestNameReply::Exists));
    }

    #[test]
    fn classify_in_queue_is_false_defensive() {
        assert!(!classify_name_reply(&RequestNameReply::InQueue));
    }

    #[test]
    fn deliver_bus_name_is_subpath_of_daemon_name() {
        assert!(DELIVER_BUS_NAME.starts_with("cz.zajca.Zwhisper1"));
        assert_ne!(DELIVER_BUS_NAME, "cz.zajca.Zwhisper1");
    }
}
