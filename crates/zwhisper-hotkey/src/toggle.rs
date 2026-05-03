//! Toggle decision logic — debounce, cooldown, and the single
//! `Recorder1` call shared by the CLI (`zwhisper toggle`) and
//! the tray (`HotkeySession` listener task).
//!
//! ## Decision table (decision D1 in `docs/M6-plan.md`)
//!
//! Given a press event:
//!
//! 1. **Debounce / cooldown gate** — if we are inside the
//!    debounce window since the last accepted toggle, or inside
//!    the post-stop cooldown, fail with `ToggleError::Debounced`
//!    or `ToggleError::CoolingDown`.
//! 2. **Already-draining check (β-light window, A1 fix)** — if
//!    `active-session.json` is present AND
//!    `Recorder1.GetStatus` reports `Idle`, the daemon is
//!    transcribing the previous session. Surface this as
//!    `ToggleOutcome::NoOp { reason: AlreadyDraining }`. The
//!    cooldown bumps would normally catch this, but the file is
//!    the source-of-truth for cross-process resilience (e.g. a
//!    fresh tray instance that has no `last_stop` memory).
//! 3. **Currently recording** — if status is `Recording`, send
//!    `StopRecording(session_id)`, arm the cooldown, and return
//!    `Stopping`.
//! 4. **Idle and no draining** — fetch the active profile (D-Bus
//!    `Profiles1.GetActive`); empty result fails with
//!    `NoActiveProfile`. Otherwise call `StartRecording(profile)`
//!    and return `Started`.
//!
//! ## Daemon-down classification (`DoD #14`)
//!
//! `zbus::Error::MethodError` whose name is `ServiceUnknown` or
//! `NameHasNoOwner` -> `ToggleError::DaemonDown`. Plain
//! transport faults (`Address`, `InputOutput`,
//! disconnect-with-no-reply) likewise classify as `DaemonDown`
//! so the CLI's `notify-send` fallback fires once for any
//! daemon-not-on-bus failure mode. All other zbus errors
//! surface as `ToggleError::Rpc(message)`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use thiserror::Error;
use tracing::debug;
use zwhisper_ipc::{Profiles1Proxy, Recorder1Proxy};

use crate::active_session::{self, ActiveSessionRef};
use crate::config::HotkeyConfig;

/// Well-known D-Bus error names that mean the daemon is not on
/// the bus right now. Mirrors `zwhisper-cli::commands::mod`.
const ERR_SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";
const ERR_NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";

/// Snapshot of the daemon's recorder state, narrowed to what the
/// toggle decision needs. The wire `Status` only carries
/// `state | active_profile | duration_ms` — the `session_id`
/// portion of `Recording` comes from `active-session.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecorderStatus {
    /// Daemon is idle (or transcribing — caller distinguishes
    /// via `active_session::read_active_session`).
    Idle,
    /// Daemon is actively recording. `session_id` is sourced
    /// from `active-session.json` since `Status.session_id` is
    /// not part of the wire format.
    Recording { session_id: String },
}

/// Outcome of a successful toggle attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToggleOutcome {
    /// A fresh recording was started.
    Started { session_id: String, profile: String },
    /// An existing recording was stopped (transitioning into
    /// the drain window).
    Stopping { session_id: String },
    /// We declined to act — typically because the daemon is
    /// inside its post-stop drain window.
    NoOp { reason: NoOpReason },
}

/// Why the toggle decision returned `NoOp`. Kept as a typed
/// enum (vs. a bare string) so the CLI can pick a tailored
/// message + exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoOpReason {
    /// `active-session.json` present AND `Status` is `Idle`. The
    /// daemon is mid-transcribe; a fresh `StartRecording` would
    /// race the lifecycle.
    AlreadyDraining,
    /// We tried to `StartRecording` but the daemon answered with
    /// the typed `cz.zajca.Zwhisper1.Error.SessionInUse` error
    /// — a benign cross-process race (e.g. CLI + tray fired the
    /// chord at the same instant; the other party won). The
    /// recording IS running, just not started by us, so the
    /// caller should treat this as success.
    AlreadyActive,
    /// Catch-all for unforeseen "we should not act" branches.
    /// Currently unused but reserved so adding new branches does
    /// not require an enum bump elsewhere.
    Unknown,
}

/// Errors surfaced by the toggle decision. Each variant maps to
/// a distinct CLI exit code and tray notification message.
#[derive(Debug, Error)]
pub enum ToggleError {
    /// The press came in inside the debounce window.
    #[error("toggle debounced ({debounce_ms} ms)")]
    Debounced { debounce_ms: u64 },
    /// The press came in inside the post-stop cooldown window.
    #[error("post-stop cooldown active ({cooldown_ms} ms)")]
    CoolingDown { cooldown_ms: u64 },
    /// The daemon is not on the bus.
    #[error("daemon unreachable")]
    DaemonDown,
    /// `Profiles1.GetActive` returned an empty string and we
    /// have no profile to start a recording with.
    #[error("no active profile (set one with `zwhisper profile set <name>`)")]
    NoActiveProfile,
    /// Daemon returned the typed `SessionInUse` error during
    /// `StartRecording` — a benign concurrent-toggle race. Used
    /// only as a low-level signal between `from_zbus` and the
    /// `Idle` branch of [`toggle_once`], which folds it into
    /// `ToggleOutcome::NoOp { reason: AlreadyActive }` so callers
    /// never see it as a hard error. NOT a CLI exit-code variant.
    #[error("session already active (concurrent toggle)")]
    AlreadyActive,
    /// Anything else from zbus that did not classify above.
    #[error("rpc: {0}")]
    Rpc(String),
}

impl ToggleError {
    /// Map a `zbus::Error` to either `DaemonDown` (for the
    /// daemon-not-on-bus class of failures), `AlreadyActive`
    /// (for the typed `cz.zajca.Zwhisper1.Error.SessionInUse`
    /// concurrent-toggle race) or a string-bodied `Rpc` variant
    /// otherwise. See module docs for the classification policy.
    ///
    /// `SessionInUse` is teased out HERE (rather than at the
    /// `Idle` arm of `toggle_once`) so the typed daemon error is
    /// recovered before the body string is collapsed into the
    /// catch-all `Rpc(...)` form.
    fn from_zbus(err: &zbus::Error) -> Self {
        match err {
            zbus::Error::MethodError(name, ..) => {
                let name_str: &str = name.as_str();
                if name_str == ERR_SERVICE_UNKNOWN || name_str == ERR_NAME_HAS_NO_OWNER {
                    return Self::DaemonDown;
                }
                if matches!(
                    zwhisper_ipc::error::parse_error_name_from_zbus(err),
                    Some("SessionInUse")
                ) {
                    return Self::AlreadyActive;
                }
                Self::Rpc(err.to_string())
            }
            // Address parse / connection-not-established / I/O
            // failures during connect all mean the daemon is not
            // reachable.
            zbus::Error::Address(_) | zbus::Error::InputOutput(_) => Self::DaemonDown,
            zbus::Error::FDO(_) => {
                // `fdo::Error::Failed` is the wire shape used by
                // the daemon to encode typed errors (see
                // `zwhisper-ipc::error`). Recover the typed name
                // before falling through.
                if matches!(
                    zwhisper_ipc::error::parse_error_name_from_zbus(err),
                    Some("SessionInUse")
                ) {
                    return Self::AlreadyActive;
                }
                Self::Rpc(err.to_string())
            }
            _ => Self::Rpc(err.to_string()),
        }
    }
}

/// Time-based gate on toggle attempts. Tracks the most recent
/// accepted press (debounce window) and the most recent stop
/// (cooldown window) so `try_accept` can short-circuit
/// rapid-fire input deterministically.
#[derive(Debug)]
pub struct Debouncer {
    last_accept: Option<Instant>,
    last_stop: Option<Instant>,
    debounce: Duration,
    cooldown: Duration,
}

impl Debouncer {
    #[must_use]
    pub fn new(cfg: &HotkeyConfig) -> Self {
        Self {
            last_accept: None,
            last_stop: None,
            debounce: Duration::from_millis(cfg.debounce_ms),
            cooldown: Duration::from_millis(cfg.cooldown_ms),
        }
    }

    /// Try to accept a press at the given instant.
    ///
    /// Cooldown takes priority over debounce in the error
    /// classification — when both windows are active, the
    /// caller almost certainly cares about the cooldown
    /// (post-stop) more than the debounce (rapid retry).
    ///
    /// On accept, `last_accept` is updated so subsequent
    /// presses inside `debounce` are rejected.
    pub fn try_accept(&mut self, now: Instant) -> Result<(), ToggleError> {
        if let Some(stop_at) = self.last_stop {
            if let Some(elapsed) = now.checked_duration_since(stop_at) {
                if elapsed < self.cooldown {
                    return Err(ToggleError::CoolingDown {
                        cooldown_ms: u64::try_from(self.cooldown.as_millis()).unwrap_or(u64::MAX),
                    });
                }
            }
        }
        if let Some(accept_at) = self.last_accept {
            if let Some(elapsed) = now.checked_duration_since(accept_at) {
                if elapsed < self.debounce {
                    return Err(ToggleError::Debounced {
                        debounce_ms: u64::try_from(self.debounce.as_millis()).unwrap_or(u64::MAX),
                    });
                }
            }
        }
        self.last_accept = Some(now);
        Ok(())
    }

    /// Arm the post-stop cooldown. Called by `toggle_once`
    /// after a successful `StopRecording` and never directly
    /// by the listener.
    pub fn note_stop(&mut self, now: Instant) {
        self.last_stop = Some(now);
    }
}

/// Trait the toggle decision uses to talk to the daemon. The
/// production impl wraps a real `Recorder1Proxy` /
/// `Profiles1Proxy` pair; tests inject a `MockRecorder` so they
/// run without a live D-Bus.
#[async_trait]
pub trait RecorderClient: Send + Sync {
    async fn get_status(&self) -> Result<RecorderStatus, ToggleError>;
    /// Returns the active profile name, or empty string when no
    /// profile is set. Empty string is the daemon's documented
    /// "no profile yet" sentinel for `Profiles1.GetActive`.
    async fn get_active_profile(&self) -> Result<String, ToggleError>;
    async fn start_recording(&self, profile: &str) -> Result<String, ToggleError>;
    async fn stop_recording(&self, session_id: &str) -> Result<(), ToggleError>;
}

/// Production impl: bundle the two proxies the toggle needs so
/// callers do not have to plumb both through every layer.
#[derive(Debug)]
pub struct LiveRecorderClient<'a> {
    pub recorder: Recorder1Proxy<'a>,
    pub profiles: Profiles1Proxy<'a>,
}

impl<'a> LiveRecorderClient<'a> {
    /// Construct from already-built proxies. The caller usually
    /// builds these from a single `zbus::Connection`.
    #[must_use]
    pub fn new(recorder: Recorder1Proxy<'a>, profiles: Profiles1Proxy<'a>) -> Self {
        Self { recorder, profiles }
    }
}

#[async_trait]
impl RecorderClient for LiveRecorderClient<'_> {
    async fn get_status(&self) -> Result<RecorderStatus, ToggleError> {
        let status = self
            .recorder
            .get_status()
            .await
            .map_err(|e| ToggleError::from_zbus(&e))?;
        // `state` is one of the strings rendered by
        // `RecorderState::Display`. Anything other than
        // "recording" is treated as "not recording" for the
        // toggle decision; the β-light window is detected via
        // the active-session file in `toggle_once`.
        if status.state == "recording" {
            // The wire-frozen Status struct does not carry the
            // `session_id`, so we have to read it from the
            // state file. If the file is missing here the
            // daemon is in an inconsistent state — surface as
            // `Rpc` rather than silently falling through to
            // start-a-new-recording.
            match active_session::read_active_session() {
                Some(active) => Ok(RecorderStatus::Recording {
                    session_id: active.session_id,
                }),
                None => Err(ToggleError::Rpc(
                    "daemon reports recording but active-session.json is missing".to_owned(),
                )),
            }
        } else {
            Ok(RecorderStatus::Idle)
        }
    }

    async fn get_active_profile(&self) -> Result<String, ToggleError> {
        self.profiles
            .get_active()
            .await
            .map_err(|e| ToggleError::from_zbus(&e))
    }

    async fn start_recording(&self, profile: &str) -> Result<String, ToggleError> {
        self.recorder
            .start_recording(profile)
            .await
            .map_err(|e| ToggleError::from_zbus(&e))
    }

    async fn stop_recording(&self, session_id: &str) -> Result<(), ToggleError> {
        self.recorder
            .stop_recording(session_id)
            .await
            .map(|_returned_id| ())
            .map_err(|e| ToggleError::from_zbus(&e))
    }
}

/// Run one toggle decision against the supplied client.
///
/// Time is read once at the start of the call (`Instant::now()`)
/// and threaded through both the debouncer gate and the
/// post-stop arming, so a long-running RPC inside the call does
/// NOT extend the cooldown beyond its 1500 ms default.
///
/// ## Cross-process resilience
///
/// The β-light `NoOp` branch (`active-session.json` present,
/// `Status == Idle`) still fires even if the in-memory
/// debouncer has no recent `last_stop` — e.g. a fresh tray
/// instance that just attached. This is the whole point of D1:
/// the file is the source-of-truth, the cooldown is the
/// best-effort fast path.
pub async fn toggle_once<C: RecorderClient + ?Sized>(
    client: &C,
    debouncer: &mut Debouncer,
) -> Result<ToggleOutcome, ToggleError> {
    toggle_once_with_reader(client, debouncer, active_session::read_active_session).await
}

/// Test-friendly variant of [`toggle_once`] that accepts an
/// injectable active-session reader. Production code uses
/// [`toggle_once`], which calls
/// [`active_session::read_active_session`] directly. Tests pass
/// a closure so they do not need to mutate `$XDG_STATE_HOME`
/// (which would race when `cargo test` runs cases in parallel).
pub async fn toggle_once_with_reader<C, R>(
    client: &C,
    debouncer: &mut Debouncer,
    read_active: R,
) -> Result<ToggleOutcome, ToggleError>
where
    C: RecorderClient + ?Sized,
    R: Fn() -> Option<ActiveSessionRef>,
{
    let now = Instant::now();
    debouncer.try_accept(now)?;

    let status = client.get_status().await?;
    let active_session_present = read_active().is_some();

    // β-light: file present AND status is idle -> daemon is
    // mid-transcribe. Decline to act.
    if matches!(status, RecorderStatus::Idle) && active_session_present {
        debug!("toggle: NoOp because daemon is draining (active-session.json present)");
        return Ok(ToggleOutcome::NoOp {
            reason: NoOpReason::AlreadyDraining,
        });
    }

    match status {
        RecorderStatus::Recording { session_id } => {
            client.stop_recording(&session_id).await?;
            debouncer.note_stop(Instant::now());
            Ok(ToggleOutcome::Stopping { session_id })
        }
        RecorderStatus::Idle => {
            let profile = client.get_active_profile().await?;
            if profile.is_empty() {
                return Err(ToggleError::NoActiveProfile);
            }
            match client.start_recording(&profile).await {
                Ok(session_id) => Ok(ToggleOutcome::Started {
                    session_id,
                    profile,
                }),
                // Concurrent-toggle race: another caller (CLI vs
                // tray, double-press across processes, …) already
                // had StartRecording in flight and won. The
                // recording IS running — just not started by us.
                // Treat as a benign no-op so the user sees no
                // exit-3 noise.
                Err(ToggleError::AlreadyActive) => {
                    debug!(
                        "toggle: NoOp because daemon answered SessionInUse (concurrent toggle race)"
                    );
                    Ok(ToggleOutcome::NoOp {
                        reason: NoOpReason::AlreadyActive,
                    })
                }
                Err(e) => Err(e),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Track of every call the test harness made on the mock.
    /// Useful for asserting "we issued a `stop_recording` with the
    /// expected session id" without inspecting argument tuples
    /// in the test body.
    #[derive(Debug, Default)]
    struct CallLog {
        starts: Vec<String>,
        stops: Vec<String>,
        get_status_count: usize,
        get_active_profile_count: usize,
    }

    /// In-memory stand-in for `LiveRecorderClient`. Each method
    /// returns a programmable result; the constructor wraps the
    /// settings in `Mutex` so tests can mutate them between
    /// `toggle_once` calls.
    struct MockRecorder {
        next_status: Mutex<Result<RecorderStatus, ToggleError>>,
        next_profile: Mutex<Result<String, ToggleError>>,
        next_start: Mutex<Result<String, ToggleError>>,
        next_stop: Mutex<Result<(), ToggleError>>,
        log: Mutex<CallLog>,
    }

    impl MockRecorder {
        fn idle_with_profile(profile: &str) -> Self {
            Self {
                next_status: Mutex::new(Ok(RecorderStatus::Idle)),
                next_profile: Mutex::new(Ok(profile.to_owned())),
                next_start: Mutex::new(Ok("session-001".to_owned())),
                next_stop: Mutex::new(Ok(())),
                log: Mutex::new(CallLog::default()),
            }
        }

        fn recording(session_id: &str) -> Self {
            Self {
                next_status: Mutex::new(Ok(RecorderStatus::Recording {
                    session_id: session_id.to_owned(),
                })),
                next_profile: Mutex::new(Ok("default".to_owned())),
                next_start: Mutex::new(Ok("never-called".to_owned())),
                next_stop: Mutex::new(Ok(())),
                log: Mutex::new(CallLog::default()),
            }
        }

        fn set_status(&self, s: Result<RecorderStatus, ToggleError>) {
            *self.next_status.lock().unwrap() = s;
        }

        fn set_profile(&self, p: Result<String, ToggleError>) {
            *self.next_profile.lock().unwrap() = p;
        }

        fn set_start(&self, p: Result<String, ToggleError>) {
            *self.next_start.lock().unwrap() = p;
        }
    }

    fn clone_result_status(
        r: &Result<RecorderStatus, ToggleError>,
    ) -> Result<RecorderStatus, ToggleError> {
        match r {
            Ok(v) => Ok(v.clone()),
            Err(ToggleError::DaemonDown) => Err(ToggleError::DaemonDown),
            Err(ToggleError::NoActiveProfile) => Err(ToggleError::NoActiveProfile),
            Err(ToggleError::Debounced { debounce_ms }) => Err(ToggleError::Debounced {
                debounce_ms: *debounce_ms,
            }),
            Err(ToggleError::CoolingDown { cooldown_ms }) => Err(ToggleError::CoolingDown {
                cooldown_ms: *cooldown_ms,
            }),
            Err(ToggleError::AlreadyActive) => Err(ToggleError::AlreadyActive),
            Err(ToggleError::Rpc(msg)) => Err(ToggleError::Rpc(msg.clone())),
        }
    }

    fn clone_result_string(r: &Result<String, ToggleError>) -> Result<String, ToggleError> {
        match r {
            Ok(s) => Ok(s.clone()),
            Err(ToggleError::DaemonDown) => Err(ToggleError::DaemonDown),
            Err(ToggleError::NoActiveProfile) => Err(ToggleError::NoActiveProfile),
            Err(ToggleError::Debounced { debounce_ms }) => Err(ToggleError::Debounced {
                debounce_ms: *debounce_ms,
            }),
            Err(ToggleError::CoolingDown { cooldown_ms }) => Err(ToggleError::CoolingDown {
                cooldown_ms: *cooldown_ms,
            }),
            Err(ToggleError::AlreadyActive) => Err(ToggleError::AlreadyActive),
            Err(ToggleError::Rpc(msg)) => Err(ToggleError::Rpc(msg.clone())),
        }
    }

    fn clone_result_unit(r: &Result<(), ToggleError>) -> Result<(), ToggleError> {
        match r {
            Ok(()) => Ok(()),
            Err(ToggleError::DaemonDown) => Err(ToggleError::DaemonDown),
            Err(ToggleError::NoActiveProfile) => Err(ToggleError::NoActiveProfile),
            Err(ToggleError::Debounced { debounce_ms }) => Err(ToggleError::Debounced {
                debounce_ms: *debounce_ms,
            }),
            Err(ToggleError::CoolingDown { cooldown_ms }) => Err(ToggleError::CoolingDown {
                cooldown_ms: *cooldown_ms,
            }),
            Err(ToggleError::AlreadyActive) => Err(ToggleError::AlreadyActive),
            Err(ToggleError::Rpc(msg)) => Err(ToggleError::Rpc(msg.clone())),
        }
    }

    #[async_trait]
    impl RecorderClient for MockRecorder {
        async fn get_status(&self) -> Result<RecorderStatus, ToggleError> {
            self.log.lock().unwrap().get_status_count += 1;
            clone_result_status(&self.next_status.lock().unwrap())
        }

        async fn get_active_profile(&self) -> Result<String, ToggleError> {
            self.log.lock().unwrap().get_active_profile_count += 1;
            clone_result_string(&self.next_profile.lock().unwrap())
        }

        async fn start_recording(&self, profile: &str) -> Result<String, ToggleError> {
            self.log.lock().unwrap().starts.push(profile.to_owned());
            clone_result_string(&self.next_start.lock().unwrap())
        }

        async fn stop_recording(&self, session_id: &str) -> Result<(), ToggleError> {
            self.log.lock().unwrap().stops.push(session_id.to_owned());
            clone_result_unit(&self.next_stop.lock().unwrap())
        }
    }

    fn cfg_default() -> HotkeyConfig {
        HotkeyConfig::default()
    }

    /// Active-session reader stub: returns `None` (the steady-
    /// state, no-recording case). Most tests use this so they
    /// don't pick up a real `active-session.json` from the
    /// developer's `$XDG_STATE_HOME`.
    fn no_active_session() -> Option<ActiveSessionRef> {
        None
    }

    /// Active-session reader stub: returns `Some(...)` to
    /// simulate the β-light drain window where the file is
    /// present but `Status` is `Idle`. Wrapped at the call site
    /// (`|| Some(fixture_active_session())`) so the closure
    /// signature matches the production reader.
    fn fixture_active_session() -> ActiveSessionRef {
        ActiveSessionRef {
            session_id: "drain-sid".to_owned(),
            profile: "default".to_owned(),
            started_at: chrono::DateTime::<chrono::Utc>::from_timestamp_millis(1)
                .expect("epoch+1ms is valid"),
        }
    }

    // ============================================================
    // Debouncer-only tests (no MockRecorder needed)
    // ============================================================

    #[test]
    fn debouncer_first_press_accepts() {
        let mut d = Debouncer::new(&cfg_default());
        let now = Instant::now();
        assert!(d.try_accept(now).is_ok());
    }

    #[test]
    fn debouncer_rejects_press_inside_debounce_window() {
        let mut d = Debouncer::new(&cfg_default());
        let now = Instant::now();
        d.try_accept(now).unwrap();
        let later = now + Duration::from_millis(100);
        match d.try_accept(later) {
            Err(ToggleError::Debounced { debounce_ms }) => {
                assert_eq!(debounce_ms, crate::config::DEFAULT_DEBOUNCE_MS);
            }
            other => panic!("expected Debounced, got {other:?}"),
        }
    }

    #[test]
    fn debouncer_accepts_after_debounce_window_elapses() {
        let mut d = Debouncer::new(&cfg_default());
        let now = Instant::now();
        d.try_accept(now).unwrap();
        let later = now + Duration::from_millis(crate::config::DEFAULT_DEBOUNCE_MS + 1);
        assert!(d.try_accept(later).is_ok());
    }

    #[test]
    fn debouncer_cooldown_takes_priority_over_debounce() {
        let mut d = Debouncer::new(&cfg_default());
        let t0 = Instant::now();
        d.note_stop(t0);
        let later = t0 + Duration::from_millis(50);
        match d.try_accept(later) {
            Err(ToggleError::CoolingDown { cooldown_ms }) => {
                assert_eq!(cooldown_ms, crate::config::DEFAULT_COOLDOWN_MS);
            }
            other => panic!("expected CoolingDown, got {other:?}"),
        }
    }

    #[test]
    fn debouncer_cooldown_expires_after_window() {
        let mut d = Debouncer::new(&cfg_default());
        let t0 = Instant::now();
        d.note_stop(t0);
        let later = t0 + Duration::from_millis(crate::config::DEFAULT_COOLDOWN_MS + 1);
        assert!(d.try_accept(later).is_ok());
    }

    // ============================================================
    // toggle_once decision-table tests
    // ============================================================

    #[tokio::test]
    async fn idle_starts_recording_with_active_profile() {
        let mock = MockRecorder::idle_with_profile("default");
        let mut d = Debouncer::new(&cfg_default());

        let outcome = toggle_once_with_reader(&mock, &mut d, no_active_session)
            .await
            .unwrap();

        match outcome {
            ToggleOutcome::Started {
                session_id,
                profile,
            } => {
                assert_eq!(session_id, "session-001");
                assert_eq!(profile, "default");
            }
            other => panic!("expected Started, got {other:?}"),
        }
        assert_eq!(mock.log.lock().unwrap().starts, vec!["default".to_owned()]);
        assert!(mock.log.lock().unwrap().stops.is_empty());
    }

    #[tokio::test]
    async fn idle_with_active_session_file_is_no_op_already_draining() {
        // The β-light window: daemon's `Status` reads `Idle`
        // (transcribe phase), but `active-session.json` is
        // still on disk. A toggle here MUST decline to act —
        // a fresh `StartRecording` would race the lifecycle
        // of the previous session. Decision D1 in M6-plan.md.
        let mock = MockRecorder::idle_with_profile("default");
        let mut d = Debouncer::new(&cfg_default());

        let outcome = toggle_once_with_reader(&mock, &mut d, || Some(fixture_active_session()))
            .await
            .unwrap();

        match outcome {
            ToggleOutcome::NoOp { reason } => {
                assert_eq!(reason, NoOpReason::AlreadyDraining);
            }
            other => panic!("expected NoOp(AlreadyDraining), got {other:?}"),
        }
        // Critically: must not have started a new recording.
        assert!(mock.log.lock().unwrap().starts.is_empty());
        assert!(mock.log.lock().unwrap().stops.is_empty());
    }

    #[tokio::test]
    async fn recording_stops_session() {
        let mock = MockRecorder::recording("session-zzz");
        let mut d = Debouncer::new(&cfg_default());

        let outcome = toggle_once_with_reader(&mock, &mut d, no_active_session)
            .await
            .unwrap();

        match outcome {
            ToggleOutcome::Stopping { session_id } => {
                assert_eq!(session_id, "session-zzz");
            }
            other => panic!("expected Stopping, got {other:?}"),
        }
        assert_eq!(
            mock.log.lock().unwrap().stops,
            vec!["session-zzz".to_owned()],
        );
        assert!(mock.log.lock().unwrap().starts.is_empty());
    }

    #[tokio::test]
    async fn stop_arms_cooldown() {
        let mock = MockRecorder::recording("sid-stop-arms-cd");
        let mut d = Debouncer::new(&cfg_default());

        // First press: stop the recording.
        toggle_once_with_reader(&mock, &mut d, no_active_session)
            .await
            .unwrap();

        // Second press inside the cooldown window: must reject.
        // We advance the wall-clock by 100 ms via the debouncer
        // contract — `note_stop` was called from inside
        // `toggle_once`, so any subsequent `try_accept` inside
        // the cooldown window must fail.
        let later = Instant::now() + Duration::from_millis(100);
        match d.try_accept(later) {
            Err(ToggleError::CoolingDown { cooldown_ms }) => {
                assert_eq!(cooldown_ms, crate::config::DEFAULT_COOLDOWN_MS);
            }
            other => panic!("expected CoolingDown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cooldown_expires_after_window() {
        let mut d = Debouncer::new(&cfg_default());
        let t0 = Instant::now();
        d.note_stop(t0);
        let after_window = t0 + Duration::from_millis(crate::config::DEFAULT_COOLDOWN_MS + 1);
        assert!(d.try_accept(after_window).is_ok());
    }

    #[tokio::test]
    async fn rapid_double_press_debounced() {
        let mock = MockRecorder::idle_with_profile("default");
        let mut d = Debouncer::new(&cfg_default());

        // First press accepts.
        toggle_once_with_reader(&mock, &mut d, no_active_session)
            .await
            .unwrap();

        // Rewire mock so a second `Idle` status read still
        // looks plausible (the daemon would not actually flip
        // back to idle this fast, but the gate runs before the
        // status read).
        // Second press in <250ms returns Debounced.
        let immediately =
            Instant::now() + Duration::from_millis(crate::config::DEFAULT_DEBOUNCE_MS / 2);
        match d.try_accept(immediately) {
            Err(ToggleError::Debounced { .. }) => {}
            other => panic!("expected Debounced, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_active_profile_errors() {
        let mock = MockRecorder::idle_with_profile("");
        let mut d = Debouncer::new(&cfg_default());

        match toggle_once_with_reader(&mock, &mut d, no_active_session).await {
            Err(ToggleError::NoActiveProfile) => {}
            other => panic!("expected NoActiveProfile, got {other:?}"),
        }
        // A recording must NOT be started when the profile
        // resolution returned empty.
        assert!(mock.log.lock().unwrap().starts.is_empty());
    }

    #[tokio::test]
    async fn daemon_unreachable_classifies_as_daemon_down() {
        let mock = MockRecorder::idle_with_profile("default");
        // GetStatus is the first RPC on the path; injecting a
        // DaemonDown there exercises the classification.
        mock.set_status(Err(ToggleError::DaemonDown));
        let mut d = Debouncer::new(&cfg_default());

        match toggle_once_with_reader(&mock, &mut d, no_active_session).await {
            Err(ToggleError::DaemonDown) => {}
            other => panic!("expected DaemonDown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rpc_error_propagates_with_message() {
        let mock = MockRecorder::idle_with_profile("default");
        mock.set_profile(Err(ToggleError::Rpc("boom".into())));
        let mut d = Debouncer::new(&cfg_default());

        match toggle_once_with_reader(&mock, &mut d, no_active_session).await {
            Err(ToggleError::Rpc(msg)) => assert_eq!(msg, "boom"),
            other => panic!("expected Rpc, got {other:?}"),
        }
    }

    // ============================================================
    // from_zbus mapping
    // ============================================================

    fn synthetic_method_error(name: &'static str) -> zbus::Error {
        let placeholder = zbus::Message::signal("/", "test.Iface", "Sig")
            .expect("builder")
            .build(&())
            .expect("build");
        zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from(name).expect("valid name"),
            None,
            placeholder,
        )
    }

    #[test]
    fn from_zbus_service_unknown_classifies_as_daemon_down() {
        let err = synthetic_method_error("org.freedesktop.DBus.Error.ServiceUnknown");
        assert!(matches!(
            ToggleError::from_zbus(&err),
            ToggleError::DaemonDown,
        ));
    }

    #[test]
    fn from_zbus_name_has_no_owner_classifies_as_daemon_down() {
        let err = synthetic_method_error("org.freedesktop.DBus.Error.NameHasNoOwner");
        assert!(matches!(
            ToggleError::from_zbus(&err),
            ToggleError::DaemonDown,
        ));
    }

    #[test]
    fn from_zbus_other_method_error_falls_through_to_rpc() {
        // Pick an error name that is neither the daemon-down
        // class NOR a known typed daemon error, so it must
        // surface as the catch-all `Rpc(...)` variant.
        let err = synthetic_method_error("org.example.SomeOther.Error");
        assert!(matches!(ToggleError::from_zbus(&err), ToggleError::Rpc(_)));
    }

    #[test]
    fn from_zbus_session_in_use_classifies_as_already_active() {
        // The daemon encodes typed errors via `fdo::Error::Failed`
        // with a `cz.zajca.Zwhisper1.Error.<Variant>: <msg>` body
        // prefix (see `zwhisper-ipc::error`). Receivers see this
        // as `MethodError("Failed", Some(body), …)`. The toggle
        // path must NOT classify this race as an RPC failure.
        let placeholder = zbus::Message::signal("/", "test.Iface", "Sig")
            .expect("builder")
            .build(&())
            .expect("build");
        let err = zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from("org.freedesktop.DBus.Error.Failed")
                .expect("valid name"),
            Some("cz.zajca.Zwhisper1.Error.SessionInUse: a recording session is already active (id=abc)".to_string()),
            placeholder,
        );
        assert!(matches!(
            ToggleError::from_zbus(&err),
            ToggleError::AlreadyActive
        ));
    }

    #[tokio::test]
    async fn start_recording_session_in_use_classified_as_no_op_already_active() {
        // End-to-end: the Idle arm of `toggle_once` must catch the
        // typed `SessionInUse` error and surface it as the benign
        // `NoOp { reason: AlreadyActive }` outcome instead of
        // exit-3 noise.
        let mock = MockRecorder::idle_with_profile("default");
        mock.set_start(Err(ToggleError::AlreadyActive));
        let mut d = Debouncer::new(&cfg_default());

        let outcome = toggle_once_with_reader(&mock, &mut d, no_active_session)
            .await
            .expect("AlreadyActive must fold into NoOp, not bubble as error");

        match outcome {
            ToggleOutcome::NoOp { reason } => {
                assert_eq!(reason, NoOpReason::AlreadyActive);
            }
            other => panic!("expected NoOp(AlreadyActive), got {other:?}"),
        }
        // The `StartRecording` call MUST have been attempted —
        // we only learn about `SessionInUse` after trying.
        assert_eq!(mock.log.lock().unwrap().starts, vec!["default".to_owned()]);
    }

    #[test]
    fn from_zbus_address_error_classifies_as_daemon_down() {
        let err = zbus::Error::Address("not a valid address".into());
        assert!(matches!(
            ToggleError::from_zbus(&err),
            ToggleError::DaemonDown,
        ));
    }
}
