//! `zwhisper status` — query the daemon and print a summary.
//!
//! Exit codes (per `DoD` #12):
//! - `0` — daemon responded with a `Status` snapshot
//! - `2` — daemon not on the bus (`ServiceUnknown` / `NameHasNoOwner`)
//! - `3` — any other zbus failure (transport, marshalling, …)

use std::time::Duration;

use serde::Serialize;
use tracing::debug;
use zwhisper_ipc::{Recorder1Proxy, Status};

use super::{
    DAEMON_DOWN_HINT, EXIT_IPC_FAILURE, EXIT_OK, EXIT_PROTOCOL_ERROR, build_runtime,
    is_daemon_down, report_protocol_mismatch, verify_protocol,
};
use crate::cli::StatusArgs;

/// Synchronous entry point. Wraps the async dispatcher in a one-shot
/// current-thread runtime and translates the resulting exit code into
/// a `color_eyre::Result` via `process::exit` — `color_eyre::Result`
/// only carries a single `Err` shape, so the explicit exit gives us
/// the full 0/2/3 spread the contract requires.
pub(crate) fn run(args: &StatusArgs) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    let code = rt.block_on(run_async(args));
    if code == EXIT_OK {
        Ok(())
    } else {
        std::process::exit(i32::from(u8::try_from(code).unwrap_or(2)));
    }
}

#[allow(clippy::print_stderr)]
async fn run_async(args: &StatusArgs) -> i32 {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("{DAEMON_DOWN_HINT}");
            eprintln!("failed to connect to session bus: {err}");
            return EXIT_PROTOCOL_ERROR;
        }
    };

    let proxy = match Recorder1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            eprintln!("failed to build Recorder1 proxy: {err}");
            return EXIT_IPC_FAILURE;
        }
    };

    // M8 pre-flight handshake. The daemon-down case falls through
    // to GetStatus below so the existing actionable hint surfaces
    // unchanged.
    match verify_protocol(&proxy).await {
        super::HandshakeOutcome::Match | super::HandshakeOutcome::DaemonDown => {}
        super::HandshakeOutcome::Mismatch(err) => return report_protocol_mismatch(&err),
    }

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

    if let Err(err) = print_status(&status, args) {
        eprintln!("failed to render status: {err}");
        return EXIT_IPC_FAILURE;
    }

    EXIT_OK
}

fn print_status(status: &Status, args: &StatusArgs) -> color_eyre::Result<()> {
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&StatusJson::from(status))?
        );
    } else if args.waybar {
        println!("{}", serde_json::to_string(&WaybarStatus::from(status))?);
    } else {
        let active = display_active_profile(status);
        println!("state: {}", status.state);
        println!("active profile: {active}");
        println!("duration: {}", format_duration_ms(status.duration_ms));
    }
    Ok(())
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct StatusJson {
    state: String,
    active_profile: Option<String>,
    duration_ms: u64,
    duration: String,
}

impl From<&Status> for StatusJson {
    fn from(status: &Status) -> Self {
        Self {
            state: status.state.clone(),
            active_profile: active_profile_option(status),
            duration_ms: status.duration_ms,
            duration: format_duration_ms(status.duration_ms),
        }
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct WaybarStatus {
    text: String,
    tooltip: String,
    class: Vec<String>,
    percentage: u8,
}

impl From<&Status> for WaybarStatus {
    fn from(status: &Status) -> Self {
        let active = display_active_profile(status);
        let duration = format_duration_ms(status.duration_ms);
        let text = match status.state.as_str() {
            "recording" => format!("REC {duration}"),
            "starting" => "starting".to_owned(),
            "stopping" | "transcribing" => status.state.clone(),
            "failed" => "failed".to_owned(),
            _ => "idle".to_owned(),
        };
        Self {
            text,
            tooltip: format!(
                "zwhisper: state={}, active_profile={}, duration={duration}",
                status.state, active
            ),
            class: waybar_classes(&status.state),
            percentage: waybar_percentage(&status.state),
        }
    }
}

fn active_profile_option(status: &Status) -> Option<String> {
    if status.active_profile.is_empty() {
        None
    } else {
        Some(status.active_profile.clone())
    }
}

fn display_active_profile(status: &Status) -> String {
    active_profile_option(status).unwrap_or_else(|| "(none)".to_owned())
}

fn waybar_classes(state: &str) -> Vec<String> {
    vec!["zwhisper".to_owned(), state.to_owned()]
}

fn waybar_percentage(state: &str) -> u8 {
    match state {
        "recording" => 100,
        "starting" | "stopping" | "transcribing" => 50,
        _ => 0,
    }
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
    use zwhisper_ipc::Status;

    use crate::cli::StatusArgs;

    use super::{
        StatusJson, WaybarStatus, active_profile_option, format_duration_ms, print_status,
    };

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

    #[test]
    fn empty_active_profile_serializes_as_null() {
        let status = Status {
            state: "idle".to_owned(),
            active_profile: String::new(),
            duration_ms: 0,
        };
        assert_eq!(active_profile_option(&status), None);
        assert_eq!(
            StatusJson::from(&status),
            StatusJson {
                state: "idle".to_owned(),
                active_profile: None,
                duration_ms: 0,
                duration: "0ms".to_owned(),
            }
        );
    }

    #[test]
    fn waybar_recording_status_is_compact() {
        let status = Status {
            state: "recording".to_owned(),
            active_profile: "meeting".to_owned(),
            duration_ms: 90_000,
        };
        assert_eq!(
            WaybarStatus::from(&status),
            WaybarStatus {
                text: "REC 1m 30s".to_owned(),
                tooltip: "zwhisper: state=recording, active_profile=meeting, duration=1m 30s"
                    .to_owned(),
                class: vec!["zwhisper".to_owned(), "recording".to_owned()],
                percentage: 100,
            }
        );
    }

    #[test]
    fn print_status_accepts_default_format() {
        let status = Status {
            state: "idle".to_owned(),
            active_profile: String::new(),
            duration_ms: 0,
        };
        let args = StatusArgs {
            json: false,
            waybar: false,
        };
        print_status(&status, &args).unwrap();
    }
}
