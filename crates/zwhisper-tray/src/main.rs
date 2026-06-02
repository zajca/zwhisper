//! `zwhisper-tray` — M4 milestone entry point.
//!
//! See `docs/M4-plan.md` for the milestone scope. This binary is a
//! D-Bus client of the M3 daemon (`cz.zajca.Zwhisper1`); it never
//! depends on `zwhisperd` directly.
//!
//! Phase P3 wires the four core tasks:
//!
//! - **Pump (Task B)**: subscribes to D-Bus signals, owns the
//!   single writer of the `watch::Sender<TrayState>`.
//! - **Tray (Task C)**: implements `ksni::Tray`, registered on the
//!   session bus via `ksni::TrayMethods::spawn`.
//! - **Supervisor (Task D)**: forwards new `TrayState` snapshots to
//!   the ksni service, exit(1)s if ksni dies (M4-plan C3 contract).
//! - **Quit watcher (Task E, P3)**: maps the Quit menu item /
//!   Ctrl-C onto the shared shutdown channel.
//!
//! ## Concurrency-3 (ksni liveness)
//!
//! ksni 0.3 is async and runs as a tokio task on our runtime.
//! `Handle::update` returning `None` is the only liveness signal we
//! get; the supervisor polls liveness implicitly through every state
//! propagation. See `crate::supervisor` for the full contract.
//!
//! ## P4: command dispatcher
//!
//! Menu callbacks `try_send` [`PendingCmd`] values onto an mpsc
//! channel. The dispatcher (see [`zwhisper_tray::cmd::run_dispatcher`])
//! consumes them, sets `pending_cmd` for the optimistic action lock,
//! and fires the matching RPC against the daemon.
//!
//! The dispatcher runs on its own `zbus::Connection` separate from
//! the pump's. Two concurrent session-bus connections are cheap and
//! fully supported by zbus 5.15; sharing the pump's connection would
//! require refactoring `pump.rs` to expose its connection handle and
//! is left as a future cleanup. The dispatcher only writes to the
//! shared `watch::Sender<TrayState>` — the pump remains the single
//! source of truth for daemon-driven state changes.
//!
//! When the session bus is unreachable at startup, the tray falls
//! back into "offline mode": a draining task consumes commands so
//! the bounded mpsc buffer never fills, and every command is logged
//! and dropped.

use std::path::PathBuf;

use color_eyre::eyre::Result;
use ksni::TrayMethods;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use zwhisper_hotkey::config::HotkeyConfig;

use zwhisper_tray::cmd::run_dispatcher;
use zwhisper_tray::config::{
    COMMAND_CHANNEL_CAPACITY, Config, SINK_CHANNEL_CAPACITY, tray_registration_retry_delay,
};
use zwhisper_tray::dbus::connect_session;
use zwhisper_tray::hotkey::{HotkeyControl, run_hotkey};
use zwhisper_tray::pump::run_pump;
use zwhisper_tray::session_env::{SessionProbe, probe as session_probe};
use zwhisper_tray::single_instance;
use zwhisper_tray::sink::clipboard::ClipboardSink;
use zwhisper_tray::sink::dispatch::{TranscriptJob, run_dispatcher as run_sink_dispatcher};
use zwhisper_tray::sink::notification::NotificationSink;
use zwhisper_tray::state::{PendingCmd, TrayState};
use zwhisper_tray::supervisor::run_supervisor;
use zwhisper_tray::tray::ZwhisperTray;

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    color_eyre::install()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        "zwhisper-tray starting"
    );

    // P6: refuse to run without a graphical session. The systemd
    // unit uses `After=graphical-session.target` (not `Requisite=`)
    // so the service starts even on wlroots setups that never
    // activate the target — this check turns that into a clean
    // user-visible exit instead of a useless tray process.
    if matches!(session_probe(), SessionProbe::Unavailable) {
        error!("WAYLAND_DISPLAY is not set; zwhisper-tray needs a Wayland session — exiting",);
        return Err(color_eyre::eyre::eyre!(
            "no Wayland session: WAYLAND_DISPLAY is unset",
        ));
    }

    // Channels: the watch carries the state snapshot from the pump
    // to the supervisor; two clones of `shutdown_rx` go to the two
    // long-running tasks; mpsc carries menu commands and the Quit
    // signal to the dispatcher / quit-watcher respectively.
    let (state_tx, state_rx) = watch::channel(TrayState::default());
    let (shutdown_tx, shutdown_rx_supervisor) = watch::channel(());
    let shutdown_rx_pump = shutdown_rx_supervisor.clone();
    let shutdown_rx_dispatcher = shutdown_rx_supervisor.clone();
    let shutdown_rx_sinks = shutdown_rx_supervisor.clone();

    let (cmd_tx, cmd_rx) = mpsc::channel::<PendingCmd>(COMMAND_CHANNEL_CAPACITY);
    let (sink_tx, sink_rx) = mpsc::channel::<TranscriptJob>(SINK_CHANNEL_CAPACITY);
    let (quit_tx, mut quit_rx) = mpsc::channel::<()>(1);
    // M6: hotkey listener channels.
    //
    // - `hotkey_ctl_*` carries Bind / Unbind / Probe requests
    //   from the menu callbacks to the listener task.
    // - `daemon_ready_*` is the A4 mitigation gate: the listener
    //   refuses to talk to the portal or the daemon until this
    //   watch flips to `true`, which happens once we have a
    //   working `zbus::Connection` to the session bus.
    let (hotkey_ctl_tx, hotkey_ctl_rx) = mpsc::channel::<HotkeyControl>(8);
    let (daemon_ready_tx, daemon_ready_rx) = watch::channel(false);
    let shutdown_rx_hotkey = shutdown_rx_supervisor.clone();

    // Sink dispatcher (P5). Owns the clipboard handle for the
    // tray's lifetime (binding amendment C1) and runs the
    // notification sink. The clipboard size guard reads
    // `ZWHISPER_TRAY_CLIPBOARD_MAX_BYTES` (per `Config::from_env`);
    // invalid values fall back to the documented default with a warn.
    let cfg = Config::from_env();
    info!(
        clipboard_max_bytes = cfg.clipboard_max_bytes,
        "tray runtime config loaded"
    );
    let clipboard_sink = ClipboardSink::new();
    let notification_sink = NotificationSink::new();
    let sink_join = tokio::spawn(async move {
        if let Err(e) = run_sink_dispatcher(
            sink_rx,
            clipboard_sink,
            notification_sink,
            cfg.clipboard_max_bytes,
            shutdown_rx_sinks,
        )
        .await
        {
            error!(error = %e, "sink dispatcher ended with error");
        }
    });

    // Command dispatcher (P4). Owns its own session-bus connection
    // separate from the pump's. If the session bus is unreachable,
    // fall back to a degraded-mode draining task so menu try_send
    // calls never fill the bounded mpsc buffer.
    let dispatcher_state_tx = state_tx.clone();
    let dispatcher_state_rx = state_rx.clone();
    // M6: the hotkey listener wants the connection too — clone
    // it so both tasks can build their own proxies without
    // contending on a shared one. zbus 5.15 connections are
    // cheap reference-counted handles.
    let mut hotkey_conn: Option<zbus::Connection> = None;
    // We MUST spawn the hotkey listener BEFORE the dispatcher so
    // its pre-ready Activated buffer is already running when the
    // dispatcher's `GetStatus` probe flips `daemon_ready_tx`. The
    // dispatcher connection is acquired here, but its task spawn
    // is deferred until after `run_hotkey` is on the runtime
    // (DoD #16, A4 mitigation).
    let dispatcher_conn = match connect_session().await {
        Ok(conn) => {
            // P6 single-instance: claim cz.zajca.Zwhisper1.Tray on
            // the same connection that the dispatcher uses; the
            // name is held for the connection's lifetime. A second
            // tray process would observe `Exists` and exit 0.
            match single_instance::claim(&conn).await {
                Ok(true) => {
                    info!(
                        name = single_instance::TRAY_BUS_NAME,
                        "single-instance lock acquired"
                    );
                }
                Ok(false) => {
                    info!(
                        name = single_instance::TRAY_BUS_NAME,
                        "another zwhisper-tray instance is already running; exiting cleanly",
                    );
                    return Ok(());
                }
                Err(e) => {
                    // Soft failure: a transient bus glitch should
                    // not kill the process. Log and continue without
                    // single-instance enforcement; if a second
                    // instance shows up later both will run, which
                    // is at worst a duplicate icon.
                    warn!(error = %e, "could not claim single-instance bus name; continuing without it");
                }
            }
            // The connection is healthy enough to claim a name —
            // hand a clone to the hotkey listener. The
            // `daemon_ready_tx` gate stays `false` for now; the
            // dispatcher flips it after its `GetStatus` probe
            // succeeds (see `run_dispatcher`).
            hotkey_conn = Some(conn.clone());
            Some(conn)
        }
        Err(err) => {
            warn!(error = %err, "no session bus; menu commands disabled");
            None
        }
    };

    // M6 hotkey listener — owns its own pair of proxies on
    // `hotkey_conn`. When the session bus was unreachable above,
    // `hotkey_conn` is `None` and we skip the spawn entirely.
    //
    // Spawned BEFORE the dispatcher: the listener must be inside
    // its pre-ready buffer loop before the dispatcher's probe
    // flips `daemon_ready_tx`, otherwise the 1-slot Activated
    // buffer can never fire (the gate would already be `true`
    // when the listener finally observes it).
    let hotkey_join = if let Some(conn) = hotkey_conn {
        let hotkey_cfg_path = hotkey_config_path();
        let hotkey_cfg = HotkeyConfig::from_path(&hotkey_cfg_path);
        info!(
            path = %hotkey_cfg_path.display(),
            debounce_ms = hotkey_cfg.debounce_ms,
            cooldown_ms = hotkey_cfg.cooldown_ms,
            auto_bind_on_startup = hotkey_cfg.auto_bind_on_startup,
            "hotkey config loaded"
        );
        let hotkey_state_tx = state_tx.clone();
        let hotkey_state_rx = state_rx.clone();
        Some(tokio::spawn(async move {
            if let Err(err) = run_hotkey(
                conn,
                hotkey_cfg,
                hotkey_ctl_rx,
                hotkey_state_tx,
                hotkey_state_rx,
                daemon_ready_rx,
                shutdown_rx_hotkey,
            )
            .await
            {
                error!(error = %err, "hotkey listener task ended with error");
            }
        }))
    } else {
        // No bus → no listener. Drain the control channel so menu
        // try_send calls never wedge a bounded mpsc.
        Some(tokio::spawn(drain_hotkey_offline(hotkey_ctl_rx)))
    };

    // Now that `run_hotkey` is on the runtime (and inside its
    // pre-ready buffer loop), spawn the dispatcher. It will
    // build its proxies, probe `GetStatus`, and only then flip
    // `daemon_ready_tx` so the listener drains any buffered
    // Activated press.
    let cmd_consumer = if let Some(conn) = dispatcher_conn {
        let dispatcher_daemon_ready_tx = daemon_ready_tx.clone();
        // M8 follow-up: dispatcher needs to broadcast workspace
        // shutdown when its own protocol-version handshake fails,
        // because it owns a separate `zbus::Connection` from the
        // pump and the hotkey listener.
        let dispatcher_shutdown_broadcast = shutdown_tx.clone();
        tokio::spawn(async move {
            if let Err(err) = run_dispatcher(
                conn,
                cmd_rx,
                dispatcher_state_tx,
                dispatcher_state_rx,
                dispatcher_daemon_ready_tx,
                dispatcher_shutdown_broadcast,
                shutdown_rx_dispatcher,
            )
            .await
            {
                error!(error = %err, "dispatcher task ended with error");
            }
        })
    } else {
        tokio::spawn(drain_commands_offline(cmd_rx))
    };

    // Pump task — owns the single writer of `state_tx`. Also
    // produces sink jobs on `sink_tx` whenever a `TranscriptComplete`
    // signal arrives (P5). It starts before SNI registration so
    // notifications and state tracking still work while a Wayland
    // tray host such as Waybar is starting.
    let pump_sink_tx = sink_tx.clone();
    // M8 follow-up: pump needs the shutdown broadcaster so a
    // protocol-version mismatch tears down the whole tray (not
    // just the pump's own loop). See `pump::run_pump` rustdoc.
    let pump_shutdown_broadcast = shutdown_tx.clone();
    let pump_join = tokio::spawn(async move {
        if let Err(e) = run_pump(
            state_tx,
            pump_sink_tx,
            pump_shutdown_broadcast,
            shutdown_rx_pump,
        )
        .await
        {
            error!(error = %e, "pump task ended with error");
        }
    });

    // Build the tray and register it on the session bus. `spawn`
    // returns once the tray is registered with the desktop's
    // `StatusNotifierWatcher`. Missing watcher/host is retryable:
    // Sway, Hyprland, and other compositor setups commonly start
    // the watcher via an external process (`waybar`, a panel) after
    // this user service is already up.
    let handle =
        spawn_tray_with_retry(cmd_tx.clone(), quit_tx.clone(), hotkey_ctl_tx.clone()).await?;

    // Supervisor task — forwards state snapshots to ksni and exits 1
    // if ksni dies (M4-plan C3).
    let supervisor_join = tokio::spawn(async move {
        if let Err(e) = run_supervisor(handle, state_rx, shutdown_rx_supervisor).await {
            error!(error = %e, "supervisor task ended with error");
        }
    });

    // Quit watcher — the Quit menu item or any future "user
    // requested shutdown" path drops a `()` into `quit_rx`.
    let shutdown_tx_quit = shutdown_tx.clone();
    let quit_join = tokio::spawn(async move {
        if quit_rx.recv().await.is_some() {
            info!("Quit menu item activated; shutting down");
            let _ = shutdown_tx_quit.send(());
        }
    });

    // Ctrl-C handler — same shutdown path, different trigger.
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!(error = %e, "failed to install ctrl-c handler");
    } else {
        info!("ctrl-c received, shutting down");
    }

    // Broadcast shutdown to both long-running tasks.
    let _ = shutdown_tx.send(());
    drop(shutdown_tx);

    // Wait for the supervisor first (it owns the ksni handle).
    if let Err(e) = supervisor_join.await {
        error!(error = %e, "supervisor task join failed");
    }
    if let Err(e) = pump_join.await {
        error!(error = %e, "pump task join failed");
    }
    quit_join.abort();
    let _ = quit_join.await;
    cmd_consumer.abort();
    let _ = cmd_consumer.await;
    if let Some(h) = hotkey_join {
        h.abort();
        let _ = h.await;
    }
    // Drop the producer side first so the sink dispatcher can
    // observe a clean channel close after draining in-flight jobs.
    drop(sink_tx);
    if let Err(e) = sink_join.await {
        error!(error = %e, "sink dispatcher join failed");
    }

    info!("zwhisper-tray exiting cleanly");
    Ok(())
}

/// Degraded-mode command consumer. Used when the session bus is
/// unreachable at startup so menu activations have somewhere to go
/// (a full mpsc would silently drop, but a `try_send` against a
/// receiver-less channel returns `Err` immediately and would spam
/// the log).
async fn drain_commands_offline(mut rx: mpsc::Receiver<PendingCmd>) {
    while let Some(cmd) = rx.recv().await {
        warn!(?cmd, "dropping menu command (no D-Bus connection)");
    }
}

/// M6 — hotkey-control consumer for the offline path. The same
/// rationale as `drain_commands_offline` applies: a bounded mpsc
/// with no consumer would `try_send`-fail and spam the log; a
/// dedicated drain task absorbs them silently.
async fn drain_hotkey_offline(mut rx: mpsc::Receiver<HotkeyControl>) {
    while let Some(ctl) = rx.recv().await {
        warn!(?ctl, "dropping hotkey control (no D-Bus connection)");
    }
}

/// Register the SNI tray item, retrying when the desktop tray host
/// is not ready yet.
async fn spawn_tray_with_retry(
    cmd_tx: mpsc::Sender<PendingCmd>,
    quit_tx: mpsc::Sender<()>,
    hotkey_ctl_tx: mpsc::Sender<HotkeyControl>,
) -> Result<ksni::Handle<ZwhisperTray>> {
    let mut attempt = 0usize;

    loop {
        let tray = ZwhisperTray::new(cmd_tx.clone(), quit_tx.clone(), hotkey_ctl_tx.clone());
        match tray.spawn().await {
            Ok(handle) => {
                info!(
                    attempts = attempt + 1,
                    "ksni tray registered with StatusNotifierWatcher"
                );
                return Ok(handle);
            }
            Err(err) => {
                let diag = zwhisper_tray::tray_diag::diagnose(&err);
                let retryable =
                    zwhisper_tray::tray_diag::registration_failure_is_retryable(diag.category);
                if attempt == 0 || !retryable {
                    error!(
                        category = ?diag.category,
                        error = %err,
                        "{}",
                        diag.summary,
                    );
                    for step in diag.next_steps {
                        error!("  - {}", step);
                    }
                }
                if !retryable {
                    return Err(color_eyre::eyre::eyre!(
                        "tray registration failed: {} ({err})",
                        diag.summary,
                    ));
                }

                let delay = tray_registration_retry_delay(attempt);
                warn!(
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis(),
                    category = ?diag.category,
                    "tray host unavailable; retrying registration"
                );
                tokio::time::sleep(delay).await;
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

/// Resolve `~/.config/zwhisper/hotkey.toml`. Falls back to the
/// current directory when `dirs::config_dir()` returns `None`
/// (sandboxed test runners) — the resulting path almost certainly
/// won't exist, which is the documented "missing file is fine"
/// path in [`HotkeyConfig::from_path`].
fn hotkey_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("zwhisper")
        .join("hotkey.toml")
}
