// `expect` is used here only when re-locking the session manager's
// own mutex; a poisoned mutex means an earlier panic in our own
// code, the daemon is unrecoverable, and propagating via `expect`
// keeps the error type set small (mirrors the pattern in
// `zwhisper_core::audio::recorder`).
#![allow(clippy::expect_used)]

//! Single-session policy state machine.
//!
//! M3 ships a daemon that runs **at most one** recording at a time
//! (M3-plan § "Single-session policy"). The session manager is the
//! exclusive guardian of that invariant: every `StartRecording`
//! D-Bus call goes through [`SessionManager::try_reserve`], every
//! lifecycle-task exit (including failures) goes through
//! [`SessionManager::release`].
//!
//! Per stress-test correction C5, the slot is released **before** the
//! transcribe step starts, so a follow-up `StartRecording` arriving
//! during transcription succeeds. The transcribe future runs after
//! release and emits its `TranscriptComplete` signal under the
//! original `session_id` regardless of which session is active by
//! then.

use std::sync::{Arc, Mutex};

use zwhisper_core::audio::state::{SessionId, StopReason};
use zwhisper_ipc::RpcError;

/// Type-erased "stop" hook installed by the lifecycle task. When
/// `Recorder1.StopRecording` lands, the daemon calls this with the
/// requested [`StopReason`]; the hook forwards it into the recorder's
/// own `tokio::sync::watch` channel via `Recorder::request_stop`.
pub(crate) type StopHook = Arc<dyn Fn(StopReason) + Send + Sync + 'static>;

/// One in-flight recording. The `recorder` is consumed by the
/// lifecycle task when it calls `Recorder::await_completion`, so we
/// keep a copy of the bookkeeping on this struct (`session_id`,
/// `profile_name`, `started_at`) for `GetStatus`.
pub(crate) struct ActiveSession {
    pub(crate) session_id: SessionId,
    pub(crate) profile_name: String,
    pub(crate) started_at: std::time::Instant,
    /// Hook the lifecycle task installs at spawn time so
    /// `StopRecording` (and the SIGTERM handler) can ask the
    /// recorder to drain without holding the recorder itself.
    pub(crate) stop_hook: Option<StopHook>,
}

impl std::fmt::Debug for ActiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveSession")
            .field("session_id", &self.session_id)
            .field("profile_name", &self.profile_name)
            .field("started_at", &self.started_at)
            .field("stop_hook", &self.stop_hook.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

/// Holds the at-most-one active session. The internal mutex is a
/// std `Mutex` (not tokio) because critical sections are tiny: a
/// single `Option` swap. Holding a `tokio::sync::Mutex` across
/// `await` points adds nothing here and would let the lock straddle
/// signal-emission `await`s.
#[derive(Debug, Default)]
pub(crate) struct SessionManager {
    inner: Mutex<Option<ActiveSession>>,
}

impl SessionManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Attempt to claim the single session slot. On success the
    /// caller owns the slot until [`SessionManager::release`] is
    /// called. On contention returns
    /// [`RpcError::SessionInUse`] carrying the existing session id.
    pub(crate) fn try_reserve(
        &self,
        session_id: SessionId,
        profile_name: &str,
    ) -> Result<(), RpcError> {
        let mut slot = self
            .inner
            .lock()
            .expect("session manager mutex poisoned");
        if let Some(existing) = slot.as_ref() {
            return Err(RpcError::SessionInUse {
                existing: existing.session_id.to_string(),
            });
        }
        *slot = Some(ActiveSession {
            session_id,
            profile_name: profile_name.to_owned(),
            started_at: std::time::Instant::now(),
            stop_hook: None,
        });
        Ok(())
    }

    /// Install the stop hook for the currently-active session. Must
    /// be called immediately after `try_reserve` succeeds and before
    /// the lifecycle task moves the recorder into `spawn_blocking`.
    /// Silently no-ops when no session is active (defensive — the
    /// only realistic caller is the lifecycle task itself, which
    /// just reserved the slot).
    pub(crate) fn install_stop_hook(&self, hook: StopHook) {
        let mut slot = self
            .inner
            .lock()
            .expect("session manager mutex poisoned");
        if let Some(session) = slot.as_mut() {
            session.stop_hook = Some(hook);
        }
    }

    /// Forward a `StopReason` to the active session's stop hook.
    /// Idempotent — calling twice in a row just sends the reason
    /// twice; the recorder's `watch::Sender::send_replace` collapses
    /// them. Returns `false` when no session is active so the caller
    /// can decide whether to surface `RpcError::SessionUnknown`.
    pub(crate) fn request_stop_active(&self, reason: StopReason) -> bool {
        // Clone the hook out under the lock so the actual fn call
        // happens after we drop the mutex; the hook itself runs the
        // recorder's `request_stop` which only touches a channel.
        let hook = {
            let slot = self
                .inner
                .lock()
                .expect("session manager mutex poisoned");
            slot.as_ref().and_then(|s| s.stop_hook.clone())
        };
        match hook {
            Some(f) => {
                f(reason);
                true
            }
            None => false,
        }
    }

    /// Release the session slot. Idempotent — releasing an empty
    /// slot is silently a no-op so the lifecycle task can call this
    /// from both the success and failure branches without
    /// double-bookkeeping.
    pub(crate) fn release(&self) {
        let mut slot = self
            .inner
            .lock()
            .expect("session manager mutex poisoned");
        *slot = None;
    }

    /// Snapshot of the active session (if any). Cloned out so the
    /// lock is released before the caller does any signal emission
    /// or other awaitable work.
    pub(crate) fn snapshot(&self) -> Option<SessionSnapshot> {
        let slot = self
            .inner
            .lock()
            .expect("session manager mutex poisoned");
        slot.as_ref().map(|s| SessionSnapshot {
            session_id: s.session_id,
            started_at: s.started_at,
        })
    }

    /// Returns whether the supplied `id` matches the currently active
    /// session. Used by `StopRecording` to validate the caller.
    pub(crate) fn matches(&self, id: &SessionId) -> bool {
        let slot = self
            .inner
            .lock()
            .expect("session manager mutex poisoned");
        slot.as_ref().is_some_and(|s| s.session_id == *id)
    }
}

/// Lock-free copy of an [`ActiveSession`]'s fields. Returned by
/// [`SessionManager::snapshot`] so callers can read state without
/// holding the manager's mutex across an `await`.
#[derive(Debug, Clone)]
pub(crate) struct SessionSnapshot {
    /// Session id of the active recording. Currently used only by
    /// `Recorder1.GetStatus` for log correlation; kept on the
    /// snapshot so future status fields (e.g. profile name) have a
    /// natural home without re-locking.
    #[allow(dead_code)]
    pub(crate) session_id: SessionId,
    pub(crate) started_at: std::time::Instant,
}


#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn try_reserve_succeeds_when_slot_empty() {
        let mgr = SessionManager::new();
        let id = SessionId::new();
        mgr.try_reserve(id, "default").unwrap();
        assert!(mgr.snapshot().is_some());
    }

    #[test]
    fn second_try_reserve_returns_session_in_use() {
        let mgr = SessionManager::new();
        let first = SessionId::new();
        mgr.try_reserve(first, "default").unwrap();
        let second = SessionId::new();
        let err = mgr.try_reserve(second, "default").unwrap_err();
        match err {
            RpcError::SessionInUse { existing } => {
                assert_eq!(existing, first.to_string());
            }
            other => panic!("expected SessionInUse, got {other:?}"),
        }
    }

    #[test]
    fn release_then_reserve_succeeds() {
        let mgr = SessionManager::new();
        let first = SessionId::new();
        mgr.try_reserve(first, "default").unwrap();
        mgr.release();
        assert!(mgr.snapshot().is_none());
        let second = SessionId::new();
        mgr.try_reserve(second, "default").unwrap();
        assert!(mgr.snapshot().is_some());
    }

    #[test]
    fn release_on_empty_slot_is_noop() {
        let mgr = SessionManager::new();
        mgr.release(); // should not panic
        assert!(mgr.snapshot().is_none());
    }

    #[test]
    fn matches_returns_true_for_active_id_only() {
        let mgr = SessionManager::new();
        let id = SessionId::new();
        mgr.try_reserve(id, "default").unwrap();
        assert!(mgr.matches(&id));
        let other = SessionId::new();
        assert!(!mgr.matches(&other));
    }

    #[test]
    fn matches_returns_false_when_no_session() {
        let mgr = SessionManager::new();
        let id = SessionId::new();
        assert!(!mgr.matches(&id));
    }
}
