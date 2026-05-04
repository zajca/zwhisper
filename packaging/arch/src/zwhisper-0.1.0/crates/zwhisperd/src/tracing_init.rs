//! Daemon-side tracing setup.
//!
//! Mirrors the CLI's daily-appender pattern (`zwhisper-cli::main::init_tracing`)
//! so log files for the recording daemon end up in the same XDG state
//! directory: `~/.local/state/zwhisper/zwhisperd.log`. Best-effort —
//! if the directory cannot be created we still log to stderr.
//!
//! Uses a separate file name (`zwhisperd.log`) from the CLI so the
//! two ring buffers never compete for the same file handle when both
//! are running on the same machine.

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Install the daemon's tracing subscriber. Returns the appender
/// guard; the caller must keep it alive for the duration of the
/// process so the background flush thread does not exit.
pub(crate) fn init(verbosity: u8) -> Option<WorkerGuard> {
    let default_level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let stderr_layer = fmt::layer().with_target(false).with_writer(std::io::stderr);

    // Best-effort daily file appender. Failure to create the
    // directory is logged but never aborts the daemon — recording
    // still works, only the on-disk log goes silent.
    let (file_layer, guard) = match log_dir() {
        Some(dir) if std::fs::create_dir_all(&dir).is_ok() => {
            let appender = tracing_appender::rolling::daily(dir, "zwhisperd.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(fmt::layer().with_ansi(false).with_writer(writer)),
                Some(guard),
            )
        }
        _ => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    guard
}

fn log_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|base| base.join("zwhisper"))
}
