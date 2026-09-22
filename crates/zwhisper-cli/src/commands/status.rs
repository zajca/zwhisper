//! `zwhisper status` — query the daemon and print a summary.
//!
//! Two modes share one renderer:
//!
//! - **one-shot** (default) — a single `GetStatus` snapshot, printed
//!   in the format selected by `--json` / `--waybar`, then exit.
//! - **`--watch`** — print the current state immediately, then one
//!   line per transition, driven by `Recorder1.StateChanged` and the
//!   `Jobs1` signals. Replaces interval polling in a status bar; see
//!   `contrib/waybar/zwhisper.jsonc`.
//!
//! Exit codes (per `DoD` #12):
//! - `0` — daemon responded with a `Status` snapshot; in watch mode,
//!   a clean Ctrl+C
//! - `2` — daemon not on the bus (`ServiceUnknown` / `NameHasNoOwner`)
//! - `3` — any other zbus failure (transport, marshalling, …)
//!
//! Watch mode deliberately does *not* exit 2 when the daemon is
//! absent: a bar module must keep running across daemon restarts. It
//! reports `idle` and waits for `NameOwnerChanged` instead. Only a
//! dead session bus (exit 3) or a protocol mismatch (exit 4) stops it.

use std::collections::HashSet;
use std::io::Write;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Serialize;
use tracing::debug;
use zwhisper_hotkey::active_session::{ActiveSessionRef, read_active_session};
use zwhisper_ipc::{Jobs1Proxy, Recorder1Proxy, Status};

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
    if args.watch {
        return run_watch(args).await;
    }

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
    // Defensive surface for an orphaned recording: an active-session.json
    // present while the daemon is NOT mid-session means the startup reaper
    // has not (or could not) clean it. Only meaningful outside the
    // recording states, where the file is expected.
    let orphan = if is_active_recording_state(&status.state) {
        None
    } else {
        read_active_session()
    };

    if args.json {
        let mut json = StatusJson::from(status);
        json.orphaned_session = orphan.as_ref().map(OrphanedSessionJson::from);
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else if args.waybar {
        // Waybar output stays compact; the orphan note would not fit the
        // bar and is surfaced in the default + json views instead.
        println!("{}", serde_json::to_string(&WaybarStatus::from(status))?);
    } else {
        let active = display_active_profile(status);
        println!("state: {}", status.state);
        println!("active profile: {active}");
        println!("duration: {}", format_duration_ms(status.duration_ms));
        if let Some(orphan) = &orphan {
            print_orphan_note(orphan);
        }
    }
    Ok(())
}

/// Recording states in which an `active-session.json` on disk is
/// expected (the daemon is mid-session). In any other state a present
/// file is stale — an orphaned recording, surfaced so the user can act.
fn is_active_recording_state(state: &str) -> bool {
    matches!(
        state,
        "recording" | "starting" | "stopping" | "transcribing"
    )
}

/// Human-readable warning about a stale recording marker. The audio file
/// is always preserved; restarting the daemon auto-recovers it.
#[allow(clippy::print_stdout)]
fn print_orphan_note(orphan: &ActiveSessionRef) {
    println!();
    println!(
        "WARNING: an orphaned recording marker is on disk but the daemon is not recording it."
    );
    println!(
        "  session: {}  profile: {}  started: {}",
        orphan.session_id,
        orphan.profile,
        orphan.started_at.to_rfc3339(),
    );
    println!(
        "  Restart zwhisperd to auto-recover it (preserve audio + transcribe), or remove\n  \
         {} to discard the marker (the recording file is kept).",
        zwhisper_hotkey::active_session::state_file_path().display(),
    );
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct StatusJson {
    state: String,
    active_profile: Option<String>,
    duration_ms: u64,
    duration: String,
    /// Present only when a stale recording marker is detected (daemon not
    /// mid-session). Omitted from the JSON otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    orphaned_session: Option<OrphanedSessionJson>,
}

impl From<&Status> for StatusJson {
    fn from(status: &Status) -> Self {
        Self {
            state: status.state.clone(),
            active_profile: active_profile_option(status),
            duration_ms: status.duration_ms,
            duration: format_duration_ms(status.duration_ms),
            orphaned_session: None,
        }
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct OrphanedSessionJson {
    session_id: String,
    profile: String,
    started_at: String,
}

impl From<&ActiveSessionRef> for OrphanedSessionJson {
    fn from(orphan: &ActiveSessionRef) -> Self {
        Self {
            session_id: orphan.session_id.clone(),
            profile: orphan.profile.clone(),
            started_at: orphan.started_at.to_rfc3339(),
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

// ---------------------------------------------------------------------------
// `--watch` — push-based streaming (#25)
// ---------------------------------------------------------------------------

/// How often the recording timer re-renders while a recording is in
/// flight. The daemon emits no signal per elapsed second, so the
/// duration is derived locally from the last anchored snapshot. One
/// second is the finest granularity `format_duration_ms` renders for a
/// running recording, and it costs no bus traffic.
const WATCH_TICK: Duration = Duration::from_secs(1);

/// Recorder states in which the locally derived timer should keep
/// ticking. Outside them the duration is frozen at the last value the
/// daemon reported, so a finished recording does not keep counting up.
fn ticks_while(state: &str) -> bool {
    state == "recording"
}

/// Terminal `Jobs1.JobProgress` states. A job in any of these is no
/// longer occupying the daemon, so it stops contributing the
/// synthesized `transcribing` state.
fn is_terminal_job_state(state: &str) -> bool {
    matches!(state, "done" | "failed" | "cancelled")
}

/// Everything the watcher needs to render a line without calling the
/// daemon. `base_ms` + `anchor` together reconstruct the recording
/// duration between transitions; `running_jobs` synthesizes the
/// `transcribing` state that `Recorder1` alone cannot express
/// (`RecorderState` has no such variant — transcription happens in a
/// `Jobs1` job after the recorder has already gone back to idle).
struct WatchState {
    recorder_state: String,
    active_profile: String,
    base_ms: u64,
    anchor: Instant,
    orphan: Option<ActiveSessionRef>,
    running_jobs: HashSet<String>,
}

impl WatchState {
    /// Starting point before the daemon has said anything. Mirrors the
    /// shape `GetStatus` returns on a freshly started daemon so a bar
    /// module renders "idle" rather than a blank slot.
    fn new() -> Self {
        Self {
            recorder_state: "idle".to_owned(),
            active_profile: String::new(),
            base_ms: 0,
            anchor: Instant::now(),
            orphan: None,
            running_jobs: HashSet::new(),
        }
    }

    /// Adopt an authoritative snapshot and re-anchor the local timer.
    fn adopt(&mut self, status: &Status) {
        self.recorder_state.clone_from(&status.state);
        self.active_profile.clone_from(&status.active_profile);
        self.base_ms = status.duration_ms;
        self.anchor = Instant::now();
    }

    /// The daemon went away. Report idle and drop job bookkeeping: any
    /// job we were tracking died with it.
    fn daemon_gone(&mut self) {
        "idle".clone_into(&mut self.recorder_state);
        self.active_profile = String::new();
        self.base_ms = 0;
        self.anchor = Instant::now();
        self.running_jobs.clear();
    }

    /// State as rendered. A running `Jobs1` job while the recorder sits
    /// idle is the post-recording transcription step; surfacing it as
    /// `transcribing` is what makes the bar show the part of the wait
    /// the user actually notices. A live recorder state always wins —
    /// a queued job must never mask `recording` or `failed`.
    fn effective_state(&self) -> String {
        if self.recorder_state == "idle" && !self.running_jobs.is_empty() {
            return "transcribing".to_owned();
        }
        self.recorder_state.clone()
    }

    fn duration_ms(&self) -> u64 {
        if ticks_while(&self.recorder_state) {
            let elapsed = u64::try_from(self.anchor.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.base_ms.saturating_add(elapsed)
        } else {
            self.base_ms
        }
    }

    fn observe(&self) -> Observation {
        Observation {
            status: Status {
                state: self.effective_state(),
                active_profile: self.active_profile.clone(),
                duration_ms: self.duration_ms(),
            },
            orphan: self.orphan.clone(),
        }
    }
}

/// One rendered observation: the effective daemon state plus the
/// locally derived orphan marker the one-shot path also reports.
struct Observation {
    status: Status,
    orphan: Option<ActiveSessionRef>,
}

/// Render one line for the stream. Every format is single-line and
/// newline-terminated by the caller: `--json` deliberately drops the
/// one-shot's pretty-printing, because a multi-line object breaks
/// every line-oriented consumer, Waybar included.
fn render_watch_line(obs: &Observation, args: &StatusArgs) -> color_eyre::Result<String> {
    if args.waybar {
        return Ok(serde_json::to_string(&WaybarStatus::from(&obs.status))?);
    }
    if args.json {
        let mut json = StatusJson::from(&obs.status);
        json.orphaned_session = obs.orphan.as_ref().map(OrphanedSessionJson::from);
        return Ok(serde_json::to_string(&json)?);
    }
    let active = display_active_profile(&obs.status);
    let mut line = format!(
        "state: {}  active profile: {active}  duration: {}",
        obs.status.state,
        format_duration_ms(obs.status.duration_ms),
    );
    if obs.orphan.is_some() {
        line.push_str("  [orphaned recording marker on disk]");
    }
    Ok(line)
}

/// `--watch` dispatcher. Returns an exit code like [`run_async`].
///
/// Connection strategy: subscribe to `Recorder1.StateChanged`, the
/// `Jobs1` signals and `NameOwnerChanged` for the daemon's well-known
/// name **before** the first `GetStatus`, for the same reason
/// `record.rs` subscribes first — a transition that happens between
/// the snapshot and the subscription would otherwise be lost.
///
/// `GetStatus` is called only while the bus name is actually owned.
/// A bar module started at login therefore does not D-Bus-activate
/// the daemon just by watching it; the polled module it replaces did
/// exactly that, every interval, forever.
#[allow(clippy::print_stderr, clippy::too_many_lines)]
async fn run_watch(args: &StatusArgs) -> i32 {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("{DAEMON_DOWN_HINT}");
            eprintln!("failed to connect to session bus: {err}");
            return EXIT_PROTOCOL_ERROR;
        }
    };

    let recorder = match Recorder1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            eprintln!("failed to build Recorder1 proxy: {err}");
            return EXIT_IPC_FAILURE;
        }
    };
    // `Jobs1` is only present on RFC-daemon-role daemons. Building the
    // proxy and its match rules never fails against an older daemon —
    // the rules live on the bus, not on the service — so an old daemon
    // simply means these streams stay silent and the synthesized
    // `transcribing` state never appears.
    let jobs = match Jobs1Proxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            eprintln!("failed to build Jobs1 proxy: {err}");
            return EXIT_IPC_FAILURE;
        }
    };
    let dbus = match zbus::fdo::DBusProxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            eprintln!("failed to build org.freedesktop.DBus proxy: {err}");
            return EXIT_IPC_FAILURE;
        }
    };

    let mut state_stream = match recorder.receive_state_changed().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to subscribe to StateChanged: {err}");
            return EXIT_IPC_FAILURE;
        }
    };
    let mut job_progress = match jobs.receive_job_progress().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to subscribe to JobProgress: {err}");
            return EXIT_IPC_FAILURE;
        }
    };
    let mut job_completed = match jobs.receive_job_completed().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to subscribe to JobCompleted: {err}");
            return EXIT_IPC_FAILURE;
        }
    };
    let mut job_failed = match jobs.receive_job_failed().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to subscribe to JobFailed: {err}");
            return EXIT_IPC_FAILURE;
        }
    };
    let mut owner_changes = match dbus.receive_name_owner_changed().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to subscribe to NameOwnerChanged: {err}");
            return EXIT_IPC_FAILURE;
        }
    };

    let mut st = WatchState::new();
    if daemon_owns_name(&dbus).await {
        if let super::HandshakeOutcome::Mismatch(err) = verify_protocol(&recorder).await {
            return report_protocol_mismatch(&err);
        }
        match recorder.get_status().await {
            Ok(status) => st.adopt(&status),
            // The name was owned a moment ago; losing the race here is
            // harmless. Report the default idle line and let the next
            // signal correct it.
            Err(err) => debug!(error = %err, "initial GetStatus failed; starting from idle"),
        }
    } else {
        debug!("daemon not on the bus yet; reporting idle without activating it");
    }
    st.orphan = orphan_for(&st.recorder_state);

    let mut last_line = String::new();
    if let Err(err) = emit_if_changed(&st, args, &mut last_line) {
        eprintln!("failed to render status: {err}");
        return EXIT_IPC_FAILURE;
    }

    // `interval` would fire its first tick immediately, duplicating the
    // line just emitted above; start one period out instead. `Delay`
    // keeps a descheduled watcher from replaying a burst of catch-up
    // ticks, which a bar would render as a stutter.
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + WATCH_TICK, WATCH_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    // One flag per stream, mirroring `record.rs`: a stream that yielded
    // `None` must stop being polled or the select spins. All streams
    // are fed by the same connection, so all-closed means the session
    // bus went away — unrecoverable here, and worth a non-zero exit so
    // the bar restarts us.
    let mut state_done = false;
    let mut progress_done = false;
    let mut completed_done = false;
    let mut failed_done = false;
    let mut owner_done = false;

    loop {
        if state_done && progress_done && completed_done && failed_done && owner_done {
            eprintln!("session bus connection closed; stopping watch");
            return EXIT_IPC_FAILURE;
        }

        let mut dirty = false;

        tokio::select! {
            _ = &mut ctrl_c => return EXIT_OK,

            maybe = state_stream.next(), if !state_done => {
                let Some(signal) = maybe else {
                    debug!("StateChanged stream closed");
                    state_done = true;
                    continue;
                };
                let Ok(sig_args) = signal.args() else {
                    debug!("StateChanged with malformed args, dropping");
                    continue;
                };
                st.recorder_state = sig_args.new_state.to_owned();
                // The signal carries state and session id only, so the
                // active profile and the daemon's own duration still
                // come from a snapshot. One RPC per transition, never
                // per tick.
                match recorder.get_status().await {
                    Ok(status) => {
                        st.active_profile.clone_from(&status.active_profile);
                        st.base_ms = status.duration_ms;
                        st.anchor = Instant::now();
                    }
                    Err(err) => debug!(error = %err, "GetStatus after StateChanged failed"),
                }
                st.orphan = orphan_for(&st.recorder_state);
                dirty = true;
            },

            maybe = job_progress.next(), if !progress_done => {
                let Some(signal) = maybe else {
                    debug!("JobProgress stream closed");
                    progress_done = true;
                    continue;
                };
                let Ok(sig_args) = signal.args() else {
                    debug!("JobProgress with malformed args, dropping");
                    continue;
                };
                if is_terminal_job_state(sig_args.state) {
                    st.running_jobs.remove(sig_args.job_id);
                } else {
                    st.running_jobs.insert(sig_args.job_id.to_owned());
                }
                dirty = true;
            },

            maybe = job_completed.next(), if !completed_done => {
                let Some(signal) = maybe else {
                    debug!("JobCompleted stream closed");
                    completed_done = true;
                    continue;
                };
                if let Ok(sig_args) = signal.args() {
                    st.running_jobs.remove(sig_args.job_id);
                    dirty = true;
                }
            },

            maybe = job_failed.next(), if !failed_done => {
                let Some(signal) = maybe else {
                    debug!("JobFailed stream closed");
                    failed_done = true;
                    continue;
                };
                if let Ok(sig_args) = signal.args() {
                    st.running_jobs.remove(sig_args.job_id);
                    dirty = true;
                }
            },

            maybe = owner_changes.next(), if !owner_done => {
                let Some(signal) = maybe else {
                    debug!("NameOwnerChanged stream closed");
                    owner_done = true;
                    continue;
                };
                let Ok(sig_args) = signal.args() else { continue };
                if sig_args.name.as_str() != zwhisper_ipc::BUS_NAME {
                    continue;
                }
                if sig_args.new_owner.is_none() {
                    debug!("daemon left the bus");
                    st.daemon_gone();
                    st.orphan = orphan_for(&st.recorder_state);
                    dirty = true;
                } else {
                    debug!("daemon appeared on the bus");
                    // A restarted daemon may be a different build.
                    if let super::HandshakeOutcome::Mismatch(err) = verify_protocol(&recorder).await {
                        return report_protocol_mismatch(&err);
                    }
                    match recorder.get_status().await {
                        Ok(status) => st.adopt(&status),
                        Err(err) => debug!(error = %err, "GetStatus after daemon restart failed"),
                    }
                    st.orphan = orphan_for(&st.recorder_state);
                    dirty = true;
                }
            },

            _ = ticker.tick(), if ticks_while(&st.recorder_state) => {
                dirty = true;
            },
        }

        if dirty && let Err(err) = emit_if_changed(&st, args, &mut last_line) {
            eprintln!("failed to render status: {err}");
            return EXIT_IPC_FAILURE;
        }
    }
}

/// Whether the daemon currently owns its well-known name. Uses
/// `NameHasOwner` rather than any `Recorder1` call precisely because
/// it does **not** trigger D-Bus activation.
async fn daemon_owns_name(dbus: &zbus::fdo::DBusProxy<'_>) -> bool {
    let Ok(name) = zbus::names::BusName::try_from(zwhisper_ipc::BUS_NAME) else {
        return false;
    };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

/// Read the stale-recording marker, but only in states where its
/// presence is anomalous — the same rule the one-shot path applies.
fn orphan_for(state: &str) -> Option<ActiveSessionRef> {
    if is_active_recording_state(state) {
        None
    } else {
        read_active_session()
    }
}

/// Print one line, but only when it differs from the previous one.
/// Unrelated `NameOwnerChanged` traffic and repeated terminal job
/// states would otherwise push identical lines at a bar for no reason.
#[allow(clippy::print_stdout)]
fn emit_if_changed(
    st: &WatchState,
    args: &StatusArgs,
    last_line: &mut String,
) -> color_eyre::Result<()> {
    let line = render_watch_line(&st.observe(), args)?;
    if line == *last_line {
        return Ok(());
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    // A status bar reads this incrementally; without an explicit flush
    // the pipe buffer would hold lines back until it filled.
    out.flush()?;
    last_line.clear();
    last_line.push_str(&line);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use zwhisper_ipc::Status;

    use crate::cli::StatusArgs;

    use super::{
        Observation, StatusJson, WatchState, WaybarStatus, active_profile_option, emit_if_changed,
        format_duration_ms, is_active_recording_state, is_terminal_job_state, print_status,
        render_watch_line, ticks_while,
    };

    #[test]
    fn recording_states_expect_active_session_file() {
        for state in ["recording", "starting", "stopping", "transcribing"] {
            assert!(is_active_recording_state(state), "{state}");
        }
        for state in ["idle", "failed", "", "unknown"] {
            assert!(!is_active_recording_state(state), "{state}");
        }
    }

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
                orphaned_session: None,
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

    // -- `--watch` (#25) -------------------------------------------------

    fn watch_args(json: bool, waybar: bool) -> StatusArgs {
        StatusArgs {
            json,
            waybar,
            watch: true,
        }
    }

    fn status_of(state: &str, profile: &str, duration_ms: u64) -> Status {
        Status {
            state: state.to_owned(),
            active_profile: profile.to_owned(),
            duration_ms,
        }
    }

    #[test]
    fn watch_starts_idle_before_the_daemon_says_anything() {
        let st = WatchState::new();
        assert_eq!(st.effective_state(), "idle");
        assert_eq!(st.duration_ms(), 0);
    }

    #[test]
    fn only_recording_advances_the_local_timer() {
        assert!(ticks_while("recording"));
        for state in ["idle", "starting", "stopping", "failed", "transcribing"] {
            assert!(!ticks_while(state), "{state}");
        }
    }

    #[test]
    fn duration_is_frozen_outside_recording() {
        let mut st = WatchState::new();
        st.adopt(&status_of("stopping", "meeting", 90_000));
        // No sleep needed: a frozen duration must equal the daemon's
        // last word regardless of how much wall-clock passes.
        assert_eq!(st.duration_ms(), 90_000);
    }

    #[test]
    fn duration_resumes_from_the_daemons_anchor_while_recording() {
        let mut st = WatchState::new();
        st.adopt(&status_of("recording", "meeting", 5_000));
        // Anchored at 5s; the locally derived value may only grow.
        assert!(st.duration_ms() >= 5_000);
    }

    #[test]
    fn a_running_job_synthesizes_the_transcribing_state() {
        let mut st = WatchState::new();
        st.adopt(&status_of("idle", "", 0));
        st.running_jobs.insert("job-1".to_owned());
        assert_eq!(st.effective_state(), "transcribing");
        st.running_jobs.remove("job-1");
        assert_eq!(st.effective_state(), "idle");
    }

    #[test]
    fn a_live_recorder_state_is_never_masked_by_a_job() {
        let mut st = WatchState::new();
        st.running_jobs.insert("job-1".to_owned());
        for state in ["recording", "starting", "stopping", "failed"] {
            st.adopt(&status_of(state, "meeting", 0));
            assert_eq!(st.effective_state(), state, "{state}");
        }
    }

    #[test]
    fn terminal_job_states_stop_contributing() {
        for state in ["done", "failed", "cancelled"] {
            assert!(is_terminal_job_state(state), "{state}");
        }
        for state in ["queued", "running"] {
            assert!(!is_terminal_job_state(state), "{state}");
        }
    }

    #[test]
    fn a_departed_daemon_reports_idle_and_drops_its_jobs() {
        let mut st = WatchState::new();
        st.adopt(&status_of("recording", "meeting", 42_000));
        st.running_jobs.insert("job-1".to_owned());
        st.daemon_gone();
        assert_eq!(st.effective_state(), "idle");
        assert_eq!(st.duration_ms(), 0);
        assert!(st.running_jobs.is_empty());
    }

    #[test]
    fn every_watch_format_renders_exactly_one_line() {
        let obs = Observation {
            status: status_of("recording", "meeting", 90_000),
            orphan: None,
        };
        for args in [
            watch_args(false, true),
            watch_args(true, false),
            watch_args(false, false),
        ] {
            let line = render_watch_line(&obs, &args).unwrap();
            assert!(!line.contains('\n'), "{line}");
            assert!(!line.is_empty());
        }
    }

    #[test]
    fn watch_waybar_line_matches_the_one_shot_shape() {
        let status = status_of("recording", "meeting", 90_000);
        let obs = Observation {
            status: status.clone(),
            orphan: None,
        };
        let line = render_watch_line(&obs, &watch_args(false, true)).unwrap();
        let expected = serde_json::to_string(&WaybarStatus::from(&status)).unwrap();
        assert_eq!(line, expected);
    }

    #[test]
    fn watch_json_is_compact_unlike_the_one_shot_pretty_form() {
        let status = status_of("idle", "", 0);
        let obs = Observation {
            status: status.clone(),
            orphan: None,
        };
        let line = render_watch_line(&obs, &watch_args(true, false)).unwrap();
        assert_eq!(
            line,
            serde_json::to_string(&StatusJson::from(&status)).unwrap()
        );
        assert!(!line.contains('\n'));
    }

    #[test]
    fn identical_observations_are_not_re_emitted() {
        let mut st = WatchState::new();
        st.adopt(&status_of("idle", "", 0));
        let args = watch_args(false, true);
        let mut last = String::new();

        emit_if_changed(&st, &args, &mut last).unwrap();
        let first = last.clone();
        assert!(!first.is_empty(), "first observation must be emitted");

        // Same state again: `last_line` must be untouched, which is what
        // the caller uses to decide nothing was written.
        emit_if_changed(&st, &args, &mut last).unwrap();
        assert_eq!(last, first);

        st.adopt(&status_of("recording", "meeting", 0));
        emit_if_changed(&st, &args, &mut last).unwrap();
        assert_ne!(last, first, "a real transition must change the line");
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
            watch: false,
        };
        print_status(&status, &args).unwrap();
    }
}
