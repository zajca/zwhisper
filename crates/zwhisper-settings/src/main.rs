//! M7 — `zwhisper-settings` binary entry point.
//!
//! Boot order:
//! 1. `color_eyre` for human-readable panic / error reports.
//! 2. `tracing_subscriber` reading `RUST_LOG`.
//! 3. Spawn the side-thread tokio runtime + `UiBridge`.
//! 4. Claim `cz.zajca.Zwhisper1.Settings` on the session bus
//!    (M7-plan § 17). On collision we exit 0 (a follow-up MV
//!    will add the `Raise` request — `DoD` #17 second clause).
//! 5. Build [`App`] and call `App::run`.
//! 6. After `Fl::run` returns, cancel the runtime token and let
//!    the `Runtime` drop trigger `shutdown_background()`.
//!
//! Linux-only — the workspace is gated to Linux, mirroring the
//! daemon and tray.

#![cfg(target_os = "linux")]

use std::time::Duration;

use color_eyre::eyre::WrapErr;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

mod app;
mod checksums;
mod client;
mod config;
mod download;
mod error;
mod hotkey_signal;
mod runtime;
mod tabs;

use crate::app::App;
use crate::runtime::spawn_runtime;

/// Best-effort grace period for in-flight downloader tasks to
/// observe the cancel token before the runtime is force-dropped.
/// Matches M7-plan § 2.4.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        "zwhisper-settings starting"
    );

    let (bridge, runtime, rx, cancel_token) =
        spawn_runtime().wrap_err("failed to spawn settings runtime")?;

    // Single-instance gate — block on the dedicated runtime so we
    // do not need a separate `tokio::main`. Three outcomes:
    //   - Acquired: keep the connection for the process lifetime.
    //   - AlreadyRunning: emit the Raise signal so the alive
    //     instance brings its window forward, then exit 0.
    //   - Err: a real D-Bus error (no session bus, invalid name,
    //     RPC failure) — bail so the tray surfaces the problem
    //     instead of silently doing nothing.
    let bus_conn = match runtime.block_on(app::try_acquire_single_instance()) {
        Ok(app::SingleInstanceOutcome::Acquired(conn)) => Some(conn),
        Ok(app::SingleInstanceOutcome::AlreadyRunning(conn)) => {
            info!(
                "another zwhisper-settings instance owns {} — sending Raise signal",
                app::SETTINGS_BUS_NAME,
            );
            if let Err(e) = runtime.block_on(app::emit_raise_signal(&conn)) {
                warn!(error = %e, "failed to send Raise signal to existing instance");
            }
            // Drop the connection so the alive instance's
            // subscriber observes our send before we exit.
            drop(conn);
            return Ok(());
        }
        Err(e) => {
            error!(
                error = %e,
                bus_name = app::SETTINGS_BUS_NAME,
                "session-bus unavailable; cannot enforce single-instance",
            );
            return Err(e).wrap_err("single-instance bus claim failed");
        }
    };

    let app_result = (|| -> color_eyre::Result<()> {
        let app = App::new(bridge.clone()).wrap_err("failed to build settings app")?;
        app.run(rx)
    })();

    // Cooperative cancel + bounded shutdown. The Runtime drop will
    // wait for spawned tasks; `shutdown_timeout` caps the wait so
    // a stuck reqwest body cannot pin the process.
    cancel_token.cancel();
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    drop(bus_conn);

    if let Err(ref e) = app_result {
        error!(error = ?e, "zwhisper-settings exiting with error");
    }
    app_result
}
