//! M7 — Hotkey rebind tab (`DoD` #15 + #16).
//!
//! Owns four FLTK widgets (current-binding label, rebind button,
//! status label, plus the parent group), the cross-thread mpsc
//! into the dispatcher, and a long-lived `Arc<AshpdAdapter>` shared
//! between the build-time `list_shortcuts` probe and every Rebind
//! click. The adapter holds a single `Session` per the M6 contract
//! — recreating it on the second click is intentional, the FLTK
//! main thread should never race the same session twice.
//!
//! ### App-id
//!
//! Sessions are created with `cz.zajca.Zwhisper1.Tray` to match the
//! M6 architectural decision (D4) and the M7 plan (D7). Using a
//! distinct app-id from settings would make the portal treat the
//! settings rebind as a *new* application's binding, leaving the
//! tray's chord stale until the user rebinds again from the tray
//! menu. Sharing the app-id is the wire-level fix.
//!
//! ### Notification path
//!
//! On a successful bind, the tab emits the
//! `cz.zajca.Zwhisper1.Settings.HotkeyRebound` signal via
//! [`crate::hotkey_signal::HotkeyReboundEmitter`]. The tray
//! subscribes to that signal and recreates its own
//! `HotkeySession` on receipt — see
//! `crates/zwhisper-tray/src/hotkey.rs`.

use std::sync::Arc;
use std::time::Duration;

use fltk::{
    button::Button,
    enums::{Color, Font, FrameType},
    frame::Frame,
    group::{Group, Tabs},
    prelude::*,
};
use tokio::time::timeout;
use tracing::{debug, info, warn};
use zwhisper_hotkey::config::{DEFAULT_BIND_TIMEOUT_SECS, HotkeyConfig};
use zwhisper_hotkey::portal::{
    AshpdAdapter, BindRequest, HotkeySession, PortalAdapter, PortalError,
    SHORTCUT_DESCRIPTION, SHORTCUT_ID,
};

use crate::app::UiMessage;
use crate::hotkey_signal::HotkeyReboundEmitter;
use crate::runtime::UiBridge;

/// App-id passed to the portal. Mirrors the tray's well-known
/// D-Bus name so the portal scopes settings' rebind to the same
/// "application" the tray listens on. Hard-coded by intent —
/// tests pin the value via `app_id_matches_tray`.
pub(crate) const PORTAL_APP_ID: &str = "cz.zajca.Zwhisper1.Tray";

/// Fixed row height for every control inside the tab.
const ROW_HEIGHT: i32 = 28;

/// Vertical gap between rows.
const ROW_GAP: i32 = 8;

/// Padding from the parent group's edges.
const PADDING: i32 = 12;

/// Rebind button width.
const BUTTON_WIDTH: i32 = 140;

/// Cross-thread messages produced by the hotkey rebind tab.
///
/// Variants line up with the rebind state machine described in
/// M7-plan § "Hotkey rebind tab" (`DoD` #15). The dispatcher in
/// `app.rs` calls [`apply_msg`] which paints the matching widget
/// state.
#[derive(Debug, Clone)]
pub(crate) enum HotkeyMsg {
    /// First read of the current binding succeeded — render the
    /// current trigger description.
    InitialBindLoaded { description: String },
    /// First read of the current binding failed — render a warning
    /// in the status row but keep the Rebind button enabled (the
    /// user can still try).
    InitialBindFailed { error: String },
    /// User clicked Rebind. The portal dialog is open.
    RebindStarted,
    /// Rebind succeeded — render the new trigger description.
    RebindCompleted { description: String },
    /// User dismissed the portal dialog.
    RebindCancelled,
    /// `tokio::time::timeout` fired before the portal returned.
    RebindTimedOut,
    /// Portal frontend not available (no `GlobalShortcuts` impl on
    /// the bus, sandbox restriction, …).
    PortalUnavailable,
    /// Rebind attempt failed mid-operation. Distinct from
    /// `InitialBindFailed`: the prior chord typically remains
    /// valid, so the binding label stays untouched and only the
    /// status line is updated.
    RebindFailed { error: String },
}

/// Holds the FLTK widgets belonging to the hotkey tab.
#[derive(Clone, Debug)]
pub(crate) struct HotkeyTab {
    #[allow(dead_code, reason = "kept alive for the FLTK widget tree")]
    group: Group,
    #[allow(dead_code, reason = "updated via UiMessage::Hotkey dispatcher")]
    binding_label: Frame,
    #[allow(dead_code, reason = "updated via UiMessage::Hotkey dispatcher")]
    status_label: Frame,
    #[allow(dead_code, reason = "wired via callback closure")]
    rebind_button: Button,
}

/// Outcome of one rebind attempt — the unit a [`run_rebind`] call
/// produces. Tests pattern-match on this enum without going through
/// the FLTK widget surface; production code wraps it in
/// [`HotkeyMsg`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RebindOutcome {
    Completed { description: String },
    Cancelled,
    TimedOut,
    Unavailable,
    /// Catch-all: any `PortalError` other than `BindCancelled` /
    /// `Unavailable` plus session-create failures. The string
    /// payload is the original error rendered through `Display`.
    Failed(String),
}

impl From<RebindOutcome> for HotkeyMsg {
    fn from(value: RebindOutcome) -> Self {
        match value {
            RebindOutcome::Completed { description } => Self::RebindCompleted { description },
            RebindOutcome::Cancelled => Self::RebindCancelled,
            RebindOutcome::TimedOut => Self::RebindTimedOut,
            RebindOutcome::Unavailable => Self::PortalUnavailable,
            // `Failed` keeps the prior binding label intact
            // (post-review fix): the previous chord typically
            // remains valid, so showing "Current binding: unknown"
            // would mislead the user into thinking they lost it.
            RebindOutcome::Failed(error) => Self::RebindFailed { error },
        }
    }
}

/// Construct the hotkey rebind tab. Spawns one async task at
/// build time to read the current binding via `list_shortcuts`;
/// the Rebind button schedules a fresh task per click.
#[allow(
    clippy::needless_pass_by_value,
    reason = "build() takes UiBridge by value to match the sibling tab signatures (profile.rs, models.rs, whisper_cli.rs)"
)]
pub(crate) fn build(parent: &mut Tabs, bridge: UiBridge) -> HotkeyTab {
    let (gx, gy, gw, gh) = parent.client_area();
    let group = Group::new(gx, gy, gw, gh, "Hotkey");

    let inner_w = gw - PADDING * 2;
    let mut y = gy + PADDING;

    let mut binding_label = Frame::new(
        gx + PADDING,
        y,
        inner_w,
        ROW_HEIGHT,
        "Current binding: loading…",
    );
    binding_label.set_label_font(Font::Helvetica);
    binding_label.set_frame(FrameType::FlatBox);
    binding_label.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);
    y += ROW_HEIGHT + ROW_GAP;

    let mut rebind_button = Button::new(gx + PADDING, y, BUTTON_WIDTH, ROW_HEIGHT, "Rebind…");
    let cb_bridge = bridge.clone();
    rebind_button.set_callback(move |_btn| {
        spawn_rebind_task(&cb_bridge, Arc::new(AshpdAdapter::new()));
    });
    y += ROW_HEIGHT + ROW_GAP;

    let mut status_label = Frame::new(gx + PADDING, y, inner_w, ROW_HEIGHT, "");
    status_label.set_label_font(Font::Helvetica);
    status_label.set_frame(FrameType::FlatBox);
    status_label.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);

    group.end();
    parent.add(&group);

    // Initial binding probe — same code path as Rebind but without
    // the bind step. Done in a separate task so the FLTK build
    // returns immediately.
    spawn_initial_load(&bridge, Arc::new(AshpdAdapter::new()));

    HotkeyTab {
        group,
        binding_label,
        status_label,
        rebind_button,
    }
}

/// Spawn the at-build initial-load task: open a session, list the
/// current shortcuts, send `InitialBindLoaded` or
/// `InitialBindFailed` to the dispatcher.
pub(crate) fn spawn_initial_load<A>(bridge: &UiBridge, adapter: Arc<A>)
where
    A: PortalAdapter + ?Sized + 'static,
{
    let tx = bridge.tx.clone();
    let _join = bridge.rt_handle.spawn(async move {
        let msg = match load_current_binding(adapter).await {
            Ok(description) => HotkeyMsg::InitialBindLoaded { description },
            Err(error) => HotkeyMsg::InitialBindFailed { error },
        };
        if let Err(e) = tx.send(UiMessage::Hotkey(msg)) {
            debug!(error = %e, "hotkey tab: receiver gone, dropping initial load");
        }
        fltk::app::awake();
    });
}

/// Open a session, fetch the current shortcuts, return a
/// human-readable trigger description for `SHORTCUT_ID` (or a
/// "(not bound)" placeholder if the portal returned nothing).
pub(crate) async fn load_current_binding<A>(adapter: Arc<A>) -> Result<String, String>
where
    A: PortalAdapter + ?Sized + 'static,
{
    let session = HotkeySession::create(adapter, PORTAL_APP_ID)
        .await
        .map_err(|e| format!("portal session: {e}"))?;
    let shortcuts = session
        .list_shortcuts()
        .await
        .map_err(|e| format!("list_shortcuts: {e}"))?;
    let description = shortcuts
        .iter()
        .find(|s| s.id == SHORTCUT_ID)
        .map_or_else(|| "(not bound)".to_owned(), |s| s.trigger_description.clone());
    // Best-effort close — if it fails the OS will clean up on
    // process exit. Logged at warn so a regression is observable.
    if let Err(e) = session.close().await {
        warn!(error = %e, "hotkey tab: initial-load session close failed");
    }
    Ok(description)
}

/// Spawn one rebind task on the runtime. Reads `bind_timeout_secs`
/// from `~/.config/zwhisper/hotkey.toml`, opens a fresh session,
/// runs `bind` under a timeout, emits the
/// `cz.zajca.Zwhisper1.Settings.HotkeyRebound` signal on success,
/// and pushes the outcome onto the `UiMessage` channel.
pub(crate) fn spawn_rebind_task<A>(bridge: &UiBridge, adapter: Arc<A>)
where
    A: PortalAdapter + ?Sized + 'static,
{
    let tx = bridge.tx.clone();
    let _join = bridge.rt_handle.spawn(async move {
        // Push `RebindStarted` immediately so the UI can disable
        // the button / show "Press your shortcut…" hint.
        if let Err(e) = tx.send(UiMessage::Hotkey(HotkeyMsg::RebindStarted)) {
            debug!(error = %e, "hotkey tab: receiver gone before rebind start");
            return;
        }
        fltk::app::awake();

        let bind_timeout = resolve_bind_timeout();
        let outcome = run_rebind(adapter, bind_timeout, PORTAL_APP_ID).await;

        // On a successful bind, fire-and-forget the D-Bus signal so
        // the tray refreshes its session. A failure to emit must
        // NOT mask the bind success; we still report `Completed`.
        if let RebindOutcome::Completed { description } = &outcome {
            match HotkeyReboundEmitter::new().await {
                Ok(emitter) => {
                    if let Err(e) = emitter.emit(description).await {
                        warn!(error = %e, "hotkey tab: rebound signal emit failed");
                    }
                }
                Err(e) => warn!(error = %e, "hotkey tab: rebound emitter connect failed"),
            }
        }

        let msg: HotkeyMsg = outcome.into();
        if let Err(e) = tx.send(UiMessage::Hotkey(msg)) {
            debug!(error = %e, "hotkey tab: receiver gone, dropping rebind result");
        }
        fltk::app::awake();
    });
}

/// Pure async function that drives one rebind attempt. Generic
/// over the adapter so unit tests can swap in a fake without
/// standing up xdg-desktop-portal.
pub(crate) async fn run_rebind<A>(
    adapter: Arc<A>,
    bind_timeout: Duration,
    app_id: &str,
) -> RebindOutcome
where
    A: PortalAdapter + ?Sized + 'static,
{
    let session = match HotkeySession::create(adapter, app_id).await {
        Ok(s) => s,
        Err(PortalError::Unavailable) => return RebindOutcome::Unavailable,
        Err(e) => return RebindOutcome::Failed(format!("session create: {e}")),
    };

    let req = BindRequest {
        id: SHORTCUT_ID.to_owned(),
        description: SHORTCUT_DESCRIPTION.to_owned(),
        preferred_trigger: None,
    };

    let outcome = match timeout(bind_timeout, session.bind(&req)).await {
        Ok(Ok(shortcuts)) => {
            let description = shortcuts
                .iter()
                .find(|s| s.id == SHORTCUT_ID)
                .map_or_else(
                    || "(unknown trigger)".to_owned(),
                    |s| s.trigger_description.clone(),
                );
            info!(description, "hotkey tab: rebind succeeded");
            RebindOutcome::Completed { description }
        }
        Ok(Err(PortalError::BindCancelled)) => RebindOutcome::Cancelled,
        Ok(Err(PortalError::Unavailable)) => RebindOutcome::Unavailable,
        Ok(Err(e)) => {
            warn!(error = %e, "hotkey tab: bind RPC failed");
            RebindOutcome::Failed(format!("bind: {e}"))
        }
        Err(_elapsed) => {
            warn!(
                timeout_secs = bind_timeout.as_secs(),
                "hotkey tab: bind timed out"
            );
            RebindOutcome::TimedOut
        }
    };

    // Best-effort close so the next rebind sees a clean adapter.
    if let Err(e) = session.close().await {
        warn!(error = %e, "hotkey tab: rebind session close failed");
    }
    outcome
}

/// Resolve `bind_timeout_secs` from `~/.config/zwhisper/hotkey.toml`.
/// Falls back to the documented default when the config dir is
/// unresolvable (mirrors the tray + CLI lookup).
fn resolve_bind_timeout() -> Duration {
    let cfg = if let Some(dir) = dirs::config_dir() {
        HotkeyConfig::from_path(&dir.join("zwhisper").join("hotkey.toml"))
    } else {
        debug!(
            "hotkey tab: no config_dir resolved; using default bind timeout {DEFAULT_BIND_TIMEOUT_SECS}s"
        );
        HotkeyConfig::default()
    };
    Duration::from_secs(cfg.bind_timeout_secs)
}

/// Render a [`HotkeyMsg`] into the tab's widgets. The dispatcher
/// in `app.rs` calls this once a `UiMessage::Hotkey` envelope
/// arrives. Pure-ish: only the FLTK widget state mutates.
#[allow(dead_code, reason = "wired by app.rs dispatch in a follow-up task")]
pub(crate) fn apply_msg(tab: &mut HotkeyTab, msg: &HotkeyMsg) {
    match msg {
        HotkeyMsg::InitialBindLoaded { description } => {
            tab.binding_label
                .set_label(&format!("Current binding: {description}"));
            tab.binding_label.set_label_color(Color::Foreground);
            tab.status_label.set_label("");
        }
        HotkeyMsg::InitialBindFailed { error } => {
            tab.binding_label.set_label("Current binding: unknown");
            tab.binding_label.set_label_color(Color::DarkYellow);
            tab.status_label.set_label(&format!("Probe failed: {error}"));
            tab.status_label.set_label_color(Color::DarkRed);
        }
        HotkeyMsg::RebindStarted => {
            tab.status_label
                .set_label("Press your new shortcut in the system dialog…");
            tab.status_label.set_label_color(Color::DarkBlue);
        }
        HotkeyMsg::RebindCompleted { description } => {
            tab.binding_label
                .set_label(&format!("Current binding: {description}"));
            tab.binding_label.set_label_color(Color::DarkGreen);
            tab.status_label.set_label("Rebind successful.");
            tab.status_label.set_label_color(Color::DarkGreen);
        }
        HotkeyMsg::RebindCancelled => {
            tab.status_label
                .set_label("Rebind cancelled — current binding kept.");
            tab.status_label.set_label_color(Color::DarkYellow);
        }
        HotkeyMsg::RebindTimedOut => {
            tab.status_label.set_label(&format!(
                "Rebind timed out after {}s. Try again.",
                resolve_bind_timeout().as_secs()
            ));
            tab.status_label.set_label_color(Color::DarkYellow);
        }
        HotkeyMsg::PortalUnavailable => {
            tab.status_label
                .set_label("Portal unavailable on this session — rebind not supported.");
            tab.status_label.set_label_color(Color::DarkRed);
        }
        HotkeyMsg::RebindFailed { error } => {
            // Keep the prior binding label intact — the previous
            // chord is typically still valid; only the status line
            // is updated.
            tab.status_label.set_label(&format!("Rebind failed: {error}"));
            tab.status_label.set_label_color(Color::DarkRed);
        }
    }
    if let Some(mut parent) = tab.binding_label.parent() {
        parent.redraw();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use futures_util::StreamExt;
    use futures_util::stream::{self, BoxStream};
    use tokio::sync::Mutex;
    use zwhisper_hotkey::portal::{
        BindRequest, BoundShortcut, HotkeyEvent, PortalAdapter, PortalError, SHORTCUT_DESCRIPTION,
        SHORTCUT_ID, SessionId,
    };

    use super::*;

    /// Local fake [`PortalAdapter`] — a stripped-down sibling of the
    /// `zwhisper-hotkey::portal::FakePortal` (which is not enabled in
    /// our dev-deps to avoid a new feature flag). Each call returns a
    /// configurable `Result`; the outcome of a rebind is fully
    /// determined by the values we plug in. Tests construct a fresh
    /// fake per scenario, so there is no shared state to manage.
    struct ScenarioPortal {
        /// Outcome the next `create_session` call returns.
        create_session_outcome: Mutex<Option<Result<SessionId, PortalError>>>,
        /// Outcome the next `bind` call returns. Wrapped in an
        /// `Option` because `Bind` is not necessarily called (e.g.
        /// when `create_session` fails first).
        bind_outcome: Mutex<Option<BindFakeOutcome>>,
    }

    /// What the fake's `bind` should do — return immediately with a
    /// canned outcome, or stall longer than the rebind timeout to
    /// trigger `RebindTimedOut`.
    enum BindFakeOutcome {
        Immediate(Result<Vec<BoundShortcut>, PortalError>),
        StallForever,
    }

    impl ScenarioPortal {
        fn new(
            create: Result<SessionId, PortalError>,
            bind: Option<BindFakeOutcome>,
        ) -> Arc<Self> {
            Arc::new(Self {
                create_session_outcome: Mutex::new(Some(create)),
                bind_outcome: Mutex::new(bind),
            })
        }
    }

    #[async_trait]
    impl PortalAdapter for ScenarioPortal {
        async fn create_session(&self, _app_id: &str) -> Result<SessionId, PortalError> {
            self.create_session_outcome
                .lock()
                .await
                .take()
                .unwrap_or_else(|| Ok(SessionId::new("scenario")))
        }
        async fn list_shortcuts(
            &self,
            _sid: &SessionId,
        ) -> Result<Vec<BoundShortcut>, PortalError> {
            Ok(Vec::new())
        }
        async fn bind(
            &self,
            _sid: &SessionId,
            req: &BindRequest,
        ) -> Result<Vec<BoundShortcut>, PortalError> {
            let outcome = self.bind_outcome.lock().await.take();
            match outcome {
                Some(BindFakeOutcome::Immediate(result)) => result.map(|mut v| {
                    // If a test-supplied vec was empty, stamp in a
                    // matching id so the description-extract path
                    // is exercised.
                    if v.is_empty() {
                        v.push(BoundShortcut {
                            id: req.id.clone(),
                            trigger_description: "Ctrl+Alt+R".to_owned(),
                            description: req.description.clone(),
                        });
                    }
                    v
                }),
                Some(BindFakeOutcome::StallForever) => {
                    // Sleep well beyond every test's timeout window.
                    tokio::time::sleep(Duration::from_secs(60 * 60)).await;
                    Err(PortalError::SessionLost)
                }
                None => Ok(Vec::new()),
            }
        }
        async fn unbind(&self, _sid: &SessionId) -> Result<(), PortalError> {
            Ok(())
        }
        fn events(&self, _sid: &SessionId) -> BoxStream<'static, HotkeyEvent> {
            stream::empty().boxed()
        }
        async fn close(&self, _sid: SessionId) -> Result<(), PortalError> {
            Ok(())
        }
    }

    fn one_second() -> Duration {
        Duration::from_secs(1)
    }

    fn five_millis() -> Duration {
        Duration::from_millis(5)
    }

    #[test]
    fn app_id_matches_tray() {
        // M7 plan D7: settings + tray must agree on the app-id so
        // the portal scopes the binding to the same "application".
        // The string is duplicated by intent in `single_instance.rs`
        // (TRAY_BUS_NAME). Keep this assertion in sync — the tray
        // owns its constant, settings owns ours; this test pins the
        // value on our side.
        assert_eq!(PORTAL_APP_ID, "cz.zajca.Zwhisper1.Tray");
    }

    #[test]
    fn rebind_outcome_to_msg_completed() {
        let msg: HotkeyMsg = RebindOutcome::Completed {
            description: "Ctrl+Alt+R".to_owned(),
        }
        .into();
        match msg {
            HotkeyMsg::RebindCompleted { description } => {
                assert_eq!(description, "Ctrl+Alt+R");
            }
            other => panic!("expected RebindCompleted, got {other:?}"),
        }
    }

    #[test]
    fn rebind_outcome_to_msg_terminal_states() {
        assert!(matches!(
            HotkeyMsg::from(RebindOutcome::Cancelled),
            HotkeyMsg::RebindCancelled
        ));
        assert!(matches!(
            HotkeyMsg::from(RebindOutcome::TimedOut),
            HotkeyMsg::RebindTimedOut
        ));
        assert!(matches!(
            HotkeyMsg::from(RebindOutcome::Unavailable),
            HotkeyMsg::PortalUnavailable
        ));
        let msg: HotkeyMsg =
            RebindOutcome::Failed("session create: unavailable".to_owned()).into();
        match msg {
            HotkeyMsg::RebindFailed { error } => {
                assert!(error.contains("session create"), "{error}");
            }
            other => panic!("expected RebindFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rebind_outcomes_truth_table() {
        // `DoD` #15 — feed each of the documented portal outcomes
        // through `run_rebind` and assert the resulting
        // `RebindOutcome`. Five rows: completed, cancelled, timed
        // out, portal-unavailable on bind, portal-unavailable on
        // session-create.

        // Row 1: bind returns Ok(...) → Completed.
        let portal = ScenarioPortal::new(
            Ok(SessionId::new("scenario")),
            Some(BindFakeOutcome::Immediate(Ok(vec![BoundShortcut {
                id: SHORTCUT_ID.to_owned(),
                trigger_description: "Ctrl+Alt+R".to_owned(),
                description: SHORTCUT_DESCRIPTION.to_owned(),
            }]))),
        );
        let outcome = run_rebind(portal, one_second(), PORTAL_APP_ID).await;
        match outcome {
            RebindOutcome::Completed { description } => {
                assert_eq!(description, "Ctrl+Alt+R");
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // Row 2: bind returns BindCancelled → Cancelled.
        let portal = ScenarioPortal::new(
            Ok(SessionId::new("scenario")),
            Some(BindFakeOutcome::Immediate(Err(PortalError::BindCancelled))),
        );
        assert_eq!(
            run_rebind(portal, one_second(), PORTAL_APP_ID).await,
            RebindOutcome::Cancelled
        );

        // Row 3: bind stalls forever → TimedOut.
        let portal = ScenarioPortal::new(
            Ok(SessionId::new("scenario")),
            Some(BindFakeOutcome::StallForever),
        );
        let outcome = run_rebind(portal, five_millis(), PORTAL_APP_ID).await;
        assert_eq!(outcome, RebindOutcome::TimedOut);

        // Row 4: bind returns Unavailable → Unavailable.
        let portal = ScenarioPortal::new(
            Ok(SessionId::new("scenario")),
            Some(BindFakeOutcome::Immediate(Err(PortalError::Unavailable))),
        );
        assert_eq!(
            run_rebind(portal, one_second(), PORTAL_APP_ID).await,
            RebindOutcome::Unavailable
        );

        // Row 5: create_session returns Unavailable → Unavailable
        // (separate path: session never opens, so bind is not
        // attempted).
        let portal = ScenarioPortal::new(Err(PortalError::Unavailable), None);
        assert_eq!(
            run_rebind(portal, one_second(), PORTAL_APP_ID).await,
            RebindOutcome::Unavailable
        );

        // Row 6 (bonus): create_session returns SessionLost → Failed.
        let portal = ScenarioPortal::new(Err(PortalError::SessionLost), None);
        match run_rebind(portal, one_second(), PORTAL_APP_ID).await {
            RebindOutcome::Failed(msg) => {
                assert!(msg.contains("session create"), "{msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_current_binding_renders_not_bound_when_empty() {
        let portal = ScenarioPortal::new(Ok(SessionId::new("scenario")), None);
        let result = load_current_binding(portal).await.unwrap();
        assert_eq!(result, "(not bound)");
    }
}
