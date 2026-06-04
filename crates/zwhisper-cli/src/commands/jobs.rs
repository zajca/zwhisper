//! `zwhisper jobs` — inspect and cancel daemon transcription jobs
//! (RFC-daemon-role Feature 1).
//!
//! Two shapes:
//! - `zwhisper jobs` / `zwhisper jobs list` — snapshot the job registry
//!   (`Jobs1.ListJobs`) and print an aligned table.
//! - `zwhisper jobs cancel <id>` — best-effort cancel (`Jobs1.Cancel`).
//!
//! Mirrors `status.rs`: a current-thread runtime, a `Recorder1`-style
//! proxy build, and the daemon-down / daemon-too-old hint split. The
//! synchronous wrapper translates the async dispatcher's exit code into
//! a `process::exit` so scripts see the same 0/2/3 spread the rest of
//! the CLI honours.

use tracing::warn;
use zwhisper_ipc::{JobInfo, Jobs1Proxy};

use super::{
    DAEMON_DOWN_HINT, DAEMON_TOO_OLD_HINT, EXIT_IPC_FAILURE, EXIT_OK, EXIT_PROTOCOL_ERROR,
    build_runtime, classify_error, is_daemon_down, is_daemon_too_old,
};
use crate::cli::JobsCmd;

/// Synchronous entry point. Wraps the async dispatcher in a one-shot
/// current-thread runtime; a non-zero code becomes a `process::exit`
/// (mirroring `status.rs` / `record.rs`) because `color_eyre::Result`
/// only carries a single `Err` shape and we need the full exit spread.
pub(crate) fn run(command: Option<&JobsCmd>) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    let code = rt.block_on(run_async(command));
    if code == EXIT_OK {
        Ok(())
    } else {
        std::process::exit(code);
    }
}

#[allow(clippy::print_stderr)]
async fn run_async(command: Option<&JobsCmd>) -> i32 {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("{DAEMON_DOWN_HINT}");
            eprintln!("failed to connect to session bus: {err}");
            return EXIT_PROTOCOL_ERROR;
        }
    };

    let proxy = match Jobs1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => return map_daemon_err("build Jobs1 proxy", &err),
    };

    match command {
        // Bare `zwhisper jobs` and explicit `list` share the table path.
        None | Some(JobsCmd::List) => run_list(&proxy).await,
        Some(JobsCmd::Cancel { id }) => run_cancel(&proxy, id).await,
    }
}

#[allow(clippy::print_stderr)]
async fn run_list(proxy: &Jobs1Proxy<'_>) -> i32 {
    let jobs = match proxy.list_jobs().await {
        Ok(j) => j,
        Err(err) => return map_daemon_err("Jobs1.ListJobs", &err),
    };

    if jobs.is_empty() {
        println!("no active jobs");
        return EXIT_OK;
    }

    for line in format_jobs_table(&jobs) {
        println!("{line}");
    }
    EXIT_OK
}

#[allow(clippy::print_stderr)]
async fn run_cancel(proxy: &Jobs1Proxy<'_>, id: &str) -> i32 {
    match proxy.cancel(id).await {
        Ok(()) => {
            println!("cancelled {id}");
            EXIT_OK
        }
        // `JobUnknown` (and any other typed error) is surfaced verbatim
        // and routed through `classify_error` (→ exit 2 for these).
        Err(err) => map_daemon_err("Jobs1.Cancel", &err),
    }
}

/// Render the job list as a header + aligned rows.
///
/// `SUBMITTED` is the raw `submitted_ms` epoch value: `zwhisper-cli`
/// does not depend on `chrono`, and the no-hidden-dependency rule
/// (CLAUDE.md) outweighs a prettier local clock here. Scripts get a
/// stable integer; humans can still eyeball relative ordering.
fn format_jobs_table(jobs: &[JobInfo]) -> Vec<String> {
    // Column widths grow to the longest cell so the table stays aligned
    // without truncating ids (full UUIDs are required to `cancel`).
    let id_w = max_width("JOB_ID", jobs.iter().map(|j| j.job_id.as_str()));
    let state_w = max_width("STATE", jobs.iter().map(|j| j.state.as_str()));
    let label_w = max_width("LABEL", jobs.iter().map(|j| j.label.as_str()));

    let mut lines = Vec::with_capacity(jobs.len() + 1);
    lines.push(format!(
        "{:<id_w$}  {:<state_w$}  {:<label_w$}  {}",
        "JOB_ID", "STATE", "LABEL", "SUBMITTED",
    ));
    for j in jobs {
        lines.push(format!(
            "{:<id_w$}  {:<state_w$}  {:<label_w$}  {}",
            j.job_id, j.state, j.label, j.submitted_ms,
        ));
    }
    lines
}

/// Width of a column = the longest of its header and every cell.
fn max_width<'a>(header: &str, cells: impl Iterator<Item = &'a str>) -> usize {
    cells.map(str::len).fold(header.len(), usize::max)
}

/// Map a daemon-side zbus error to an exit code + user-facing message,
/// distinguishing daemon-down / too-old / typed-error cases. Mirrors
/// `transcribe::map_daemon_err` so every daemon-routed command reports
/// the same hints.
#[allow(clippy::print_stderr)]
fn map_daemon_err(ctx: &str, err: &zbus::Error) -> i32 {
    if is_daemon_down(err) {
        eprintln!("{DAEMON_DOWN_HINT}");
        return EXIT_PROTOCOL_ERROR;
    }
    if is_daemon_too_old(err) {
        eprintln!("{DAEMON_TOO_OLD_HINT}");
        return EXIT_PROTOCOL_ERROR;
    }
    warn!(context = ctx, error = %err, "daemon call failed");
    eprintln!("{ctx} failed: {err}");
    let code = classify_error(err);
    // `classify_error` never returns OK; guard against a future change
    // silently swallowing the failure.
    if code == EXIT_OK {
        EXIT_IPC_FAILURE
    } else {
        code
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use zwhisper_ipc::JobInfo;

    use super::{format_jobs_table, max_width};

    fn job(id: &str, state: &str, label: &str, submitted_ms: u64) -> JobInfo {
        JobInfo {
            job_id: id.to_owned(),
            state: state.to_owned(),
            label: label.to_owned(),
            submitted_ms,
        }
    }

    #[test]
    fn max_width_uses_longest_of_header_and_cells() {
        assert_eq!(max_width("ID", ["a", "longest", "bb"].into_iter()), 7);
        // Header longer than every cell wins.
        assert_eq!(max_width("HEADER", ["a"].into_iter()), 6);
        // No cells → header width.
        assert_eq!(max_width("STATE", std::iter::empty()), 5);
    }

    #[test]
    fn table_has_header_and_one_row_per_job() {
        let jobs = vec![
            job(
                "11111111-2222-3333-4444-555555555555",
                "running",
                "demo.flac",
                1_700_000_000_000,
            ),
            job("aaaa", "queued", "auto:s1", 42),
        ];
        let lines = format_jobs_table(&jobs);
        assert_eq!(lines.len(), 3, "header + 2 rows");
        assert!(lines[0].starts_with("JOB_ID"));
        // Full UUID is preserved (needed to `cancel`).
        assert!(lines[1].contains("11111111-2222-3333-4444-555555555555"));
        assert!(lines[1].contains("running"));
        // Raw submitted_ms is rendered verbatim.
        assert!(lines[1].contains("1700000000000"));
        assert!(lines[2].contains("queued"));
    }

    #[test]
    fn columns_align_to_widest_cell() {
        let jobs = vec![job("short", "done", "x", 1)];
        let lines = format_jobs_table(&jobs);
        // The id column is at least as wide as the "JOB_ID" header.
        let header_id = lines[0].find("STATE").unwrap();
        let row_state = lines[1].find("done").unwrap();
        assert_eq!(header_id, row_state, "STATE column must line up");
    }
}
