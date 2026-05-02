//! `zwhisper status` — query the daemon and print a 3-line summary.
//!
//! Exit codes (per `DoD` #12):
//! - `0` — daemon responded with a `Status` snapshot
//! - `2` — daemon not on the bus (`ServiceUnknown` / `NameHasNoOwner`)
//! - `3` — any other zbus failure (transport, marshalling, …)

use std::time::Duration;

use tracing::debug;
use zwhisper_ipc::Recorder1Proxy;

use super::{
    DAEMON_DOWN_HINT, EXIT_IPC_FAILURE, EXIT_OK, EXIT_PROTOCOL_ERROR, build_runtime, is_daemon_down,
};

/// Synchronous entry point. Wraps the async dispatcher in a one-shot
/// current-thread runtime and translates the resulting exit code into
/// a `color_eyre::Result` via `process::exit` — `color_eyre::Result`
/// only carries a single `Err` shape, so the explicit exit gives us
/// the full 0/2/3 spread the contract requires.
pub(crate) fn run() -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    let code = rt.block_on(run_async());
    if code == EXIT_OK {
        Ok(())
    } else {
        std::process::exit(i32::from(u8::try_from(code).unwrap_or(2)));
    }
}

#[allow(clippy::print_stderr)]
async fn run_async() -> i32 {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("failed to connect to session bus: {err}");
            return EXIT_IPC_FAILURE;
        }
    };

    let proxy = match Recorder1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            eprintln!("failed to build Recorder1 proxy: {err}");
            return EXIT_IPC_FAILURE;
        }
    };

    let status = match proxy.get_status().await {
        Ok(s) => s,
        Err(err) => {
            debug!(error = %err, "GetStatus failed");
            if is_daemon_down(&err) {
                eprintln!("{DAEMON_DOWN_HINT}");
                return EXIT_PROTOCOL_ERROR;
            }
            eprintln!("daemon RPC failed: {err}");
            return EXIT_IPC_FAILURE;
        }
    };

    let active = if status.active_profile.is_empty() {
        "(none)".to_owned()
    } else {
        status.active_profile.clone()
    };
    println!("state: {}", status.state);
    println!("active profile: {active}");
    println!("duration: {}", format_duration_ms(status.duration_ms));

    EXIT_OK
}

/// Format a millisecond count as a short human-readable duration.
/// We render `0ms` exactly when no recording is active so the user
/// sees the daemon's intent literally; otherwise we degrade to a
/// `Hh Mm Ss` form for legibility.
fn format_duration_ms(ms: u64) -> String {
    if ms == 0 {
        return "0ms".to_owned();
    }
    let dur = Duration::from_millis(ms);
    let total_secs = dur.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    let millis = ms % 1000;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else if seconds > 0 {
        format!("{seconds}.{millis:03}s")
    } else {
        format!("{millis}ms")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::format_duration_ms;

    #[test]
    fn zero_ms_renders_literally() {
        assert_eq!(format_duration_ms(0), "0ms");
    }

    #[test]
    fn sub_second_renders_as_milliseconds() {
        assert_eq!(format_duration_ms(250), "250ms");
    }

    #[test]
    fn seconds_render_with_millis() {
        assert_eq!(format_duration_ms(1_250), "1.250s");
    }

    #[test]
    fn minutes_render_compactly() {
        assert_eq!(format_duration_ms(90_000), "1m 30s");
    }

    #[test]
    fn hours_render_with_zero_padded_minutes_and_seconds() {
        // 1h 02m 03s
        let ms = 3_600_000 + 2 * 60_000 + 3_000;
        assert_eq!(format_duration_ms(ms), "1h 02m 03s");
    }
}
