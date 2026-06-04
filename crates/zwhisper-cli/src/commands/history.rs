//! `zwhisper history` + `zwhisper retry` — durable session history
//! (RFC-daemon-role Feature 2).
//!
//! Shapes:
//! - `zwhisper history [--limit N]` — list recent sessions
//!   (`History1.ListSessions`, most-recent-first) as an aligned table.
//! - `zwhisper history forget <id> [--delete-files]` — drop a session
//!   from the index (`History1.Forget`), optionally deleting its files.
//! - `zwhisper retry <id>` — re-run a session's transcription
//!   (`History1.Retry`). In Phases 2/3 the daemon gates this behind the
//!   typed `RetryUnavailable` error (RFC F2.4); we surface an actionable
//!   hint rather than a raw D-Bus error.
//!
//! Mirrors `status.rs` / `jobs.rs`: a current-thread runtime, the
//! daemon-down / daemon-too-old hint split, and a `process::exit` for
//! non-zero codes.

use tracing::warn;
use zwhisper_ipc::{History1Proxy, HistorySession};

use super::{
    DAEMON_DOWN_HINT, DAEMON_TOO_OLD_HINT, EXIT_IPC_FAILURE, EXIT_OK, EXIT_PROTOCOL_ERROR,
    HISTORY_DEFAULT_LIMIT, build_runtime, classify_error, is_daemon_down, is_daemon_too_old,
};
use crate::cli::{HistoryArgs, HistoryCmd};

/// Number of leading characters of a session id shown in the `SESSION`
/// column. A UUID's first 8 hex chars are plenty to disambiguate a
/// human's recent sessions; `forget`/`retry` still take the full id.
const SESSION_ID_DISPLAY_LEN: usize = 8;

/// Synchronous entry point for `zwhisper history [forget]`. Non-zero
/// codes become a `process::exit` (mirroring `jobs.rs` / `status.rs`).
pub(crate) fn run(args: &HistoryArgs) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    let code = rt.block_on(run_async(args));
    if code == EXIT_OK {
        Ok(())
    } else {
        std::process::exit(code);
    }
}

/// Synchronous entry point for `zwhisper retry <id>`.
pub(crate) fn run_retry(id: &str) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    let code = rt.block_on(run_retry_async(id));
    if code == EXIT_OK {
        Ok(())
    } else {
        std::process::exit(code);
    }
}

#[allow(clippy::print_stderr)]
async fn run_async(args: &HistoryArgs) -> i32 {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("{DAEMON_DOWN_HINT}");
            eprintln!("failed to connect to session bus: {err}");
            return EXIT_PROTOCOL_ERROR;
        }
    };

    let proxy = match History1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => return map_daemon_err("build History1 proxy", &err),
    };

    match &args.command {
        Some(HistoryCmd::Forget { id, delete_files }) => {
            run_forget(&proxy, id, *delete_files).await
        }
        None => run_list(&proxy, args.limit.unwrap_or(HISTORY_DEFAULT_LIMIT)).await,
    }
}

#[allow(clippy::print_stderr)]
async fn run_list(proxy: &History1Proxy<'_>, limit: u32) -> i32 {
    let sessions = match proxy.list_sessions(limit, 0).await {
        Ok(s) => s,
        Err(err) => return map_daemon_err("History1.ListSessions", &err),
    };

    if sessions.is_empty() {
        println!("no sessions in history");
        return EXIT_OK;
    }

    for line in format_sessions_table(&sessions) {
        println!("{line}");
    }
    EXIT_OK
}

#[allow(clippy::print_stderr)]
async fn run_forget(proxy: &History1Proxy<'_>, id: &str, delete_files: bool) -> i32 {
    match proxy.forget(id, delete_files).await {
        Ok(()) => {
            if delete_files {
                println!("forgot {id} (files deleted)");
            } else {
                println!("forgot {id}");
            }
            EXIT_OK
        }
        Err(err) => map_daemon_err("History1.Forget", &err),
    }
}

#[allow(clippy::print_stderr)]
async fn run_retry_async(id: &str) -> i32 {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("{DAEMON_DOWN_HINT}");
            eprintln!("failed to connect to session bus: {err}");
            return EXIT_PROTOCOL_ERROR;
        }
    };

    let proxy = match History1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => return map_daemon_err("build History1 proxy", &err),
    };

    match proxy.retry(id).await {
        Ok(job_id) => {
            println!("retry queued: {job_id}");
            EXIT_OK
        }
        Err(err) => map_retry_err(&err),
    }
}

/// Map a `History1.Retry` failure. The Phase 4 gate (`RetryUnavailable`)
/// and a missing audio file (`AudioNotFound`) get bespoke, actionable
/// hints; everything else falls through to the shared mapper.
#[allow(clippy::print_stderr)]
fn map_retry_err(err: &zbus::Error) -> i32 {
    // Detect the specific typed variants before the generic daemon-down /
    // too-old checks: a typed error means the daemon answered.
    match zwhisper_ipc::parse_error_name_from_zbus(err) {
        Some("RetryUnavailable") => {
            eprintln!(
                "retry is not yet available; it lands once the audio-model RFC \
                 ships (RFC-daemon-role F2.4)"
            );
            // Typed error → exit 2 per the classifier table.
            return classify_error(err);
        }
        Some("AudioNotFound") => {
            eprintln!(
                "cannot retry: the source FLAC is gone. zwhisper keeps the audio \
                 as the source of truth; once it is deleted the session can no \
                 longer be re-transcribed (`zwhisper history forget` to drop it)."
            );
            return classify_error(err);
        }
        _ => {}
    }
    map_daemon_err("History1.Retry", err)
}

/// Render the session list as a header + aligned rows.
///
/// `CREATED` is the raw `created_at_ms` epoch value — `zwhisper-cli`
/// does not depend on `chrono` and the no-hidden-dependency rule
/// (CLAUDE.md) wins over a prettier local clock. `TRANSCRIPT` shows the
/// basename of `transcript_path` (or `-` when none), which is the part
/// a human cares about without the long state dir prefix.
fn format_sessions_table(sessions: &[HistorySession]) -> Vec<String> {
    let rows: Vec<Row> = sessions.iter().map(Row::from_session).collect();

    let session_w = max_width("SESSION", rows.iter().map(|r| r.session.as_str()));
    let created_w = max_width("CREATED", rows.iter().map(|r| r.created.as_str()));
    let profile_w = max_width("PROFILE", rows.iter().map(|r| r.profile.as_str()));
    let backend_w = max_width("BACKEND", rows.iter().map(|r| r.backend.as_str()));
    let status_w = max_width("STATUS", rows.iter().map(|r| r.status.as_str()));

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format!(
        "{:<session_w$}  {:<created_w$}  {:<profile_w$}  {:<backend_w$}  {:<status_w$}  {}",
        "SESSION", "CREATED", "PROFILE", "BACKEND", "STATUS", "TRANSCRIPT",
    ));
    for r in &rows {
        lines.push(format!(
            "{:<session_w$}  {:<created_w$}  {:<profile_w$}  {:<backend_w$}  {:<status_w$}  {}",
            r.session, r.created, r.profile, r.backend, r.status, r.transcript,
        ));
    }
    lines
}

/// A single pre-rendered table row. Built once per session so the
/// width pass and the print pass agree on the exact cell strings.
struct Row {
    session: String,
    created: String,
    profile: String,
    backend: String,
    status: String,
    transcript: String,
}

impl Row {
    fn from_session(s: &HistorySession) -> Self {
        Self {
            session: short_session_id(&s.session_id),
            created: s.created_at_ms.to_string(),
            profile: s.profile.clone(),
            backend: s.backend.clone(),
            status: s.status.clone(),
            transcript: transcript_basename(&s.transcript_path),
        }
    }
}

/// First [`SESSION_ID_DISPLAY_LEN`] chars of a session id, char-safe
/// (a UUID is ASCII, but we slice on char boundaries to be robust).
fn short_session_id(id: &str) -> String {
    id.chars().take(SESSION_ID_DISPLAY_LEN).collect()
}

/// Basename of a transcript path, or `-` when empty. We avoid pulling
/// the full path into the table; the user can `zwhisper history` then
/// look up the directory if needed.
fn transcript_basename(path: &str) -> String {
    if path.is_empty() {
        return "-".to_owned();
    }
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .map_or_else(|| path.to_owned(), str::to_owned)
}

/// Width of a column = the longest of its header and every cell.
fn max_width<'a>(header: &str, cells: impl Iterator<Item = &'a str>) -> usize {
    cells.map(str::len).fold(header.len(), usize::max)
}

/// Map a daemon-side zbus error to an exit code + user-facing message.
/// Mirrors `jobs::map_daemon_err` / `transcribe::map_daemon_err`.
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
    if code == EXIT_OK {
        EXIT_IPC_FAILURE
    } else {
        code
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use zwhisper_ipc::HistorySession;

    use super::{format_sessions_table, max_width, short_session_id, transcript_basename};

    fn session(
        id: &str,
        profile: &str,
        backend: &str,
        status: &str,
        tpath: &str,
    ) -> HistorySession {
        HistorySession {
            session_id: id.to_owned(),
            created_at_ms: 1_700_000_000_000,
            profile: profile.to_owned(),
            audio_path: "/x/a.flac".to_owned(),
            backend: backend.to_owned(),
            model: "small".to_owned(),
            lang: "en".to_owned(),
            status: status.to_owned(),
            transcript_path: tpath.to_owned(),
            last_error: String::new(),
        }
    }

    #[test]
    fn short_session_id_takes_first_eight() {
        assert_eq!(
            short_session_id("11111111-2222-3333-4444-555555555555"),
            "11111111"
        );
        // Shorter than the window → whole string.
        assert_eq!(short_session_id("abc"), "abc");
    }

    #[test]
    fn transcript_basename_strips_dir() {
        assert_eq!(transcript_basename("/home/u/rec/foo.txt"), "foo.txt");
        assert_eq!(transcript_basename("bare.txt"), "bare.txt");
    }

    #[test]
    fn transcript_basename_empty_is_dash() {
        assert_eq!(transcript_basename(""), "-");
    }

    #[test]
    fn max_width_uses_longest_of_header_and_cells() {
        assert_eq!(
            max_width("STATUS", ["done", "transcribing"].into_iter()),
            12
        );
        assert_eq!(max_width("STATUS", std::iter::empty()), 6);
    }

    #[test]
    fn table_has_header_and_one_row_per_session() {
        let sessions = vec![
            session(
                "11111111-aaaa",
                "dictation",
                "whisper-cpp",
                "done",
                "/r/a.txt",
            ),
            session("22222222-bbbb", "meeting", "deepgram", "failed", ""),
        ];
        let lines = format_sessions_table(&sessions);
        assert_eq!(lines.len(), 3, "header + 2 rows");
        assert!(lines[0].starts_with("SESSION"));
        assert!(lines[1].contains("11111111"));
        assert!(lines[1].contains("dictation"));
        assert!(lines[1].contains("a.txt"));
        // Missing transcript renders as `-`.
        assert!(lines[2].contains("failed"));
        assert!(lines[2].trim_end().ends_with('-'));
    }
}
