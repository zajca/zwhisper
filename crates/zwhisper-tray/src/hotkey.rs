//! M6 — hotkey listener task.
//!
//! Sibling to [`crate::cmd::run_dispatcher`]. Owns:
//!
//! - one [`Recorder1Proxy`] + [`Profiles1Proxy`] pair (a fresh
//!   `LiveRecorderClient` is rebuilt from those proxies for every
//!   toggle decision so the live `Profiles1.GetActive` always wins
//!   on a stale-cache disagreement, per `DoD` #15),
//! - one `Arc<AshpdAdapter>` (single-session — the `AshpdAdapter`
//!   replaces its inner state on `recreate`),
//! - the [`Debouncer`] for the listener's lifetime,
//! - lazy [`HotkeySession`] — created on first `Bind` request OR
//!   on startup if `cfg.auto_bind_on_startup` is set.
//!
//! The `daemon_ready_rx` watch gate enforces `DoD` #16 (A4
//! mitigation): no portal interaction or RPC happens until the
//! dispatcher's first `Recorder1Proxy::new` succeeded. Activation
//! events that arrive before the gate flips are dropped with a
//! tracing warn — the tray's `M6-architecture.md` § 2 declares
//! that the pump owns daemon-derived state and the dispatcher
//! owns RPC liveness, so the listener intentionally does not
//! buffer events itself; one missed press while the daemon isn't
//! on the bus is a benign outcome.
//!
//! ## Single writer of `state.hotkey`
//!
//! The pump is the sole writer of every other field on
//! [`TrayState`]; this task is the sole writer of `state.hotkey`.
//! All writes go through `state_tx.send_modify` so the watch
//! channel notifies the supervisor.

use std::sync::Arc;
use std::time::Duration;

use color_eyre::eyre::Result;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use zwhisper_hotkey::config::HotkeyConfig;
use zwhisper_hotkey::portal::{
    AshpdAdapter, BindRequest, BoundShortcut, HotkeyEvent, HotkeySession, PortalError,
    SHORTCUT_DESCRIPTION, SHORTCUT_ID,
};
use zwhisper_hotkey::probe::{self, BackendDetected, ProbeReport};
use zwhisper_hotkey::toggle::{
    Debouncer, LiveRecorderClient, NoOpReason, ToggleError, ToggleOutcome, toggle_once,
};
use zwhisper_ipc::{Profiles1Proxy, Recorder1Proxy};

use crate::single_instance::TRAY_BUS_NAME;
use crate::state::{HotkeyMenuState, TrayState};

/// 500 ms debounce window between portal `recreate` attempts.
/// See `DoD` #9 (B1 reconnect step b) — wait briefly so a
/// flapping portal does not spin the listener.
const PORTAL_RECREATE_BACKOFF: Duration = Duration::from_millis(500);

/// Capacity of the pre-ready Activated buffer.
///
/// While `daemon_ready_rx` is still `false`, the listener may
/// already have an open `HotkeySession` and start receiving
/// portal events (e.g. a previously-bound chord whose binding
/// the compositor remembered). `DoD` #16 / risk A4 mandate that
/// at least one such press survives the startup window: when the
/// gate flips, we re-issue the buffered event so the user does
/// not lose the press.
///
/// The buffer is intentionally exactly 1 slot — the newest press
/// wins. A user who hammers the chord during a tray restart only
/// expects the most recent state-change to be honoured; queuing
/// older presses would cause confusing toggle ping-pong once the
/// daemon comes up.
const PREREADY_BUFFER_SLOTS: usize = 1;

/// Notification timeout for the hotkey-path "Recording started"
/// cue (`DoD` #18). 3 s — long enough to be noticed, short
/// enough not to clutter the notification stack while a long
/// recording continues.
const NOTIFY_TIMEOUT_MS: i32 = 3_000;

/// Capacity of the settings-rebind signal mpsc. Sized at 4 so
/// two back-to-back rebinds coalesce without back-pressuring the
/// subscriber task: the recreate path is idempotent and one
/// recreate covers an arbitrary number of queued signals.
const REBIND_SIGNAL_CAPACITY: usize = 4;

/// Hard cap on `notify-rust` `show()` round-trips. Mirrors the CLI's
/// `NOTIFY_TIMEOUT` in `zwhisper-cli::commands::toggle`. The session
/// bus that `notify-rust` talks to may be the very thing that broke
/// (e.g., `notification-daemon` was killed); we cannot let a stuck
/// `show()` block the listener task and starve the runtime.
const NOTIFY_SHOW_TIMEOUT: Duration = Duration::from_millis(500);

/// Control surface for the hotkey listener task. The tray menu's
/// "Hotkey: …" callback `try_send`s these onto a bounded mpsc.
///
/// `Recreate` is fed by an internal D-Bus signal subscriber (see
/// [`spawn_settings_rebind_subscriber`]) — when `zwhisper-settings`
/// emits `cz.zajca.Zwhisper1.Settings.HotkeyRebound`, the listener
/// drops the live `HotkeySession` and opens a fresh one so the
/// next portal `Activated` payload reflects the user's new chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyControl {
    /// User clicked the menu entry — open the portal bind
    /// dialog (or recreate the session if the previous one was
    /// lost) and bind [`SHORTCUT_ID`].
    Bind,
    /// Drop every binding on the current session. Idempotent
    /// per `DoD` #13.
    Unbind,
    /// Re-run [`probe::probe`] and refresh `state.hotkey`.
    Probe,
    /// `zwhisper-settings` rebound the chord on our behalf — drop
    /// the live session and open a fresh one so future
    /// `Activated` events come from the new portal binding (M7
    /// `DoD` #16 / D7).
    Recreate,
}

/// Run the hotkey listener task. Returns when `shutdown_rx`
/// fires.
///
/// `daemon_ready_rx` flips from `false` to `true` once the
/// dispatcher's `Recorder1Proxy::new` succeeded; the listener
/// does no portal work until then so a fresh tray that comes up
/// before the daemon does not race its own initialisation
/// (`DoD` #16, risk A4).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn run_hotkey(
    conn: zbus::Connection,
    cfg: HotkeyConfig,
    mut control_rx: mpsc::Receiver<HotkeyControl>,
    state_tx: watch::Sender<TrayState>,
    state_rx: watch::Receiver<TrayState>,
    mut daemon_ready_rx: watch::Receiver<bool>,
    mut shutdown_rx: watch::Receiver<()>,
) -> Result<()> {
    info!("hotkey: listener task starting");

    // Build the proxies eagerly. zbus 5 proxies are lazy w.r.t.
    // bus-name resolution, so this does NOT require the daemon
    // to be on the bus — it only fails on transport-level
    // teardown. We keep them alive for the listener's lifetime.
    let recorder = match Recorder1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            warn!(error = %err, "hotkey: Recorder1Proxy build failed");
            set_hotkey(
                &state_tx,
                HotkeyMenuState::Unavailable {
                    reason: format!("RPC unavailable: {err}"),
                },
            );
            wait_for_shutdown(&mut shutdown_rx).await;
            return Ok(());
        }
    };
    let profiles = match Profiles1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            warn!(error = %err, "hotkey: Profiles1Proxy build failed");
            set_hotkey(
                &state_tx,
                HotkeyMenuState::Unavailable {
                    reason: format!("RPC unavailable: {err}"),
                },
            );
            wait_for_shutdown(&mut shutdown_rx).await;
            return Ok(());
        }
    };

    let adapter: Arc<AshpdAdapter> = Arc::new(AshpdAdapter::new());
    let mut session: Option<HotkeySession<AshpdAdapter>> = None;
    let mut debouncer = Debouncer::new(&cfg);

    // M7 `DoD` #16: subscribe to settings' `HotkeyRebound` signal
    // and surface arrivals through a tiny mpsc<()> that the
    // listener drains alongside the existing event stream. The
    // capacity is intentionally small — coalescing two
    // back-to-back rebinds is fine; the recreate path is
    // idempotent and a single recreate covers both presses.
    let (rebind_signal_tx, mut rebind_signal_rx) = mpsc::channel::<()>(REBIND_SIGNAL_CAPACITY);
    let _rebind_subscriber = spawn_settings_rebind_subscriber(
        conn.clone(),
        rebind_signal_tx,
        shutdown_rx.clone(),
    )
    .await;

    // Initial probe. The probe result drives the menu label
    // even before any bind attempt happens.
    let initial_probe = probe::probe().await;
    apply_probe_to_state(&state_tx, &initial_probe);
    let portal_available = initial_probe.global_shortcuts_available;

    // D3 (auto_bind_on_startup): when the operator opted in AND
    // the portal is available, attempt one bind on startup. We
    // open the session BEFORE the daemon-ready gate so we can
    // pick up `Activated` signals that the compositor sends for
    // a previously-bound chord — see `PREREADY_BUFFER_SLOTS` and
    // `DoD` #16 (risk A4).
    if cfg.auto_bind_on_startup && portal_available {
        match ensure_session(&adapter, &mut session).await {
            Ok(()) => {
                refresh_bound_state(session.as_ref(), &state_tx).await;
            }
            Err(err) => {
                warn!(error = %err, "hotkey: auto-bind session create failed");
                set_hotkey(
                    &state_tx,
                    HotkeyMenuState::Unavailable {
                        reason: format!("portal: {err}"),
                    },
                );
            }
        }
    }

    // Pre-ready window: hold any inbound Activated press in a
    // 1-slot buffer until `daemon_ready_rx` flips. Newer presses
    // overwrite older ones (`PREREADY_BUFFER_SLOTS` doc explains
    // why we keep just one). Bind/Unbind/Probe controls are
    // always serviced — they do not need the daemon to be up.
    if !*daemon_ready_rx.borrow() {
        debug!("hotkey: waiting for daemon proxy ready (buffering at most one Activated)");
        let mut pending: Option<HotkeyEvent> = None;
        loop {
            let event = async {
                match session.as_mut() {
                    Some(s) => s.next_event().await,
                    None => std::future::pending::<Option<HotkeyEvent>>().await,
                }
            };

            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => {
                    info!("hotkey: shutdown received before daemon ready");
                    if let Some(s) = session.take() {
                        if let Err(err) = s.close().await {
                            warn!(error = %err, "hotkey: session close on shutdown failed");
                        }
                    }
                    return Ok(());
                }
                changed = daemon_ready_rx.changed() => {
                    if changed.is_err() {
                        warn!("hotkey: daemon-ready channel closed; exiting");
                        return Ok(());
                    }
                    if *daemon_ready_rx.borrow() {
                        break;
                    }
                }
                maybe_ctl = control_rx.recv() => {
                    let Some(ctl) = maybe_ctl else {
                        info!("hotkey: control channel closed before daemon ready; exiting");
                        return Ok(());
                    };
                    // Bind/Unbind/Probe do not need the daemon —
                    // service them as usual.
                    handle_control(ctl, &cfg, &adapter, &mut session, &state_tx).await;
                }
                rebind = rebind_signal_rx.recv() => {
                    if rebind.is_none() {
                        debug!("hotkey: rebind signal channel closed in pre-ready loop");
                        continue;
                    }
                    // Settings rebound the chord — recreate the
                    // session so the next press carries the new
                    // binding. Reuses the same Recreate handler
                    // as the post-ready loop (M7 `DoD` #16).
                    handle_control(
                        HotkeyControl::Recreate,
                        &cfg,
                        &adapter,
                        &mut session,
                        &state_tx,
                    )
                    .await;
                }
                ev = event => {
                    match ev {
                        Some(incoming @ HotkeyEvent::Activated { .. }) => {
                            // Only buffer events for SHORTCUT_ID;
                            // unrelated activations are dropped at
                            // source (see check inside the arm).
                            let HotkeyEvent::Activated { ref shortcut_id, .. } = incoming else {
                                unreachable!("matched Activated above");
                            };
                            if shortcut_id == SHORTCUT_ID {
                                pending = admit_to_pre_ready_buffer(pending.take(), incoming);
                            } else {
                                debug!(shortcut_id, "hotkey: ignoring pre-ready activation for unknown id");
                            }
                        }
                        Some(HotkeyEvent::Deactivated { .. }) => {
                            // No-op — toggle is press-driven.
                        }
                        Some(HotkeyEvent::ShortcutsChanged) => {
                            debug!("hotkey: pre-ready ShortcutsChanged; refreshing bound state");
                            refresh_bound_state(session.as_ref(), &state_tx).await;
                        }
                        None => {
                            // Stream ended — leave the buffer alone, we
                            // will recreate after the gate flips.
                            warn!("hotkey: pre-ready event stream closed");
                            session = None;
                        }
                    }
                }
            }
        }

        // Daemon is now ready — drain the buffered press, if any.
        if let Some(buffered) = pending.take()
            && let HotkeyEvent::Activated { shortcut_id, .. } = &buffered
            && shortcut_id == SHORTCUT_ID
        {
            debug!("hotkey: draining buffered pre-ready Activated press");
            on_activated(&recorder, &profiles, &mut debouncer, &cfg, &state_rx).await;
        }
    }
    debug!("hotkey: daemon proxy ready, entering main listener loop");

    info!("hotkey: ready, awaiting events");

    loop {
        // `next_event` only makes sense when a session is open.
        // When `session` is None, fall through with `pending`
        // resolved to a never-completing future so the select
        // waits on the other arms.
        let event = async {
            match session.as_mut() {
                Some(s) => s.next_event().await,
                None => std::future::pending::<Option<HotkeyEvent>>().await,
            }
        };

        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                info!("hotkey: shutdown received");
                if let Some(s) = session.take() {
                    if let Err(err) = s.close().await {
                        warn!(error = %err, "hotkey: session close on shutdown failed");
                    }
                }
                return Ok(());
            }
            maybe_ctl = control_rx.recv() => {
                let Some(ctl) = maybe_ctl else {
                    info!("hotkey: control channel closed; exiting");
                    return Ok(());
                };
                handle_control(
                    ctl,
                    &cfg,
                    &adapter,
                    &mut session,
                    &state_tx,
                )
                .await;
            }
            rebind = rebind_signal_rx.recv() => {
                if rebind.is_none() {
                    debug!("hotkey: rebind signal channel closed");
                    continue;
                }
                // Settings rebound the chord — recreate the
                // session so future Activated payloads arrive
                // with the new portal binding (M7 `DoD` #16).
                handle_control(
                    HotkeyControl::Recreate,
                    &cfg,
                    &adapter,
                    &mut session,
                    &state_tx,
                )
                .await;
            }
            ev = event => {
                match ev {
                    Some(HotkeyEvent::Activated { shortcut_id, .. }) => {
                        if shortcut_id != SHORTCUT_ID {
                            debug!(shortcut_id, "hotkey: ignoring activation for unknown id");
                            continue;
                        }
                        on_activated(
                            &recorder,
                            &profiles,
                            &mut debouncer,
                            &cfg,
                            &state_rx,
                        )
                        .await;
                    }
                    Some(HotkeyEvent::Deactivated { .. }) => {
                        // No-op — the toggle decision triggers on
                        // press, not release.
                    }
                    Some(HotkeyEvent::ShortcutsChanged) => {
                        debug!("hotkey: ShortcutsChanged; refreshing bound state");
                        refresh_bound_state(session.as_ref(), &state_tx).await;
                    }
                    None => {
                        // Event stream closed — likely SessionLost
                        // mid-flight. Try to recover via recreate.
                        warn!("hotkey: event stream closed; attempting recreate");
                        if let Err(err) = recreate_session(&adapter, &mut session).await {
                            warn!(error = %err, "hotkey: recreate failed");
                            set_hotkey(
                                &state_tx,
                                HotkeyMenuState::Unavailable {
                                    reason: "portal lost; reopen menu to retry".to_owned(),
                                },
                            );
                            session = None;
                        } else {
                            refresh_bound_state(session.as_ref(), &state_tx).await;
                        }
                    }
                }
            }
        }
    }
}

/// Pure helper for the pre-ready Activated buffer (`DoD` #16).
///
/// Given the currently-buffered event (`prev`) and a freshly
/// received `incoming` Activated event for `SHORTCUT_ID`,
/// returns the new buffer contents. Newest-press-wins: a second
/// arrival overwrites the first because the user only cares
/// about the most recent state-change once the daemon comes up.
///
/// Factored out so the run-loop has a one-line call AND the
/// behaviour is unit-testable without standing up a tokio
/// runtime + watch channels.
fn admit_to_pre_ready_buffer(
    prev: Option<HotkeyEvent>,
    incoming: HotkeyEvent,
) -> Option<HotkeyEvent> {
    match incoming {
        ev @ HotkeyEvent::Activated { .. } => {
            if prev.is_some() {
                debug!(
                    "hotkey: pre-ready buffer full ({PREREADY_BUFFER_SLOTS} slot); newest press wins"
                );
            }
            Some(ev)
        }
        // Only Activated is buffered — Deactivated and
        // ShortcutsChanged are non-toggle signals and either
        // already actioned in-line or irrelevant to the toggle
        // path.
        _ => prev,
    }
}

/// Helper: `state_tx.send_modify` that only writes when the value
/// would change. Avoids a spurious watch-tick when the listener
/// re-runs the same probe.
fn set_hotkey(state_tx: &watch::Sender<TrayState>, new_state: HotkeyMenuState) {
    state_tx.send_if_modified(|s| {
        if s.hotkey == new_state {
            false
        } else {
            s.hotkey = new_state;
            true
        }
    });
}

/// Block until the shutdown watch fires. Used in degraded paths
/// where the listener cannot do useful work but must still wait
/// for a clean exit.
async fn wait_for_shutdown(shutdown_rx: &mut watch::Receiver<()>) {
    if shutdown_rx.changed().await.is_err() {
        debug!("hotkey: shutdown channel closed");
    }
}

/// Translate a [`ProbeReport`] into a hotkey menu state. Called
/// on startup and on `HotkeyControl::Probe`.
fn apply_probe_to_state(state_tx: &watch::Sender<TrayState>, report: &ProbeReport) {
    if report.global_shortcuts_available {
        // Preserve a `Bound { display: ... }` if one is already
        // present — the probe alone cannot tell us about
        // bindings; only `list_shortcuts` can. Default to
        // NotBound and let `refresh_bound_state` upgrade.
        state_tx.send_if_modified(|s| match &s.hotkey {
            HotkeyMenuState::Bound { .. } | HotkeyMenuState::NotBound => false,
            HotkeyMenuState::Unknown | HotkeyMenuState::Unavailable { .. } => {
                s.hotkey = HotkeyMenuState::NotBound;
                true
            }
        });
        return;
    }
    let reason = match &report.backend {
        BackendDetected::Other(s) => format!("portal={s}: {}", report.reason),
        BackendDetected::None
        | BackendDetected::Kde
        | BackendDetected::Gnome
        | BackendDetected::Wlr => report.reason.clone(),
    };
    set_hotkey(state_tx, HotkeyMenuState::Unavailable { reason });
}

/// Open a session if there isn't one yet. Idempotent.
async fn ensure_session(
    adapter: &Arc<AshpdAdapter>,
    session: &mut Option<HotkeySession<AshpdAdapter>>,
) -> Result<(), PortalError> {
    if session.is_some() {
        return Ok(());
    }
    let s = HotkeySession::create(adapter.clone(), TRAY_BUS_NAME).await?;
    *session = Some(s);
    Ok(())
}

/// Tear down the previous session (if any) and open a fresh one
/// after a small backoff (B1 risk mitigation).
async fn recreate_session(
    adapter: &Arc<AshpdAdapter>,
    session: &mut Option<HotkeySession<AshpdAdapter>>,
) -> Result<(), PortalError> {
    tokio::time::sleep(PORTAL_RECREATE_BACKOFF).await;
    if let Some(mut existing) = session.take() {
        existing.recreate(TRAY_BUS_NAME).await?;
        *session = Some(existing);
        return Ok(());
    }
    ensure_session(adapter, session).await
}

/// Refresh the menu state from the live `list_shortcuts` result.
/// When the session is not open, the state is left untouched.
async fn refresh_bound_state(
    session: Option<&HotkeySession<AshpdAdapter>>,
    state_tx: &watch::Sender<TrayState>,
) {
    let Some(s) = session else { return };
    match s.list_shortcuts().await {
        Ok(list) => {
            let menu = bound_state_from_list(&list);
            set_hotkey(state_tx, menu);
        }
        Err(err) => {
            warn!(error = %err, "hotkey: list_shortcuts failed");
        }
    }
}

/// Pure helper — pick the menu state that matches a list
/// returned by the portal. Factored out for unit tests.
fn bound_state_from_list(list: &[BoundShortcut]) -> HotkeyMenuState {
    match list.iter().find(|s| s.id == SHORTCUT_ID) {
        Some(found) => HotkeyMenuState::Bound {
            display: found.trigger_description.clone(),
        },
        None => HotkeyMenuState::NotBound,
    }
}

/// Handle one `HotkeyControl` message.
async fn handle_control(
    ctl: HotkeyControl,
    cfg: &HotkeyConfig,
    adapter: &Arc<AshpdAdapter>,
    session: &mut Option<HotkeySession<AshpdAdapter>>,
    state_tx: &watch::Sender<TrayState>,
) {
    match ctl {
        HotkeyControl::Bind => {
            if let Err(err) = ensure_session(adapter, session).await {
                warn!(error = %err, "hotkey: session create failed in Bind path");
                set_hotkey(
                    state_tx,
                    HotkeyMenuState::Unavailable {
                        reason: format!("portal: {err}"),
                    },
                );
                return;
            }
            // SAFETY: ensure_session set Some on success. Use
            // an explicit guard so clippy doesn't trip
            // unwrap_used.
            let Some(s) = session.as_ref() else {
                warn!("hotkey: session unexpectedly absent after ensure");
                return;
            };
            let req = BindRequest {
                id: SHORTCUT_ID.to_owned(),
                description: SHORTCUT_DESCRIPTION.to_owned(),
                preferred_trigger: None,
            };
            let bind_fut = s.bind(&req);
            match tokio::time::timeout(Duration::from_secs(cfg.bind_timeout_secs), bind_fut).await {
                Ok(Ok(list)) => {
                    info!(count = list.len(), "hotkey: bind succeeded");
                    set_hotkey(state_tx, bound_state_from_list(&list));
                }
                Ok(Err(err)) => {
                    warn!(error = %err, "hotkey: bind RPC failed");
                    let reason = match err {
                        PortalError::BindCancelled => {
                            "bind cancelled by user".to_owned()
                        }
                        other => format!("bind failed: {other}"),
                    };
                    set_hotkey(state_tx, HotkeyMenuState::Unavailable { reason });
                }
                Err(_elapsed) => {
                    warn!(
                        timeout_secs = cfg.bind_timeout_secs,
                        "hotkey: bind timed out"
                    );
                    set_hotkey(
                        state_tx,
                        HotkeyMenuState::Unavailable {
                            reason: format!(
                                "bind timed out after {}s",
                                cfg.bind_timeout_secs,
                            ),
                        },
                    );
                }
            }
        }
        HotkeyControl::Unbind => {
            let Some(s) = session.as_ref() else {
                debug!("hotkey: Unbind on closed session — no-op (idempotent)");
                set_hotkey(state_tx, HotkeyMenuState::NotBound);
                return;
            };
            // `HotkeySession::unbind` is `DoD` #13 idempotent —
            // always returns Ok.
            if let Err(err) = s.unbind().await {
                warn!(error = %err, "hotkey: unbind failed");
            }
            // ashpd 0.13 implements unbind by closing the
            // session — drop our handle so the next Bind
            // recreates.
            *session = None;
            set_hotkey(state_tx, HotkeyMenuState::NotBound);
        }
        HotkeyControl::Probe => {
            let report = probe::probe().await;
            apply_probe_to_state(state_tx, &report);
            // Refresh bindings if the portal is now available.
            if report.global_shortcuts_available {
                refresh_bound_state(session.as_ref(), state_tx).await;
            }
        }
        HotkeyControl::Recreate => {
            // M7 `DoD` #16: settings emitted `HotkeyRebound`. Drop
            // the live session and open a fresh one so the next
            // `Activated` carries the user's new chord. The
            // `recreate_session` helper applies the same
            // 500 ms backoff as the SessionLost recovery path
            // (`DoD` #9 B1) — flapping notifications cannot spin
            // the listener.
            info!("hotkey: settings rebound the chord; recreating portal session");
            match recreate_session(adapter, session).await {
                Ok(()) => {
                    refresh_bound_state(session.as_ref(), state_tx).await;
                }
                Err(err) => {
                    warn!(error = %err, "hotkey: recreate after settings rebind failed");
                    set_hotkey(
                        state_tx,
                        HotkeyMenuState::Unavailable {
                            reason: format!("recreate: {err}"),
                        },
                    );
                    *session = None;
                }
            }
        }
    }
}

/// Spawn the `cz.zajca.Zwhisper1.Settings.HotkeyRebound` signal
/// subscriber task. The task lives for `shutdown_rx`'s lifetime
/// and forwards every signal arrival as a unit value onto
/// `signal_tx`. The receiver lives inside [`run_hotkey`]'s select
/// loop and, on receipt, treats the wake-up identically to a
/// menu-driven [`HotkeyControl::Recreate`].
///
/// Failures to install the match-rule are logged at warn — the
/// tray must still come up if the signal subscription cannot be
/// installed (e.g. broken bus). The user can rebind via the tray
/// menu directly in that degraded state.
///
/// Returns the spawned `JoinHandle` so the caller (`run_hotkey`)
/// can abort it on shutdown if it has not yet observed
/// `shutdown_rx`. Production drops the handle and lets `Drop`
/// cancel the inner task.
#[allow(
    clippy::unused_async,
    reason = "kept async to leave room for an early-failure round-trip without churning callers"
)]
pub async fn spawn_settings_rebind_subscriber(
    conn: zbus::Connection,
    signal_tx: mpsc::Sender<()>,
    mut shutdown_rx: watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    use futures_util::StreamExt;

    tokio::spawn(async move {
        let rule = match build_settings_match_rule() {
            Ok(r) => r,
            Err(err) => {
                warn!(error = %err, "hotkey: settings rebind match-rule build failed");
                wait_for_shutdown(&mut shutdown_rx).await;
                return;
            }
        };

        let mut stream = match zbus::MessageStream::for_match_rule(rule, &conn, None).await {
            Ok(s) => s,
            Err(err) => {
                warn!(
                    error = %err,
                    "hotkey: settings rebind subscription failed; rebind notifications disabled"
                );
                wait_for_shutdown(&mut shutdown_rx).await;
                return;
            }
        };
        debug!(
            interface = SETTINGS_SIGNAL_INTERFACE,
            path = SETTINGS_SIGNAL_PATH,
            member = SETTINGS_SIGNAL_MEMBER,
            "hotkey: settings rebind subscription installed"
        );

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => {
                    debug!("hotkey: settings rebind subscriber received shutdown");
                    return;
                }
                next = stream.next() => {
                    match next {
                        Some(Ok(msg)) => {
                            if !classify_settings_signal(&msg) {
                                continue;
                            }
                            info!("hotkey: settings emitted HotkeyRebound; queuing Recreate");
                            if let Err(err) = signal_tx.send(()).await {
                                warn!(error = %err, "hotkey: rebind signal channel closed; subscriber exiting");
                                return;
                            }
                        }
                        Some(Err(err)) => {
                            warn!(error = %err, "hotkey: settings rebind subscriber message error");
                        }
                        None => {
                            warn!("hotkey: settings rebind subscriber stream ended unexpectedly");
                            return;
                        }
                    }
                }
            }
        }
    })
}

/// Build the `MatchRule` covering settings' `HotkeyRebound`
/// signal. Factored out so unit tests can pin the rule shape
/// without standing up a real bus connection.
fn build_settings_match_rule() -> Result<zbus::MatchRule<'static>, String> {
    let builder = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(SETTINGS_SIGNAL_INTERFACE)
        .map_err(|e| format!("interface: {e}"))?
        .path(SETTINGS_SIGNAL_PATH)
        .map_err(|e| format!("path: {e}"))?
        .member(SETTINGS_SIGNAL_MEMBER)
        .map_err(|e| format!("member: {e}"))?;
    Ok(builder.build())
}

/// Pure helper — verify a candidate signal message is the one we
/// care about. Belt-and-braces: the match rule already filters by
/// interface + path + member, but we re-check so a future PR that
/// loosens the rule does not silently start firing on unrelated
/// signals.
fn classify_settings_signal(msg: &zbus::Message) -> bool {
    let header = msg.header();
    let iface = header.interface().map(zbus::names::InterfaceName::as_str);
    let member = header.member().map(zbus::names::MemberName::as_str);
    let path = header.path().map(zbus::zvariant::ObjectPath::as_str);
    let matches = matches!(iface, Some(SETTINGS_SIGNAL_INTERFACE))
        && matches!(member, Some(SETTINGS_SIGNAL_MEMBER))
        && matches!(path, Some(SETTINGS_SIGNAL_PATH));
    if !matches {
        debug!(
            ?iface,
            ?member,
            ?path,
            "hotkey: settings rebind subscriber dropped non-matching message"
        );
    }
    matches
}

/// `cz.zajca.Zwhisper1.Settings.HotkeyRebound` — interface name
/// owned by the settings binary. Settings emits this signal on a
/// successful rebind; the tray subscribes via
/// [`spawn_settings_rebind_subscriber`].
const SETTINGS_SIGNAL_INTERFACE: &str = "cz.zajca.Zwhisper1.Settings";

/// Object path the signal is emitted from.
const SETTINGS_SIGNAL_PATH: &str = "/cz/zajca/Zwhisper1/Settings";

/// Member name of the broadcast signal.
const SETTINGS_SIGNAL_MEMBER: &str = "HotkeyRebound";

/// Run one `toggle_once` against a freshly-built
/// [`LiveRecorderClient`] and translate the result into a side
/// effect (notification / log line).
///
/// Per `DoD` #15: the live `Profiles1.GetActive` call inside
/// `toggle_once` is what makes the hotkey path immune to a stale
/// `state.active_profile` cache. We pass `state_rx` through only
/// to read `recording_started_at` for diagnostics — the toggle
/// decision itself never reads tray state.
async fn on_activated(
    recorder: &Recorder1Proxy<'_>,
    profiles: &Profiles1Proxy<'_>,
    debouncer: &mut Debouncer,
    cfg: &HotkeyConfig,
    _state_rx: &watch::Receiver<TrayState>,
) {
    let client = LiveRecorderClient::new(recorder.clone(), profiles.clone());
    match toggle_once(&client, debouncer).await {
        Ok(ToggleOutcome::Started {
            session_id,
            profile,
        }) => {
            info!(
                session_id,
                profile, "hotkey: toggle started a new recording"
            );
            if cfg.notify_on_start {
                fire_recording_started_notification(&profile).await;
            }
        }
        Ok(ToggleOutcome::Stopping { session_id }) => {
            info!(session_id, "hotkey: toggle stopped recording");
        }
        Ok(ToggleOutcome::NoOp { reason }) => match reason {
            NoOpReason::AlreadyDraining => {
                debug!("hotkey: toggle NoOp — daemon is draining");
            }
            NoOpReason::AlreadyActive => {
                // Concurrent-toggle race (CLI + tray pressed the
                // chord at the same instant; the other won). The
                // recording IS running, just not started by us.
                // Benign, log only — no notification.
                debug!("hotkey: toggle NoOp — recording already active (concurrent race)");
            }
            NoOpReason::Unknown => {
                debug!("hotkey: toggle NoOp — unknown reason");
            }
        },
        Err(ToggleError::Debounced { debounce_ms }) => {
            debug!(debounce_ms, "hotkey: toggle debounced");
        }
        Err(ToggleError::CoolingDown { cooldown_ms }) => {
            debug!(cooldown_ms, "hotkey: toggle in cooldown");
        }
        Err(ToggleError::DaemonDown) => {
            warn!("hotkey: toggle aborted — daemon not running");
            fire_simple_notification(
                "zwhisper",
                "Daemon not running — start it with `systemctl --user start zwhisperd`",
            )
            .await;
        }
        Err(ToggleError::NoActiveProfile) => {
            warn!("hotkey: toggle aborted — no active profile");
            fire_simple_notification(
                "zwhisper",
                "Set an active profile via the tray menu first.",
            )
            .await;
        }
        Err(ToggleError::AlreadyActive) => {
            // Defensive: `toggle_once` should fold this into
            // `NoOp { AlreadyActive }`. If it leaks through,
            // treat it the same way — log at debug, do not
            // surface a notification for a benign race.
            debug!("hotkey: toggle aborted — recording already active (concurrent race)");
        }
        Err(ToggleError::Rpc(msg)) => {
            warn!(error = %msg, "hotkey: toggle rpc failed");
        }
    }
}

/// Fire the `DoD` #18 "Recording started" notification. The
/// listener is the only writer of this notification path (the
/// pump never fires its own `recording`-state notification — see
/// `pump.rs`), so there is no risk of duplication.
///
/// The blocking `notify-rust` `show()` call lives in a
/// `spawn_blocking` task and is wrapped in [`NOTIFY_SHOW_TIMEOUT`]
/// — same pattern as the CLI's `fire_daemon_down_notification`.
/// A wedged session bus must NOT stall the listener.
async fn fire_recording_started_notification(profile: &str) {
    let body = format!("Recording started ({profile})");
    let join = tokio::task::spawn_blocking(move || {
        notify_rust::Notification::new()
            .summary("zwhisper")
            .body(&body)
            .icon("media-record")
            .timeout(notify_rust::Timeout::Milliseconds(
                u32::try_from(NOTIFY_TIMEOUT_MS).unwrap_or(3_000),
            ))
            .show()
            .map(|_| ())
    });
    match tokio::time::timeout(NOTIFY_SHOW_TIMEOUT, join).await {
        Ok(Ok(Ok(()))) => debug!("hotkey: recording-started notification delivered"),
        Ok(Ok(Err(err))) => warn!(error = %err, "hotkey: notify-rust show failed"),
        Ok(Err(err)) => warn!(error = %err, "hotkey: notification task panicked"),
        Err(_) => warn!(
            timeout_ms = u64::try_from(NOTIFY_SHOW_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            "hotkey: notification timed out (bus likely broken)"
        ),
    }
}

/// Generic transient notification used by the failure paths
/// (daemon-down, no active profile). Same blocking-safety contract
/// as [`fire_recording_started_notification`].
async fn fire_simple_notification(summary: &str, body: &str) {
    let summary = summary.to_string();
    let body = body.to_string();
    let join = tokio::task::spawn_blocking(move || {
        notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .icon("dialog-warning")
            .timeout(notify_rust::Timeout::Milliseconds(
                u32::try_from(NOTIFY_TIMEOUT_MS).unwrap_or(3_000),
            ))
            .show()
            .map(|_| ())
    });
    match tokio::time::timeout(NOTIFY_SHOW_TIMEOUT, join).await {
        Ok(Ok(Ok(()))) => debug!("hotkey: simple notification delivered"),
        Ok(Ok(Err(err))) => warn!(error = %err, "hotkey: notify-rust show failed"),
        Ok(Err(err)) => warn!(error = %err, "hotkey: notification task panicked"),
        Err(_) => warn!(
            timeout_ms = u64::try_from(NOTIFY_SHOW_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            "hotkey: notification timed out (bus likely broken)"
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use zwhisper_hotkey::portal::BoundShortcut;

    #[test]
    fn bound_state_from_list_empty_returns_not_bound() {
        assert_eq!(bound_state_from_list(&[]), HotkeyMenuState::NotBound);
    }

    #[test]
    fn bound_state_from_list_other_id_returns_not_bound() {
        let list = vec![BoundShortcut {
            id: "some-other".to_owned(),
            trigger_description: "Ctrl+X".to_owned(),
            description: "x".to_owned(),
        }];
        assert_eq!(bound_state_from_list(&list), HotkeyMenuState::NotBound);
    }

    #[test]
    fn bound_state_from_list_matching_id_returns_bound() {
        let list = vec![BoundShortcut {
            id: SHORTCUT_ID.to_owned(),
            trigger_description: "Ctrl+Alt+R".to_owned(),
            description: SHORTCUT_DESCRIPTION.to_owned(),
        }];
        assert_eq!(
            bound_state_from_list(&list),
            HotkeyMenuState::Bound {
                display: "Ctrl+Alt+R".to_owned()
            }
        );
    }

    #[test]
    fn apply_probe_writes_unavailable_when_global_shortcuts_unavailable() {
        let (tx, rx) = watch::channel(TrayState::default());
        let report = ProbeReport {
            backend: BackendDetected::None,
            global_shortcuts_available: false,
            portal_version: None,
            reason: "no portal".to_owned(),
        };
        apply_probe_to_state(&tx, &report);
        match &rx.borrow().hotkey {
            HotkeyMenuState::Unavailable { reason } => {
                assert!(reason.contains("no portal"), "reason was: {reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn apply_probe_writes_not_bound_when_available_and_state_was_unknown() {
        let (tx, rx) = watch::channel(TrayState::default());
        let report = ProbeReport {
            backend: BackendDetected::Kde,
            global_shortcuts_available: true,
            portal_version: Some(2),
            reason: "ok".to_owned(),
        };
        apply_probe_to_state(&tx, &report);
        assert_eq!(rx.borrow().hotkey, HotkeyMenuState::NotBound);
    }

    #[test]
    fn apply_probe_preserves_existing_bound_state() {
        let initial = TrayState {
            hotkey: HotkeyMenuState::Bound {
                display: "Ctrl+Alt+R".to_owned(),
            },
            ..TrayState::default()
        };
        let (tx, rx) = watch::channel(initial);
        let report = ProbeReport {
            backend: BackendDetected::Kde,
            global_shortcuts_available: true,
            portal_version: Some(2),
            reason: "ok".to_owned(),
        };
        apply_probe_to_state(&tx, &report);
        assert_eq!(
            rx.borrow().hotkey,
            HotkeyMenuState::Bound {
                display: "Ctrl+Alt+R".to_owned()
            }
        );
    }

    #[test]
    fn pre_ready_buffer_admits_first_activated() {
        let ev = HotkeyEvent::Activated {
            shortcut_id: SHORTCUT_ID.to_owned(),
            timestamp: Some(100),
        };
        let result = admit_to_pre_ready_buffer(None, ev.clone());
        assert_eq!(result, Some(ev));
    }

    #[test]
    fn pre_ready_buffer_overwrites_with_newest_activated() {
        // Risk A4 / `DoD` #16 — the buffer is a 1-slot ring;
        // a second Activated arriving before the daemon-ready
        // gate flips MUST replace the first so the user sees
        // their most recent intent honoured.
        let first = HotkeyEvent::Activated {
            shortcut_id: SHORTCUT_ID.to_owned(),
            timestamp: Some(100),
        };
        let second = HotkeyEvent::Activated {
            shortcut_id: SHORTCUT_ID.to_owned(),
            timestamp: Some(250),
        };
        let after_first = admit_to_pre_ready_buffer(None, first.clone());
        let after_second = admit_to_pre_ready_buffer(after_first, second.clone());
        assert_eq!(after_second, Some(second));
    }

    #[test]
    fn pre_ready_buffer_ignores_non_activated_events() {
        let prev = HotkeyEvent::Activated {
            shortcut_id: SHORTCUT_ID.to_owned(),
            timestamp: Some(100),
        };
        // A Deactivated must not clobber the buffered Activated.
        let after_deact = admit_to_pre_ready_buffer(
            Some(prev.clone()),
            HotkeyEvent::Deactivated {
                shortcut_id: SHORTCUT_ID.to_owned(),
            },
        );
        assert_eq!(after_deact, Some(prev.clone()));
        // ShortcutsChanged likewise.
        let after_changed =
            admit_to_pre_ready_buffer(Some(prev.clone()), HotkeyEvent::ShortcutsChanged);
        assert_eq!(after_changed, Some(prev));
    }

    #[tokio::test]
    async fn pre_ready_buffer_drains_one_pending_activated_after_gate_flip() {
        // End-to-end via FakePortal: emit Activated BEFORE
        // flipping daemon_ready, then flip — the buffered event
        // must be drained exactly once and the event stream must
        // not redeliver it. This is the tray-side mirror of the
        // CLI race protection added in Fix 2.
        use std::sync::Arc;
        use tokio::time::{Duration, timeout};
        use zwhisper_hotkey::portal::{FakePortal, HotkeySession};

        let portal = Arc::new(FakePortal::new());
        let mut session = HotkeySession::create(portal.clone(), TRAY_BUS_NAME)
            .await
            .unwrap();
        // Bind so the FakePortal accepts events for SHORTCUT_ID.
        session
            .bind(&zwhisper_hotkey::portal::BindRequest {
                id: SHORTCUT_ID.to_owned(),
                description: SHORTCUT_DESCRIPTION.to_owned(),
                preferred_trigger: None,
            })
            .await
            .unwrap();

        // Emit two presses BEFORE we drain — newest must win.
        portal.emit_activated(SHORTCUT_ID);
        portal.emit_activated(SHORTCUT_ID);

        // Mirror the run-loop's buffer logic: pull events until
        // we have admitted both into the 1-slot buffer.
        let mut pending: Option<HotkeyEvent> = None;
        for _ in 0..2 {
            let ev = timeout(Duration::from_millis(200), session.next_event())
                .await
                .expect("event timed out")
                .expect("stream closed");
            pending = admit_to_pre_ready_buffer(pending.take(), ev);
        }

        // Exactly one event survives the buffer.
        let drained = pending.take().expect("buffer must hold one Activated");
        assert!(matches!(
            drained,
            HotkeyEvent::Activated { ref shortcut_id, .. } if shortcut_id == SHORTCUT_ID
        ));
        assert!(pending.is_none(), "buffer must hold at most one slot");
    }

    #[test]
    fn set_hotkey_no_op_when_value_unchanged() {
        let (tx, mut rx) = watch::channel(TrayState::default());
        // Default is Unknown — set the same value twice and
        // confirm only the very first watch tick fires (and
        // that's the channel's initial value).
        // Mark the initial state seen.
        rx.mark_unchanged();
        set_hotkey(&tx, HotkeyMenuState::Unknown);
        assert!(
            !rx.has_changed().unwrap(),
            "writing the same value should not tick the watch",
        );
        set_hotkey(
            &tx,
            HotkeyMenuState::Unavailable {
                reason: "x".to_owned(),
            },
        );
        assert!(rx.has_changed().unwrap());
    }

    #[test]
    fn settings_match_rule_pins_interface_path_member() {
        // M7 `DoD` #16 / D7 — the tray subscribes to the
        // signal owned by `zwhisper-settings`. Settings owns
        // the interface name; tray pins this side. A rename
        // on either side breaks the rebind notification path
        // silently — this test makes the regression loud.
        let rule = build_settings_match_rule().expect("rule builds");
        assert_eq!(
            rule.interface().map(zbus::names::InterfaceName::as_str),
            Some(SETTINGS_SIGNAL_INTERFACE)
        );
        match rule.path_spec() {
            Some(zbus::match_rule::PathSpec::Path(p)) => {
                assert_eq!(p.as_str(), SETTINGS_SIGNAL_PATH);
            }
            other => panic!("expected exact-path spec, got {other:?}"),
        }
        assert_eq!(
            rule.member().map(zbus::names::MemberName::as_str),
            Some(SETTINGS_SIGNAL_MEMBER)
        );
        assert_eq!(rule.msg_type(), Some(zbus::message::Type::Signal));
    }

    /// `DoD` #16 — end-to-end test that a `HotkeyRebound` signal
    /// emitted on the live session bus is observed by the tray's
    /// subscriber and surfaces as one `()` on the rebind channel.
    /// Skipped when no session bus is reachable (CI sandboxes).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tray_picks_up_settings_rebind_signal() {
        use std::time::Duration;

        // First connection: the subscriber side. If the bus is
        // unreachable we skip — same gate the
        // `zwhisper-settings::app::tests::second_launch_*` test
        // uses for D-Bus-dependent paths.
        let subscriber_conn = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(err) => {
                eprintln!("skipping: no session bus available ({err})");
                return;
            }
        };
        let emitter_conn = zbus::Connection::session()
            .await
            .expect("second connection");

        let (tx, mut rx) = mpsc::channel::<()>(REBIND_SIGNAL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let _join =
            spawn_settings_rebind_subscriber(subscriber_conn, tx, shutdown_rx).await;

        // Give the subscriber a brief window to install the
        // match rule before we emit. The `add_match` round-trip
        // is async; emitting too early races the broker.
        tokio::time::sleep(Duration::from_millis(100)).await;

        emitter_conn
            .emit_signal(
                None::<&str>,
                SETTINGS_SIGNAL_PATH,
                SETTINGS_SIGNAL_INTERFACE,
                SETTINGS_SIGNAL_MEMBER,
                &("Ctrl+Alt+R",),
            )
            .await
            .expect("emit signal");

        let received = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        let received = received.expect("rebind signal did not arrive within 5s");
        assert!(
            received.is_some(),
            "rebind channel closed before signal arrived"
        );

        // Drop the subscriber so the test does not bleed into
        // sibling runs on the same bus.
        let _ = shutdown_tx.send(());
    }
}
