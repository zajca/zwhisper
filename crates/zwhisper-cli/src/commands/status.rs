//! `zwhisper status` — query the daemon and print a summary.
//!
//! Two modes share one renderer:
//!
//! - **one-shot** (default) — a single `GetStatus` snapshot, printed
//!   in the format selected by `--json` / `--waybar`, then exit.
//! - **`--watch`** — print the current state immediately, then one
//!   line per transition, driven by `Recorder1.StateChanged`.
//!   Replaces interval polling in a status bar; see
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

use std::io::Write;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Serialize;
use tracing::debug;
use zwhisper_hotkey::active_session::{ActiveSessionRef, read_active_session};
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

/// Deadline for any single daemon round trip made from inside the
/// watch loop. Two seconds is far longer than a healthy `GetStatus`
/// (sub-millisecond on a local session bus) and short enough that a
/// wedged daemon costs the bar one stale interval rather than the
/// rest of the session.
const RPC_TIMEOUT: Duration = Duration::from_secs(2);

/// Recorder states in which the locally derived timer should keep
/// ticking. Outside them the duration is frozen at the last value the
/// daemon reported, so a finished recording does not keep counting up.
fn ticks_while(state: &str) -> bool {
    state == "recording"
}

/// Terminal `Recorder1` states: the end of one session's lifecycle.
/// Everything else describes a session that is still in flight.
fn is_terminal_state(state: &str) -> bool {
    matches!(state, "idle" | "failed")
}

/// Lines the watcher must never suppress as a duplicate. A repeated
/// render of a discrete, notification-worthy event is a *second*
/// event, not a no-op, and swallowing it hides a real failure from
/// the user. Continuous states (`idle`, the per-second `recording`
/// ticks) are the only ones where repetition genuinely says nothing
/// new, so dedup applies to those alone.
fn is_discrete_state(state: &str) -> bool {
    state == "failed"
}

/// Everything the watcher needs to render a line without calling the
/// daemon. `base_ms` + `anchor` together reconstruct the recording
/// duration between transitions.
struct WatchState {
    recorder_state: String,
    active_profile: String,
    base_ms: u64,
    anchor: Instant,
    orphan: Option<ActiveSessionRef>,
    /// Session whose lifecycle is currently in flight, if any. The
    /// daemon releases the recording slot *before* awaiting the
    /// transcribe job (`zwhisperd/src/lifecycle.rs:194`), so a new
    /// recording can legitimately start while the previous session is
    /// still transcribing — and the older session's terminal signal
    /// then arrives *after* the newer one is already recording. Without
    /// this, that stale terminal would overwrite the live state.
    current_session: Option<String>,
    /// The daemon owns its name but did not answer the last resync, so
    /// what we are rendering is unverified. The ticker retries while
    /// this is set; without it, a failed resync at startup would leave
    /// the bar claiming `idle` for the whole of a recording that began
    /// before the watcher attached, since no further `StateChanged`
    /// arrives until that recording stops.
    stale: bool,
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
            current_session: None,
            stale: false,
        }
    }

    /// Adopt an authoritative snapshot and re-anchor the local timer.
    fn adopt(&mut self, status: &Status) {
        self.recorder_state.clone_from(&status.state);
        self.active_profile.clone_from(&status.active_profile);
        self.base_ms = status.duration_ms;
        self.anchor = Instant::now();
        self.stale = false;
    }

    /// The daemon went away. Report idle: whatever it was doing died
    /// with it, and the next `GetStatus` after it returns re-anchors.
    fn daemon_gone(&mut self) {
        "idle".clone_into(&mut self.recorder_state);
        self.active_profile = String::new();
        self.base_ms = 0;
        self.anchor = Instant::now();
        self.current_session = None;
        // Nothing left to reconcile against: idle is the truth now.
        self.stale = false;
    }

    /// Re-anchor the local timer without an authoritative snapshot.
    /// Used when `GetStatus` fails right after a `StateChanged`: the
    /// signal told us the new state, but carrying the previous
    /// session's `base_ms` and `anchor` forward would render the new
    /// recording as already minutes old and climbing. Zeroing is the
    /// honest reading — the elapsed time of the state we just entered
    /// is, as far as we can verify, zero.
    fn reanchor_unknown(&mut self) {
        self.active_profile = String::new();
        self.base_ms = 0;
        self.anchor = Instant::now();
        self.stale = true;
    }

    /// Whether the per-second ticker should be armed: either the timer
    /// is advancing, or we owe the daemon a retry.
    fn wants_tick(&self) -> bool {
        ticks_while(&self.recorder_state) || self.stale
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
                state: self.recorder_state.clone(),
                active_profile: self.active_profile.clone(),
                duration_ms: self.duration_ms(),
            },
            orphan: self.orphan.clone(),
        }
    }
}

/// One rendered observation: the daemon's reported state plus the
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
/// Connection strategy: subscribe to `Recorder1.StateChanged` and to
/// `NameOwnerChanged` for the daemon's well-known name **before** the
/// first `GetStatus`, for the same reason
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

    // `ProtocolVersion` must never be served from zbus's property
    // cache here. The cache is populated lazily on first read and is
    // only invalidated by a `PropertiesChanged` signal, which a
    // restarted daemon has no reason to emit for a value fixed at
    // build time. A cached read would therefore report the *previous*
    // daemon's version, and the re-acquisition handshake would wave
    // through exactly the partial-upgrade case it exists to catch.
    let recorder = match Recorder1Proxy::builder(&conn)
        .uncached_properties(&[zwhisper_ipc::PROTOCOL_VERSION_PROPERTY])
        .build()
        .await
    {
        Ok(p) => p,
        Err(err) => {
            eprintln!("failed to build Recorder1 proxy: {err}");
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
    // arg0-filtered: the match rule is installed on the bus daemon, so
    // it never delivers the churn of every other connection on the
    // session bus. Unfiltered, this stream wakes the watcher for every
    // process that connects or disconnects anywhere — measured at
    // ~1/sec on a desktop running the very polled module this replaces.
    let mut owner_changes = match dbus
        .receive_name_owner_changed_with_args(&[(0, zwhisper_ipc::BUS_NAME)])
        .await
    {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to subscribe to NameOwnerChanged: {err}");
            return EXIT_IPC_FAILURE;
        }
    };

    let mut st = WatchState::new();
    if daemon_owns_name(&dbus).await {
        match resync(&recorder).await {
            Resync::Ok(status) => st.adopt(&status),
            Resync::Mismatch(code) => return code,
            // Report the default idle line, but mark it unverified so
            // the ticker retries. Waiting for the next `StateChanged`
            // is not enough: if the daemon was already recording when
            // we attached, no signal arrives until that recording
            // stops, and the bar would claim idle throughout.
            Resync::Unavailable => {
                debug!("initial resync failed; reporting idle until a retry succeeds");
                st.stale = true;
            }
        }
    } else {
        debug!("daemon not on the bus yet; reporting idle without activating it");
    }
    st.orphan = orphan_for(&st.recorder_state);

    let mut last_line = String::new();
    if let Err(err) = emit_if_changed(&st, args, &mut last_line).map(drop) {
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
    // `None` must stop being polled or the select spins. The loop head
    // then turns either flag into a clean exit.
    let mut state_done = false;
    let mut owner_done = false;

    loop {
        // Either of these two streams dying makes everything we render
        // unverifiable: `state_stream` is the only source of state, and
        // `owner_changes` is the only way we learn the daemon left. A
        // watcher that kept running on a dead `state_stream` would go
        // on printing a duration that climbs forever for a recording
        // that already ended — confidently wrong, which for a status
        // indicator is worse than being absent. Exiting non-zero lets
        // the bar's `restart-interval` bring us back.
        if state_done || owner_done {
            eprintln!("daemon signal stream closed; stopping watch so the bar can restart it");
            return EXIT_IPC_FAILURE;
        }

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
                if is_terminal_state(sig_args.new_state) {
                    if st
                        .current_session
                        .as_deref()
                        .is_some_and(|active| active != sig_args.session_id)
                    {
                        // A previous session finishing its transcribe
                        // step while a newer one is already recording.
                        // Its terminal state says nothing about the
                        // session we are displaying.
                        debug!(
                            session_id = sig_args.session_id,
                            state = sig_args.new_state,
                            "terminal signal for a superseded session, ignoring"
                        );
                        continue;
                    }
                    st.current_session = None;
                } else {
                    st.current_session = Some(sig_args.session_id.to_owned());
                }
                st.recorder_state = sig_args.new_state.to_owned();
                // The signal carries state and session id only, so the
                // active profile and the daemon's own duration still
                // come from a snapshot. One RPC per transition, never
                // per tick.
                match resync(&recorder).await {
                    Resync::Ok(status) => {
                        st.active_profile.clone_from(&status.active_profile);
                        st.base_ms = status.duration_ms;
                        st.anchor = Instant::now();
                    }
                    Resync::Mismatch(code) => return code,
                    // Carrying the previous session's anchor forward
                    // would render this brand-new state as already
                    // minutes old and climbing.
                    Resync::Unavailable => st.reanchor_unknown(),
                }
                st.orphan = orphan_for(&st.recorder_state);
            },

            maybe = owner_changes.next(), if !owner_done => {
                let Some(signal) = maybe else {
                    debug!("NameOwnerChanged stream closed");
                    owner_done = true;
                    continue;
                };
                let Ok(sig_args) = signal.args() else {
                    debug!("NameOwnerChanged with malformed args, dropping");
                    continue;
                };
                if sig_args.new_owner.is_none() {
                    debug!("daemon left the bus");
                    st.daemon_gone();
                } else {
                    debug!("daemon appeared on the bus");
                    // A restarted daemon may be a different build.
                    match resync(&recorder).await {
                        Resync::Ok(status) => st.adopt(&status),
                        Resync::Mismatch(code) => return code,
                        Resync::Unavailable => st.reanchor_unknown(),
                    }
                }
                st.orphan = orphan_for(&st.recorder_state);
            },

            _ = ticker.tick(), if st.wants_tick() => {
                if st.stale {
                    // Only retry while the name is owned, so a retry
                    // never becomes the thing that activates a daemon
                    // the user has not started.
                    if daemon_owns_name(&dbus).await {
                        match resync(&recorder).await {
                            Resync::Ok(status) => st.adopt(&status),
                            Resync::Mismatch(code) => return code,
                            Resync::Unavailable => {}
                        }
                        st.orphan = orphan_for(&st.recorder_state);
                    } else {
                        st.daemon_gone();
                        st.orphan = orphan_for(&st.recorder_state);
                    }
                }
            },
        }

        // Every arm that falls through to here changed something; the
        // ones that did not (`continue` on a closed or malformed
        // signal) never reach it.
        if let Err(err) = emit_if_changed(&st, args, &mut last_line).map(drop) {
            eprintln!("failed to render status: {err}");
            return EXIT_IPC_FAILURE;
        }
    }
}

/// Outcome of a timeout-guarded handshake-plus-snapshot round trip.
enum Resync {
    Ok(Status),
    /// The daemon answered, but with a protocol version this client
    /// refuses to talk to. Carries the exit code to return.
    Mismatch(i32),
    /// No usable answer within [`RPC_TIMEOUT`], or the call failed.
    Unavailable,
}

/// Re-verify the protocol and take a fresh snapshot, both under a
/// deadline.
///
/// The deadline is the point: `NameHasOwner` can report the daemon as
/// present while it is wedged (a deadlock, a blocking syscall on its
/// executor — anything short of a crash), and an unbounded `await`
/// here would hang the whole watcher. Because the await sits inside a
/// chosen `tokio::select!` branch, that would also stop Ctrl+C from
/// being polled, and a watcher that never exits is one the bar's
/// `restart-interval` can never recover. The rest of the CLI guards
/// this same class of call the same way — see `toggle.rs`,
/// `hotkey.rs`, `transcribe.rs`.
async fn resync(recorder: &Recorder1Proxy<'_>) -> Resync {
    let handshake = async {
        match verify_protocol(recorder).await {
            super::HandshakeOutcome::Mismatch(err) => Some(report_protocol_mismatch(&err)),
            super::HandshakeOutcome::Match | super::HandshakeOutcome::DaemonDown => None,
        }
    };
    match tokio::time::timeout(RPC_TIMEOUT, handshake).await {
        Ok(Some(code)) => return Resync::Mismatch(code),
        Ok(None) => {}
        Err(_elapsed) => {
            debug!("protocol handshake timed out; daemon present but unresponsive");
            return Resync::Unavailable;
        }
    }
    match tokio::time::timeout(RPC_TIMEOUT, recorder.get_status()).await {
        Ok(Ok(status)) => Resync::Ok(status),
        Ok(Err(err)) => {
            debug!(error = %err, "GetStatus failed");
            Resync::Unavailable
        }
        Err(_elapsed) => {
            debug!("GetStatus timed out; daemon present but unresponsive");
            Resync::Unavailable
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

/// Print one line, but only when it says something new.
///
/// Repeated renders of a continuous state (`idle`, or a `recording`
/// tick whose duration has not advanced a whole unit yet) carry no
/// information, so they are suppressed. A discrete state is different:
/// a second `failed` is a second failure, and swallowing it because it
/// happens to render identically to the first would hide a real event
/// from the user. See [`is_discrete_state`].
#[allow(clippy::print_stdout)]
fn emit_if_changed(
    st: &WatchState,
    args: &StatusArgs,
    last_line: &mut String,
) -> color_eyre::Result<bool> {
    let observation = st.observe();
    let discrete = is_discrete_state(&observation.status.state);
    let line = render_watch_line(&observation, args)?;
    if !discrete && line == *last_line {
        return Ok(false);
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    // A status bar reads this incrementally; without an explicit flush
    // the pipe buffer would hold lines back until it filled.
    out.flush()?;
    last_line.clear();
    last_line.push_str(&line);
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use zwhisper_ipc::Status;

    use crate::cli::StatusArgs;

    use super::{
        Observation, StatusJson, WatchState, WaybarStatus, active_profile_option, emit_if_changed,
        format_duration_ms, is_active_recording_state, is_discrete_state, is_terminal_state,
        print_status, render_watch_line, ticks_while,
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
        assert_eq!(st.observe().status.state, "idle");
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
    fn only_the_daemons_own_states_are_ever_rendered() {
        // The watcher reports what `Recorder1` says and never invents a
        // state of its own. `transcribing` in particular is not ours to
        // synthesize — see the follow-up issue for the daemon-side
        // signal that would carry it properly.
        let mut st = WatchState::new();
        for state in ["idle", "starting", "recording", "stopping", "failed"] {
            st.adopt(&status_of(state, "meeting", 0));
            assert_eq!(st.observe().status.state, state, "{state}");
        }
    }

    #[test]
    fn a_departed_daemon_reports_idle_from_zero() {
        let mut st = WatchState::new();
        st.adopt(&status_of("recording", "meeting", 42_000));
        st.daemon_gone();
        assert_eq!(st.observe().status.state, "idle");
        assert_eq!(st.duration_ms(), 0);
        assert_eq!(st.observe().status.active_profile, "");
    }

    #[test]
    fn a_failed_resync_does_not_carry_the_previous_session_forward() {
        // The regression this guards: `StateChanged("recording")` lands,
        // the follow-up snapshot fails, and the bar renders the *previous*
        // session's profile and elapsed time, climbing from a stale anchor.
        let mut st = WatchState::new();
        st.adopt(&status_of("stopping", "meeting", 45_000));
        assert_eq!(st.duration_ms(), 45_000);

        st.recorder_state = "recording".to_owned();
        st.reanchor_unknown();

        assert_eq!(st.observe().status.active_profile, "");
        assert!(
            st.duration_ms() < 1_000,
            "a re-anchored timer must start near zero, got {}",
            st.duration_ms()
        );
    }

    #[test]
    fn failed_is_discrete_and_the_continuous_states_are_not() {
        assert!(is_discrete_state("failed"));
        for state in ["idle", "starting", "recording", "stopping"] {
            assert!(!is_discrete_state(state), "{state}");
        }
    }

    #[test]
    fn a_second_identical_failure_is_still_emitted() {
        // Two fast failures can render byte-identically (both
        // `duration_ms: 0`). Suppressing the second would hide a real
        // second failure from the user.
        let mut st = WatchState::new();
        st.adopt(&status_of("failed", "meeting", 0));
        let args = watch_args(false, true);
        let mut last = String::new();

        assert!(emit_if_changed(&st, &args, &mut last).unwrap());
        let first = last.clone();

        assert!(
            emit_if_changed(&st, &args, &mut last).unwrap(),
            "the identical second failure must be re-emitted, not suppressed"
        );
        assert_eq!(last, first, "and it must render the same line");
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

        assert!(
            emit_if_changed(&st, &args, &mut last).unwrap(),
            "the first observation must be emitted"
        );
        let first = last.clone();

        assert!(
            !emit_if_changed(&st, &args, &mut last).unwrap(),
            "a repeated continuous state must be suppressed"
        );

        st.adopt(&status_of("recording", "meeting", 0));
        assert!(
            emit_if_changed(&st, &args, &mut last).unwrap(),
            "a real transition must be emitted"
        );
        assert_ne!(last, first);
    }

    #[test]
    fn terminal_states_are_the_end_of_a_lifecycle() {
        for state in ["idle", "failed"] {
            assert!(is_terminal_state(state), "{state}");
        }
        for state in ["starting", "recording", "stopping"] {
            assert!(!is_terminal_state(state), "{state}");
        }
    }

    #[test]
    fn a_stale_terminal_must_not_overwrite_a_newer_session() {
        // The daemon releases the recording slot before awaiting the
        // transcribe job, so session B can start while A is still
        // transcribing; A's terminal `idle` then arrives last.
        let mut st = WatchState::new();

        // A is stopping.
        st.current_session = Some("session-a".to_owned());
        st.adopt(&status_of("stopping", "meeting", 60_000));

        // B starts and is recording.
        st.current_session = Some("session-b".to_owned());
        st.adopt(&status_of("recording", "dictation", 0));

        // A's terminal signal is for a session we are no longer showing.
        let stale_terminal_is_for_another_session = st
            .current_session
            .as_deref()
            .is_some_and(|active| active != "session-a");
        assert!(
            stale_terminal_is_for_another_session,
            "the watcher must be able to tell A's terminal signal apart from B"
        );
        assert_eq!(st.observe().status.state, "recording");
        assert!(
            ticks_while(&st.recorder_state),
            "B's timer must keep running"
        );
    }

    #[test]
    fn an_unverified_snapshot_keeps_the_ticker_armed_for_a_retry() {
        let mut st = WatchState::new();
        assert!(!st.wants_tick(), "a verified idle state needs no ticker");

        st.reanchor_unknown();
        assert!(st.stale);
        assert!(
            st.wants_tick(),
            "an unverified state must keep retrying even though it is not recording"
        );

        st.adopt(&status_of("idle", "", 0));
        assert!(!st.stale, "a successful resync clears the retry");
        assert!(!st.wants_tick());
    }

    #[test]
    fn a_departed_daemon_stops_the_retry() {
        let mut st = WatchState::new();
        st.reanchor_unknown();
        st.current_session = Some("session-a".to_owned());
        assert!(st.wants_tick());

        st.daemon_gone();
        assert!(!st.stale, "idle is verified once the daemon is off the bus");
        assert!(!st.wants_tick());
        assert_eq!(st.current_session, None);
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
