//! XDG `GlobalShortcuts` portal adapter (`ashpd`) plus the in-process
//! fake used by tests.
//!
//! Group C of M6: implements the [`PortalAdapter`] trait, the real
//! [`AshpdAdapter`] backed by `ashpd 0.13`, the in-memory [`FakePortal`]
//! used by every test in this crate (and reusable cross-crate via the
//! `test-fakes` feature), and the high-level [`HotkeySession`] wrapper
//! that owns a session handle and exposes a single `next_event` stream.
//!
//! ### Sender validation (`DoD` #8 / risk G1)
//! In production the `ashpd::desktop::global_shortcuts::GlobalShortcuts`
//! proxy is constructed against the well-known bus name
//! `org.freedesktop.portal.Desktop`. zbus filters incoming D-Bus signals
//! by the proxy's destination/sender, so a malicious peer publishing
//! `Activated` from a different unique name is dropped before our task
//! ever sees it. [`FakePortal`] models that contract via
//! [`FakePortal::enable_sender_filter`] and is exercised by
//! `activated_signal_from_unexpected_sender_dropped`.

#![cfg(feature = "portal")]

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, broadcast};

// ---------------------------------------------------------------------
// Public constants — used by tray (Group E) and CLI (Group F).
// ---------------------------------------------------------------------

/// Stable shortcut id passed to the portal. Must remain stable across
/// upgrades — changing it invalidates every user's saved binding.
pub const SHORTCUT_ID: &str = "toggle-recording";

/// User-facing description shown in the portal's bind dialog.
pub const SHORTCUT_DESCRIPTION: &str = "Toggle zwhisper recording";

// ---------------------------------------------------------------------
// Trait surface
// ---------------------------------------------------------------------

/// Information about a shortcut currently bound by the compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundShortcut {
    /// Application-side id (e.g. [`SHORTCUT_ID`]).
    pub id: String,
    /// Human-readable trigger ("Ctrl+Alt+R") returned by the portal.
    pub trigger_description: String,
    /// User-readable description we passed in [`BindRequest::description`].
    pub description: String,
}

/// Single-shortcut bind request. M6 only ever binds one shortcut, but
/// the API takes a request struct so we don't churn callers when M7
/// adds more.
#[derive(Debug, Clone)]
pub struct BindRequest {
    /// Application-side id, typically [`SHORTCUT_ID`].
    pub id: String,
    /// User-facing description.
    pub description: String,
    /// Preferred trigger in the XDG "shortcuts" syntax (e.g.
    /// `"CTRL+ALT+r"`). The portal MAY ignore this and pop a UI.
    pub preferred_trigger: Option<String>,
}

/// Events surfaced from the portal session. The tray's listener task
/// (Group E) consumes this stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// Shortcut was activated.
    Activated {
        /// Application-side id.
        shortcut_id: String,
        /// Portal-supplied timestamp (millis since epoch). `None` if
        /// the source signal didn't carry one (fakes / test paths).
        timestamp: Option<u64>,
    },
    /// Shortcut was released.
    Deactivated {
        /// Application-side id.
        shortcut_id: String,
    },
    /// The compositor reconfigured one or more shortcuts; the listener
    /// should re-fetch [`PortalAdapter::list_shortcuts`].
    ShortcutsChanged,
}

/// Errors returned by every [`PortalAdapter`] method.
#[derive(Debug, Error)]
pub enum PortalError {
    /// `org.freedesktop.portal.GlobalShortcuts` is not available on
    /// the bus (no portal frontend, sandboxed without permission, …).
    #[error("global shortcuts portal not available")]
    Unavailable,
    /// Session disappeared mid-flight — caller should
    /// [`HotkeySession::recreate`].
    #[error("portal session lost (will be recreated)")]
    SessionLost,
    /// User dismissed the bind dialog.
    #[error("bind cancelled by user")]
    BindCancelled,
    /// Bind didn't complete in time. The trait itself does not enforce
    /// a timeout; callers wrap [`PortalAdapter::bind`] in
    /// `tokio::time::timeout` and translate the elapsed error into this
    /// variant.
    #[error("bind timed out after {timeout_secs}s")]
    BindTimeout {
        /// Configured bind timeout, seconds.
        timeout_secs: u64,
    },
    /// Anything else — the embedded message is the source error's
    /// `Display`. Stringifying loses structure but lets us avoid
    /// leaking `ashpd` types into our public API surface.
    #[error("ashpd: {0}")]
    Ashpd(String),
}

/// Opaque session identifier. Real `ashpd` sessions carry borrowed
/// state, so the adapter owns the underlying `Session` and the caller
/// only ever sees this opaque token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionId(String);

impl SessionId {
    /// Construct from an arbitrary string. Used by both real and fake
    /// adapters. The value is opaque and intentionally not parsed.
    #[must_use]
    pub fn new(handle: impl Into<String>) -> Self {
        Self(handle.into())
    }

    /// Borrow the underlying handle string. Useful for log lines.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Pluggable portal interface. The real impl is [`AshpdAdapter`]; tests
/// use [`FakePortal`].
#[async_trait]
pub trait PortalAdapter: Send + Sync {
    /// Open a fresh `GlobalShortcuts` session. `app_id` is reserved for
    /// future flatpak/app-id metadata — currently unused but kept on
    /// the trait so we don't churn callers.
    async fn create_session(&self, app_id: &str) -> Result<SessionId, PortalError>;

    /// List shortcuts currently bound to `sid`.
    async fn list_shortcuts(&self, sid: &SessionId) -> Result<Vec<BoundShortcut>, PortalError>;

    /// Bind (or re-bind) a single shortcut on `sid`. Returns the list
    /// of shortcuts the portal ended up with.
    async fn bind(
        &self,
        sid: &SessionId,
        req: &BindRequest,
    ) -> Result<Vec<BoundShortcut>, PortalError>;

    /// Drop every binding on `sid`. Idempotent — calling twice is OK
    /// (`DoD` #13).
    async fn unbind(&self, sid: &SessionId) -> Result<(), PortalError>;

    /// Subscribe to hotkey events for `sid`. The stream stays alive for
    /// the lifetime of the session; closing the session drops the
    /// stream.
    fn events(&self, sid: &SessionId) -> BoxStream<'static, HotkeyEvent>;

    /// Close the session. Idempotent.
    async fn close(&self, sid: SessionId) -> Result<(), PortalError>;
}

// ---------------------------------------------------------------------
// AshpdAdapter — real backend.
// ---------------------------------------------------------------------

/// Production [`PortalAdapter`] backed by `ashpd 0.13`. Single-session;
/// constructing a second session via the same adapter replaces the
/// previous one. The tray only ever needs one anyway.
///
/// Sender validation is provided by the underlying zbus proxy, which is
/// constructed against the well-known portal bus name. See module-level
/// docs for the contract this enforces and the test that pins it.
#[derive(Debug)]
pub struct AshpdAdapter {
    inner: RwLock<Option<AshpdInner>>,
}

struct AshpdInner {
    proxy: ashpd::desktop::global_shortcuts::GlobalShortcuts,
    session: ashpd::desktop::Session<ashpd::desktop::global_shortcuts::GlobalShortcuts>,
    handle: String,
    /// Serialised D-Bus object path of the live `Session<T>`.
    /// Used by the listener task to filter incoming
    /// `Activated`/`Deactivated`/`ShortcutsChanged` signals so a
    /// concurrent or recreated session cannot bleed events into
    /// our event stream. Pinned to `DoD` #8 / risk G1 — see
    /// `activated_signal_from_unexpected_session_dropped`.
    ///
    /// Stored on the inner so the value survives in the Debug
    /// printout / for future probes; the actual filtering uses
    /// a clone passed into the spawned listener task.
    #[allow(dead_code, reason = "kept on the inner for diagnostics; listener uses a clone")]
    expected_session_handle: Option<String>,
    /// Broadcast channel that the spawned listener task pushes
    /// `HotkeyEvent`s into. `events()` returns a fresh subscriber. We
    /// use broadcast (not mpsc) so multiple subscribers are tolerated;
    /// in practice there's exactly one (the tray listener).
    event_tx: broadcast::Sender<HotkeyEvent>,
    /// Aborts the listener task when the inner is dropped.
    listener: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for AshpdInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AshpdInner")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl Drop for AshpdInner {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

impl Default for AshpdAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AshpdAdapter {
    /// Create a fresh adapter with no live session.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }
}

/// Map an `ashpd::Error` to a [`PortalError`]. Centralised so every
/// adapter method has consistent classification.
#[allow(
    clippy::needless_pass_by_value,
    reason = "ashpd::Error returned by the upstream calls is consumed here at the\
              `?` boundary; taking by value keeps the call sites concise."
)]
fn map_ashpd_err(e: ashpd::Error) -> PortalError {
    use ashpd::desktop::ResponseError;

    match &e {
        // User dismissed the dialog.
        ashpd::Error::Response(ResponseError::Cancelled) => PortalError::BindCancelled,
        // No portal frontend speaks GlobalShortcuts.
        ashpd::Error::PortalNotFound(_) => PortalError::Unavailable,
        // zbus name lookup failures map to Unavailable. The string
        // match keeps us decoupled from internal zbus enum changes.
        ashpd::Error::Zbus(zerr) => {
            let msg = zerr.to_string();
            if msg.contains("ServiceUnknown") || msg.contains("NameHasNoOwner") {
                PortalError::Unavailable
            } else if msg.contains("NoReply") || msg.contains("Disconnected") {
                PortalError::SessionLost
            } else {
                PortalError::Ashpd(format!("zbus: {msg}"))
            }
        }
        _ => PortalError::Ashpd(e.to_string()),
    }
}

#[async_trait]
impl PortalAdapter for AshpdAdapter {
    // Slightly over the pedantic 100-line cap: the function is one
    // linear initialization (proxy -> session -> handle extraction
    // -> three signal subscriptions -> listener spawn) where any
    // sub-extraction would just hide the data flow. The added
    // `expected_session_handle` ashpd-hack comment block is the
    // primary reason we cross the line.
    #[allow(clippy::too_many_lines)]
    async fn create_session(&self, _app_id: &str) -> Result<SessionId, PortalError> {
        let proxy = ashpd::desktop::global_shortcuts::GlobalShortcuts::new()
            .await
            .map_err(map_ashpd_err)?;
        let session = proxy
            .create_session(ashpd::desktop::CreateSessionOptions::default())
            .await
            .map_err(map_ashpd_err)?;
        // ashpd's `Session::path()` is `pub(crate)`, so we cannot read
        // the actual D-Bus object path. Generate a stable token and
        // pin the AshpdAdapter to single-session semantics — the tray
        // never needs more than one. Subsequent adapter calls verify
        // the SessionId matches what's currently stored.
        let handle = format!(
            "ashpd-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );

        // Recover the live session's D-Bus object path so the
        // listener can filter signals by `session_handle()`.
        // Why the hack: ashpd 0.13 keeps `Session::path()` as
        // `pub(crate)`, but its `Serialize` impl emits the
        // underlying `ObjectPath` as a plain string in serde_json.
        // Silent failure (filter degrades to pass-through) would
        // let foreign-session signals leak — the `warn!` below
        // makes the regression impossible to miss in production.
        // TODO: drop once ashpd exposes `Session::path() -> &ObjectPath`
        // upstream (track https://github.com/bilelmoussaoui/ashpd).
        let expected_session_handle = match serde_json::to_value(&session) {
            Ok(serde_json::Value::String(p)) => {
                tracing::debug!(
                    session_handle = %p,
                    "extracted ashpd Session handle; signals will be filtered by session_handle"
                );
                Some(p)
            }
            other => {
                tracing::warn!(
                    serialized = ?other,
                    "could not extract ashpd Session handle path; signals will be UNFILTERED by session_handle (foreign-session signals could leak). \
                     Likely cause: ashpd Session<T> Serialize shape changed upstream — see comment above."
                );
                None
            }
        };

        // Wire up the listener task — selects across the three signal
        // streams and pushes into a broadcast channel.
        let (event_tx, _) = broadcast::channel::<HotkeyEvent>(64);
        let activated = proxy.receive_activated().await.map_err(map_ashpd_err)?;
        let deactivated = proxy.receive_deactivated().await.map_err(map_ashpd_err)?;
        let changed = proxy
            .receive_shortcuts_changed()
            .await
            .map_err(map_ashpd_err)?;

        let tx = event_tx.clone();
        let session_filter = expected_session_handle.clone();
        let listener = tokio::spawn(async move {
            // `Box::pin` is required: ashpd's stream impls are not
            // `Unpin`, but we need to call `select_next_some` etc.
            let mut activated = Box::pin(activated);
            let mut deactivated = Box::pin(deactivated);
            let mut changed = Box::pin(changed);
            loop {
                tokio::select! {
                    Some(ev) = activated.next() => {
                        if let Some(expected) = session_filter.as_deref()
                            && ev.session_handle().as_str() != expected
                        {
                            tracing::debug!(
                                received = %ev.session_handle().as_str(),
                                expected,
                                shortcut_id = ev.shortcut_id(),
                                "dropped Activated from foreign session_handle"
                            );
                            continue;
                        }
                        let ts = u64::try_from(ev.timestamp().as_millis()).ok();
                        let _ = tx.send(HotkeyEvent::Activated {
                            shortcut_id: ev.shortcut_id().to_string(),
                            timestamp: ts,
                        });
                    }
                    Some(ev) = deactivated.next() => {
                        if let Some(expected) = session_filter.as_deref()
                            && ev.session_handle().as_str() != expected
                        {
                            tracing::debug!(
                                received = %ev.session_handle().as_str(),
                                expected,
                                shortcut_id = ev.shortcut_id(),
                                "dropped Deactivated from foreign session_handle"
                            );
                            continue;
                        }
                        let _ = tx.send(HotkeyEvent::Deactivated {
                            shortcut_id: ev.shortcut_id().to_string(),
                        });
                    }
                    Some(ev) = changed.next() => {
                        if let Some(expected) = session_filter.as_deref()
                            && ev.session_handle().as_str() != expected
                        {
                            tracing::debug!(
                                received = %ev.session_handle().as_str(),
                                expected,
                                "dropped ShortcutsChanged from foreign session_handle"
                            );
                            continue;
                        }
                        let _ = tx.send(HotkeyEvent::ShortcutsChanged);
                    }
                    else => break,
                }
            }
            tracing::debug!("ashpd portal listener task exited");
        });

        let inner = AshpdInner {
            proxy,
            session,
            handle: handle.clone(),
            expected_session_handle,
            event_tx,
            listener,
        };
        *self.inner.write().await = Some(inner);
        Ok(SessionId::new(handle))
    }

    async fn list_shortcuts(&self, sid: &SessionId) -> Result<Vec<BoundShortcut>, PortalError> {
        let guard = self.inner.read().await;
        let inner = guard.as_ref().ok_or(PortalError::SessionLost)?;
        if inner.handle != sid.0 {
            return Err(PortalError::SessionLost);
        }
        let req = inner
            .proxy
            .list_shortcuts(
                &inner.session,
                ashpd::desktop::global_shortcuts::ListShortcutsOptions::default(),
            )
            .await
            .map_err(map_ashpd_err)?;
        let resp = req.response().map_err(map_ashpd_err)?;
        Ok(resp.shortcuts().iter().map(map_shortcut).collect())
    }

    async fn bind(
        &self,
        sid: &SessionId,
        req: &BindRequest,
    ) -> Result<Vec<BoundShortcut>, PortalError> {
        let guard = self.inner.read().await;
        let inner = guard.as_ref().ok_or(PortalError::SessionLost)?;
        if inner.handle != sid.0 {
            return Err(PortalError::SessionLost);
        }
        let new = ashpd::desktop::global_shortcuts::NewShortcut::new(&req.id, &req.description)
            .preferred_trigger(req.preferred_trigger.as_deref());
        let bind_req = inner
            .proxy
            .bind_shortcuts(
                &inner.session,
                std::slice::from_ref(&new),
                None,
                ashpd::desktop::global_shortcuts::BindShortcutsOptions::default(),
            )
            .await
            .map_err(map_ashpd_err)?;
        let resp = bind_req.response().map_err(map_ashpd_err)?;
        Ok(resp.shortcuts().iter().map(map_shortcut).collect())
    }

    async fn unbind(&self, _sid: &SessionId) -> Result<(), PortalError> {
        // The XDG GlobalShortcuts portal in 0.13 has no per-shortcut
        // unbind RPC — bindings are erased by closing the session.
        // Idempotent: closing a closed session is a no-op (warn-only).
        let mut guard = self.inner.write().await;
        if let Some(inner) = guard.take() {
            if let Err(e) = inner.session.close().await {
                // Non-fatal: the compositor likely already cleared the
                // session. We log and treat it as success per `DoD` #13.
                tracing::warn!(
                    error = %e,
                    "ignoring portal session close failure during unbind"
                );
            }
        }
        Ok(())
    }

    fn events(&self, _sid: &SessionId) -> BoxStream<'static, HotkeyEvent> {
        // We can't `await` here (sync trait method), so peek the inner
        // synchronously via `try_read`. If the inner isn't ready yet,
        // return an empty stream — this only happens if the caller
        // calls `events()` before `create_session()` succeeds, which
        // [`HotkeySession`] never does.
        match self.inner.try_read() {
            Ok(guard) => match guard.as_ref() {
                Some(inner) => {
                    let rx = inner.event_tx.subscribe();
                    let stream = tokio_stream_from_broadcast(rx);
                    stream.boxed()
                }
                None => stream::empty().boxed(),
            },
            Err(_) => stream::empty().boxed(),
        }
    }

    async fn close(&self, sid: SessionId) -> Result<(), PortalError> {
        let mut guard = self.inner.write().await;
        match guard.as_ref() {
            Some(inner) if inner.handle == sid.0 => {
                if let Some(inner) = guard.take()
                    && let Err(e) = inner.session.close().await
                {
                    tracing::warn!(error = %e, "ignoring portal session close failure");
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Adapt an ashpd `Shortcut` into our [`BoundShortcut`].
fn map_shortcut(s: &ashpd::desktop::global_shortcuts::Shortcut) -> BoundShortcut {
    BoundShortcut {
        id: s.id().to_string(),
        trigger_description: s.trigger_description().to_string(),
        description: s.description().to_string(),
    }
}

/// Build a `Stream` from a `broadcast::Receiver`, filtering out lag
/// errors (which we surface as a warn-log, not an event).
fn tokio_stream_from_broadcast(
    rx: broadcast::Receiver<HotkeyEvent>,
) -> BoxStream<'static, HotkeyEvent> {
    stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => return Some((ev, rx)),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "broadcast lag, dropping events");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .boxed()
}

// ---------------------------------------------------------------------
// FakePortal — exposed publicly behind `cfg(any(test, feature =
// "test-fakes"))` so Group E (tray) can reuse the same fake without
// duplicating code.
// ---------------------------------------------------------------------

#[cfg(any(test, feature = "test-fakes"))]
pub use fake::FakePortal;

#[cfg(any(test, feature = "test-fakes"))]
mod fake {
    use super::{
        BindRequest, BoundShortcut, BoxStream, HotkeyEvent, PortalAdapter, PortalError,
        SessionId, StreamExt, async_trait, broadcast, stream,
    };
    use tokio::sync::Mutex;

    /// In-memory fake. Goals:
    /// * Deterministic — every test owns its own instance.
    /// * Models the production sender-validation contract (G1).
    /// * Lets tests inject a one-shot failure on the next call.
    #[derive(Debug)]
    pub struct FakePortal {
        state: Mutex<FakeState>,
        event_tx: broadcast::Sender<HotkeyEvent>,
    }

    #[derive(Debug)]
    struct FakeState {
        bound: Vec<BoundShortcut>,
        next_call_failure: Option<PortalError>,
        sender_filter_active: bool,
        session_open: bool,
        /// `session_handle()` value the listener pretends to
        /// have opened. Defaults to the same well-known string
        /// every test uses so old tests are unaffected. Tests
        /// for risk G1 cross-session leak override this via
        /// [`super::FakePortal::set_expected_session_handle`].
        expected_session_handle: String,
        /// Mirrors the production filter: drop any event whose
        /// emit-time `session_handle` does not match
        /// `expected_session_handle`. Off by default for
        /// backward-compat with the existing test suite.
        session_filter_active: bool,
    }

    impl Default for FakePortal {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakePortal {
        /// Construct with no bindings, no failures injected, sender
        /// filter disabled (i.e. all senders accepted by default).
        #[must_use]
        pub fn new() -> Self {
            let (event_tx, _) = broadcast::channel(64);
            Self {
                state: Mutex::new(FakeState {
                    bound: Vec::new(),
                    next_call_failure: None,
                    sender_filter_active: false,
                    session_open: false,
                    expected_session_handle: "/org/freedesktop/portal/desktop/session/fake"
                        .to_string(),
                    session_filter_active: false,
                }),
                event_tx,
            }
        }

        /// Override the `session_handle` the fake will accept when
        /// [`Self::enable_session_filter`] is on. Useful for the
        /// G1 cross-session-leak regression test.
        pub async fn set_expected_session_handle(&self, handle: &str) {
            self.state.lock().await.expected_session_handle = handle.to_string();
        }

        /// Turn on production-style `session_handle` filtering.
        /// After this, [`Self::emit_activated_from_session`]
        /// drops events whose `session_handle` does not match.
        pub async fn enable_session_filter(&self) {
            self.state.lock().await.session_filter_active = true;
        }

        /// Push an Activated event tagged with an explicit
        /// `session_handle`. When the session filter is active
        /// and the handle does not match
        /// `expected_session_handle`, the event is dropped —
        /// modelling the production listener's
        /// `ev.session_handle() != expected` check.
        pub async fn emit_activated_from_session(
            &self,
            session_handle: &str,
            shortcut_id: &str,
        ) {
            let state = self.state.lock().await;
            if state.session_filter_active && session_handle != state.expected_session_handle {
                tracing::debug!(
                    received = session_handle,
                    expected = %state.expected_session_handle,
                    shortcut_id,
                    "FakePortal dropped Activated from foreign session_handle"
                );
                return;
            }
            drop(state);
            let _ = self.event_tx.send(HotkeyEvent::Activated {
                shortcut_id: shortcut_id.to_string(),
                timestamp: None,
            });
        }

        /// Inject a single failure that the next adapter call will
        /// return. Cleared after one use.
        pub async fn fail_next_call_with(&self, err: PortalError) {
            self.state.lock().await.next_call_failure = Some(err);
        }

        /// Push a synthetic `Activated` event, simulating the portal
        /// itself sending the signal (always accepted).
        pub fn emit_activated(&self, shortcut_id: &str) {
            let _ = self.event_tx.send(HotkeyEvent::Activated {
                shortcut_id: shortcut_id.to_string(),
                timestamp: None,
            });
        }

        /// Push a synthetic `Deactivated`.
        pub fn emit_deactivated(&self, shortcut_id: &str) {
            let _ = self.event_tx.send(HotkeyEvent::Deactivated {
                shortcut_id: shortcut_id.to_string(),
            });
        }

        /// Push a synthetic `ShortcutsChanged`.
        pub fn emit_shortcuts_changed(&self) {
            let _ = self.event_tx.send(HotkeyEvent::ShortcutsChanged);
        }

        /// Simulate a peer publishing an `Activated` from a non-portal
        /// bus name. When [`Self::enable_sender_filter`] has been
        /// called, the event is dropped — modelling zbus's
        /// match-by-sender filtering on the production proxy. When
        /// the filter is disabled, the event passes through (used to
        /// negative-test the test itself).
        pub async fn emit_activated_from_sender(&self, sender: &str, shortcut_id: &str) {
            let filter = self.state.lock().await.sender_filter_active;
            if filter && sender != "org.freedesktop.portal.Desktop" {
                tracing::debug!(
                    sender,
                    shortcut_id,
                    "FakePortal dropped Activated from unexpected sender"
                );
                return;
            }
            let _ = self.event_tx.send(HotkeyEvent::Activated {
                shortcut_id: shortcut_id.to_string(),
                timestamp: None,
            });
        }

        /// Turn on production-style sender filtering.
        pub async fn enable_sender_filter(&self) {
            self.state.lock().await.sender_filter_active = true;
        }

        /// Snapshot of the currently bound shortcuts (test helper).
        pub async fn bound(&self) -> Vec<BoundShortcut> {
            self.state.lock().await.bound.clone()
        }

        async fn pop_injected_failure(&self) -> Option<PortalError> {
            self.state.lock().await.next_call_failure.take()
        }
    }

    /// Helper: clone a [`PortalError`] losslessly (struct doesn't impl
    /// Clone because of the boxed `Ashpd(String)` that's already
    /// fine, but we don't derive Clone on it to keep the surface
    /// tight).
    fn clone_err(e: &PortalError) -> PortalError {
        match e {
            PortalError::Unavailable => PortalError::Unavailable,
            PortalError::SessionLost => PortalError::SessionLost,
            PortalError::BindCancelled => PortalError::BindCancelled,
            PortalError::BindTimeout { timeout_secs } => PortalError::BindTimeout {
                timeout_secs: *timeout_secs,
            },
            PortalError::Ashpd(m) => PortalError::Ashpd(m.clone()),
        }
    }

    #[async_trait]
    impl PortalAdapter for FakePortal {
        async fn create_session(&self, _app_id: &str) -> Result<SessionId, PortalError> {
            if let Some(err) = self.pop_injected_failure().await {
                return Err(err);
            }
            let mut state = self.state.lock().await;
            state.session_open = true;
            Ok(SessionId::new("fake-session"))
        }

        async fn list_shortcuts(
            &self,
            _sid: &SessionId,
        ) -> Result<Vec<BoundShortcut>, PortalError> {
            if let Some(err) = self.pop_injected_failure().await {
                return Err(err);
            }
            let state = self.state.lock().await;
            if !state.session_open {
                return Err(PortalError::SessionLost);
            }
            Ok(state.bound.clone())
        }

        async fn bind(
            &self,
            _sid: &SessionId,
            req: &BindRequest,
        ) -> Result<Vec<BoundShortcut>, PortalError> {
            if let Some(err) = self.pop_injected_failure().await {
                return Err(err);
            }
            let mut state = self.state.lock().await;
            if !state.session_open {
                return Err(PortalError::SessionLost);
            }
            // Replace any existing entry with the same id (matches the
            // real portal: rebinding the same id is allowed).
            state.bound.retain(|s| s.id != req.id);
            let new = BoundShortcut {
                id: req.id.clone(),
                trigger_description: req
                    .preferred_trigger
                    .clone()
                    .unwrap_or_else(|| "Ctrl+Alt+R".to_string()),
                description: req.description.clone(),
            };
            state.bound.push(new);
            Ok(state.bound.clone())
        }

        async fn unbind(&self, _sid: &SessionId) -> Result<(), PortalError> {
            // `DoD` #13: idempotent — never errors, never returns the
            // injected failure (unbind on shutdown must always succeed
            // from the caller's POV; warn-only on failure).
            let mut state = self.state.lock().await;
            state.bound.clear();
            Ok(())
        }

        fn events(&self, _sid: &SessionId) -> BoxStream<'static, HotkeyEvent> {
            let rx = self.event_tx.subscribe();
            stream::unfold(rx, |mut rx| async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => return Some((ev, rx)),
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "fake broadcast lag");
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                }
            })
            .boxed()
        }

        async fn close(&self, _sid: SessionId) -> Result<(), PortalError> {
            let mut state = self.state.lock().await;
            state.bound.clear();
            state.session_open = false;
            Ok(())
        }
    }

    impl Clone for super::PortalError {
        fn clone(&self) -> Self {
            clone_err(self)
        }
    }
}

// ---------------------------------------------------------------------
// HotkeySession — high-level wrapper.
// ---------------------------------------------------------------------

/// Owns a [`SessionId`] + the event stream returned by the adapter.
/// Group E (tray listener) and Group F (CLI `hotkey bind`) both consume
/// this. Generic over the adapter so tests can swap in [`FakePortal`].
pub struct HotkeySession<A: PortalAdapter + ?Sized> {
    adapter: Arc<A>,
    sid: SessionId,
    /// Wrapped in a `Mutex` so `next_event(&mut self)` doesn't move
    /// the stream out of `self`.
    event_stream: Mutex<BoxStream<'static, HotkeyEvent>>,
    alive: bool,
}

impl<A: PortalAdapter + ?Sized> std::fmt::Debug for HotkeySession<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotkeySession")
            .field("sid", &self.sid)
            .field("alive", &self.alive)
            .finish_non_exhaustive()
    }
}

impl<A: PortalAdapter + ?Sized + 'static> HotkeySession<A> {
    /// Open a new session via the adapter and wire up the event
    /// stream.
    pub async fn create(adapter: Arc<A>, app_id: &str) -> Result<Self, PortalError> {
        let sid = adapter.create_session(app_id).await?;
        let stream = adapter.events(&sid);
        Ok(Self {
            adapter,
            sid,
            event_stream: Mutex::new(stream),
            alive: true,
        })
    }

    /// Bind the requested shortcut.
    pub async fn bind(&self, req: &BindRequest) -> Result<Vec<BoundShortcut>, PortalError> {
        self.adapter.bind(&self.sid, req).await
    }

    /// List currently bound shortcuts.
    pub async fn list_shortcuts(&self) -> Result<Vec<BoundShortcut>, PortalError> {
        self.adapter.list_shortcuts(&self.sid).await
    }

    /// Drop every binding. Always returns `Ok` per `DoD` #13 — the
    /// underlying adapter logs (warn-only) and we propagate success.
    pub async fn unbind(&self) -> Result<(), PortalError> {
        match self.adapter.unbind(&self.sid).await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "ignoring unbind error (idempotent)");
                Ok(())
            }
        }
    }

    /// Pull the next event from the stream. Returns `None` when the
    /// stream has been closed (session dropped).
    pub async fn next_event(&mut self) -> Option<HotkeyEvent> {
        let mut stream = self.event_stream.lock().await;
        stream.next().await
    }

    /// True until [`Self::close`] has consumed `self` or [`Self::recreate`]
    /// has hit a hard failure. Set to false by `recreate` only when it
    /// fails — a successful `recreate` flips it back to true.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive
    }

    /// Tear down the existing session and open a fresh one. Used by
    /// the tray listener (Group E) when a previous adapter call
    /// returned [`PortalError::SessionLost`] or `xdg-desktop-portal`
    /// restarted.
    pub async fn recreate(&mut self, app_id: &str) -> Result<(), PortalError> {
        // Best-effort close of the old session — ignore failures.
        if let Err(e) = self.adapter.close(self.sid.clone()).await {
            tracing::warn!(error = %e, "old session close failed during recreate");
        }
        match self.adapter.create_session(app_id).await {
            Ok(new_sid) => {
                let stream = self.adapter.events(&new_sid);
                self.sid = new_sid;
                *self.event_stream.lock().await = stream;
                self.alive = true;
                Ok(())
            }
            Err(e) => {
                self.alive = false;
                Err(e)
            }
        }
    }

    /// Close the session and release the adapter handle.
    pub async fn close(self) -> Result<(), PortalError> {
        self.adapter.close(self.sid).await
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::time::Duration;

    const APP_ID: &str = "zwhisper-test";

    fn req() -> BindRequest {
        BindRequest {
            id: SHORTCUT_ID.to_string(),
            description: SHORTCUT_DESCRIPTION.to_string(),
            preferred_trigger: Some("CTRL+ALT+r".to_string()),
        }
    }

    #[tokio::test]
    async fn bind_records_shortcut() {
        let portal = Arc::new(FakePortal::new());
        let session = HotkeySession::create(portal.clone(), APP_ID).await.unwrap();
        let bound = session.bind(&req()).await.unwrap();
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].id, SHORTCUT_ID);
        assert_eq!(portal.bound().await, bound);
    }

    #[tokio::test]
    async fn unbind_clears_shortcuts_and_is_idempotent() {
        let portal = Arc::new(FakePortal::new());
        let session = HotkeySession::create(portal.clone(), APP_ID).await.unwrap();
        session.bind(&req()).await.unwrap();
        assert_eq!(portal.bound().await.len(), 1);

        session.unbind().await.unwrap();
        assert!(portal.bound().await.is_empty());
        // Idempotent — a second call still succeeds.
        session.unbind().await.unwrap();
        assert!(portal.bound().await.is_empty());
    }

    #[tokio::test]
    async fn list_shortcuts_returns_bound() {
        let portal = Arc::new(FakePortal::new());
        let session = HotkeySession::create(portal.clone(), APP_ID).await.unwrap();
        session.bind(&req()).await.unwrap();
        let listed = session.list_shortcuts().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, SHORTCUT_ID);
    }

    #[tokio::test]
    async fn events_stream_delivers_activated_in_order() {
        let portal = Arc::new(FakePortal::new());
        let mut session = HotkeySession::create(portal.clone(), APP_ID).await.unwrap();
        session.bind(&req()).await.unwrap();

        portal.emit_activated(SHORTCUT_ID);
        portal.emit_deactivated(SHORTCUT_ID);
        portal.emit_shortcuts_changed();

        let e1 = tokio::time::timeout(Duration::from_millis(200), session.next_event())
            .await
            .expect("first event timed out")
            .expect("stream closed");
        let e2 = tokio::time::timeout(Duration::from_millis(200), session.next_event())
            .await
            .expect("second event timed out")
            .expect("stream closed");
        let e3 = tokio::time::timeout(Duration::from_millis(200), session.next_event())
            .await
            .expect("third event timed out")
            .expect("stream closed");

        assert!(matches!(
            e1,
            HotkeyEvent::Activated { ref shortcut_id, .. } if shortcut_id == SHORTCUT_ID
        ));
        assert!(matches!(
            e2,
            HotkeyEvent::Deactivated { ref shortcut_id } if shortcut_id == SHORTCUT_ID
        ));
        assert!(matches!(e3, HotkeyEvent::ShortcutsChanged));
    }

    #[tokio::test]
    async fn service_unknown_classified_as_unavailable() {
        // We construct an `ashpd::Error::Zbus(...)` directly via the
        // public `From<zbus::Error>` impl — `ServiceUnknown` is the
        // canonical "no portal frontend" error and must surface as
        // `Unavailable` (DoD #5: friendly diagnostic, not a noisy
        // backtrace).
        let zerr = zbus::Error::Failure("ServiceUnknown: no such name".to_string());
        let mapped = map_ashpd_err(ashpd::Error::Zbus(zerr));
        assert!(matches!(mapped, PortalError::Unavailable));
    }

    #[tokio::test]
    async fn recreate_after_session_lost() {
        let portal = Arc::new(FakePortal::new());
        let mut session = HotkeySession::create(portal.clone(), APP_ID).await.unwrap();

        // Inject a session-lost on the next `bind` so we can confirm
        // recreate restores a working session afterwards.
        portal
            .fail_next_call_with(PortalError::SessionLost)
            .await;
        let err = session.bind(&req()).await.unwrap_err();
        assert!(matches!(err, PortalError::SessionLost));

        session.recreate(APP_ID).await.unwrap();
        assert!(session.is_alive());
        // After recreate, bind succeeds again.
        let bound = session.bind(&req()).await.unwrap();
        assert_eq!(bound.len(), 1);
    }

    #[tokio::test]
    async fn activated_signal_from_unexpected_sender_dropped() {
        // `DoD` #8 / risk G1 — ensure the FakePortal models the
        // production zbus sender filter. Production filtering is
        // enforced by the proxy bus-name match (see module docs).
        let portal = Arc::new(FakePortal::new());
        portal.enable_sender_filter().await;
        let mut session = HotkeySession::create(portal.clone(), APP_ID).await.unwrap();
        session.bind(&req()).await.unwrap();

        portal
            .emit_activated_from_sender("evil.process", SHORTCUT_ID)
            .await;

        // No event should be delivered.
        let result =
            tokio::time::timeout(Duration::from_millis(50), session.next_event()).await;
        assert!(
            result.is_err(),
            "expected timeout (no event delivered) but got {result:?}"
        );

        // Sanity: legitimate portal sender DOES pass through.
        portal
            .emit_activated_from_sender("org.freedesktop.portal.Desktop", SHORTCUT_ID)
            .await;
        let ev = tokio::time::timeout(Duration::from_millis(200), session.next_event())
            .await
            .expect("legit event timed out")
            .expect("stream closed");
        assert!(matches!(
            ev,
            HotkeyEvent::Activated { ref shortcut_id, .. } if shortcut_id == SHORTCUT_ID
        ));
    }

    #[tokio::test]
    async fn activated_signal_from_unexpected_session_dropped() {
        // Risk G1 / `DoD` #8 — pinned via the listener's
        // `ev.session_handle() != expected_session_handle` guard
        // in `AshpdInner::listener`. The FakePortal mirrors that
        // contract: with the session filter on, an Activated
        // tagged with a foreign session_handle is dropped before
        // it reaches the broadcast channel.
        let portal = Arc::new(FakePortal::new());
        portal
            .set_expected_session_handle("/org/freedesktop/portal/desktop/session/expected")
            .await;
        portal.enable_session_filter().await;
        let mut session = HotkeySession::create(portal.clone(), APP_ID).await.unwrap();
        session.bind(&req()).await.unwrap();

        // Foreign session_handle — must be dropped.
        portal
            .emit_activated_from_session(
                "/org/freedesktop/portal/desktop/session/foreign",
                SHORTCUT_ID,
            )
            .await;
        let result =
            tokio::time::timeout(Duration::from_millis(50), session.next_event()).await;
        assert!(
            result.is_err(),
            "expected timeout (foreign session event dropped) but got {result:?}"
        );

        // Sanity: matching session_handle DOES pass through.
        portal
            .emit_activated_from_session(
                "/org/freedesktop/portal/desktop/session/expected",
                SHORTCUT_ID,
            )
            .await;
        let ev = tokio::time::timeout(Duration::from_millis(200), session.next_event())
            .await
            .expect("legit event timed out")
            .expect("stream closed");
        assert!(matches!(
            ev,
            HotkeyEvent::Activated { ref shortcut_id, .. } if shortcut_id == SHORTCUT_ID
        ));
    }

    #[tokio::test]
    async fn bind_cancelled_returns_bind_cancelled_variant() {
        let portal = Arc::new(FakePortal::new());
        let session = HotkeySession::create(portal.clone(), APP_ID).await.unwrap();
        portal
            .fail_next_call_with(PortalError::BindCancelled)
            .await;
        let err = session.bind(&req()).await.unwrap_err();
        assert!(matches!(err, PortalError::BindCancelled));
    }

    /// Real-portal smoke test. Marked `#[ignore]` — only runs under a
    /// session bus with `xdg-desktop-portal` active.
    #[tokio::test]
    #[ignore = "requires xdg-desktop-portal"]
    async fn ashpd_adapter_create_session_smoke() {
        let adapter = Arc::new(AshpdAdapter::new());
        let session = HotkeySession::create(adapter, APP_ID).await.unwrap();
        session.close().await.unwrap();
    }
}
