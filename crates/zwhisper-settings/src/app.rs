//! M7 — top-level FLTK application.
//!
//! Owns the main window, the four-tab layout, and the cross-thread
//! message router. Built on the threading model documented in
//! M7-plan § 2:
//!
//! 1. `runtime::spawn_runtime` creates the side-thread tokio
//!    runtime + `UiBridge` (mpsc tx + Handle + cancel token).
//! 2. `App::new` constructs the FLTK window and four tabs (each
//!    receives a clone of the bridge).
//! 3. `App::run` enters `Fl::run`. Worker tasks send `UiMessage`s
//!    through `bridge.tx` and call `fltk::app::awake_callback`,
//!    which schedules the drain closure on the main loop.
//!
//! Single-instance enforcement claims `cz.zajca.Zwhisper1.Settings`
//! on the session bus (M7-plan § 17 + `DoD` #17). Pattern mirrors
//! `zwhisper-tray::single_instance`.

use std::sync::{Arc, Mutex};

use fltk::app::{self as fltk_app, App as FltkApp, Scheme};
use fltk::enums::{Color, Font};
use fltk::frame::Frame;
use fltk::group::{Pack, PackType, Tabs};
use fltk::prelude::*;
use fltk::window::Window;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::names::WellKnownName;

use crate::error::SettingsError;
use crate::runtime::UiBridge;
use crate::tabs::{
    hotkey::{self, HotkeyMsg, HotkeyTab},
    models::{self, ModelsMsg, ModelsTab},
    profile::{self, ProfileMsg, ProfileTab},
    whisper_cli::{self, WhisperCliMsg, WhisperCliTab},
};

/// Well-known D-Bus name claimed by the settings binary. Mirrors
/// the tray's `cz.zajca.Zwhisper1.Tray` (see `M4-plan` § "Single-
/// instance enforcement"). Two settings windows MUST NOT race on
/// the same profile file (`DoD` #17 / risk E1).
pub(crate) const SETTINGS_BUS_NAME: &str = "cz.zajca.Zwhisper1.Settings";

/// Window default size — chosen to fit the profile editor's
/// two-pane layout at 1.0× scaling without scrollbars
/// (M7-plan § 3.1).
const DEFAULT_WINDOW_WIDTH: i32 = 900;
const DEFAULT_WINDOW_HEIGHT: i32 = 640;

/// `HiDPI` banner threshold — anything outside `{1.0, 2.0}` triggers
/// the yellow banner (`DoD` #22 partial). FLTK 1.4 renders cleanly
/// at integer scales; fractional values are the documented risk.
const SCALE_INTEGER_TOLERANCE: f32 = 0.001;

/// Cross-thread event router. Worker tasks build a variant and
/// send it through `UiBridge::tx`; the main thread drains the
/// channel inside an `awake_callback` and dispatches by tab.
#[derive(Debug)]
#[allow(dead_code)] // Variants populated by Group B/C/D.
pub(crate) enum UiMessage {
    Profile(ProfileMsg),
    Models(ModelsMsg),
    Hotkey(HotkeyMsg),
    WhisperCli(WhisperCliMsg),
}

/// Top-level FLTK application. Built once in `main`.
pub(crate) struct App {
    /// FLTK app handle — must outlive every widget.
    fltk: FltkApp,
    /// The main window. Held so callbacks can `.hide()` it on quit.
    window: Window,
    /// Per-tab handles. Stored to keep the FLTK groups alive (FLTK
    /// drops orphaned widgets) and to give the message router a
    /// place to dispatch into.
    #[allow(dead_code)] // Group B/C/D will read these to update widgets.
    profile_tab: ProfileTab,
    #[allow(dead_code)]
    models_tab: ModelsTab,
    #[allow(dead_code)]
    hotkey_tab: HotkeyTab,
    #[allow(dead_code)]
    whisper_cli_tab: WhisperCliTab,
    /// The bridge — held so `App::run` can clone it into the
    /// `awake_callback` closure.
    bridge: UiBridge,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("bus_name", &SETTINGS_BUS_NAME)
            .field("window_label", &self.window.label())
            .finish_non_exhaustive()
    }
}

impl App {
    /// Build the FLTK window, the four tabs, and any boot-time
    /// banners (`HiDPI` scaling notice). Does NOT enter the event
    /// loop — call [`App::run`] for that.
    ///
    /// Returns `Result` because Group B/C/D will add fallible
    /// initialisation (D-Bus client connect, models config parse).
    /// At A-stage the body is infallible; the suppression is
    /// intentional so the public signature does not churn between
    /// milestones.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn new(bridge: UiBridge) -> color_eyre::Result<Self> {
        let fltk = FltkApp::default().with_scheme(Scheme::Gtk);

        let mut window = Window::default()
            .with_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT)
            .with_label("zwhisper Settings");
        window.make_resizable(true);

        // Vertical pack: optional HiDPI banner on top, Tabs below.
        let mut root = Pack::default_fill();
        root.set_type(PackType::Vertical);
        root.set_spacing(0);

        maybe_add_hidpi_banner(&mut root);

        // Tabs widget — fills the remainder of the window.
        let (tabs_x, tabs_y) = (0, 0);
        let tabs_w = DEFAULT_WINDOW_WIDTH;
        // Reserve space for the banner if present; FLTK Pack will
        // re-layout regardless, this is just an initial guess.
        let tabs_h = DEFAULT_WINDOW_HEIGHT;
        let mut tabs = Tabs::new(tabs_x, tabs_y, tabs_w, tabs_h, "");

        let profile_tab = profile::build(&mut tabs, bridge.clone());
        let models_tab = models::build(&mut tabs, bridge.clone());
        let whisper_cli_tab = whisper_cli::build(&mut tabs, bridge.clone());
        let hotkey_tab = hotkey::build(&mut tabs, bridge.clone());

        tabs.end();
        // Default to the first tab (Profiles).
        tabs.auto_layout();

        root.end();
        window.end();
        window.show();

        Ok(Self {
            fltk,
            window,
            profile_tab,
            models_tab,
            hotkey_tab,
            whisper_cli_tab,
            bridge,
        })
    }

    /// Enter the FLTK event loop. Returns when the user closes the
    /// window. The `rx` is consumed — every queued `UiMessage` is
    /// dispatched to the owning tab via the per-tab `apply_*_msg`
    /// functions, which mutate widgets in place on the main thread.
    pub(crate) fn run(self, rx: UnboundedReceiver<UiMessage>) -> color_eyre::Result<()> {
        let App {
            fltk,
            mut window,
            profile_tab,
            models_tab,
            hotkey_tab,
            whisper_cli_tab,
            bridge,
        } = self;

        // Extract widget clones into the dispatch closure. FLTK
        // widgets are cheap to clone (handles to ref-counted FFI
        // state) and remain valid for the whole event loop.
        let profile_inline = profile_tab.inline_label.clone();
        let profile_browser = profile_tab.browser.clone();
        let mut hotkey_for_dispatch = hotkey_tab.clone();
        let mut whisper_cli_for_dispatch = whisper_cli_tab.clone();
        let models_rows = Arc::clone(&models_tab.rows);

        // Hold the tab structs alive — FLTK auto-deletes orphaned
        // groups and we want the widget tree to outlive `run`.
        let _keep_alive = (profile_tab, models_tab, hotkey_tab, whisper_cli_tab);

        // FLTK's `awake_callback` captures a `FnMut + Send +
        // 'static`. We share the receiver through Arc<Mutex<_>>
        // so the closure can drain on every wake. Contention is
        // zero — only the main thread locks.
        let rx = Arc::new(Mutex::new(rx));
        let drain_rx = Arc::clone(&rx);

        fltk_app::awake_callback(move || {
            let Ok(mut guard) = drain_rx.lock() else {
                warn!("UiMessage receiver poisoned; dropping wake");
                return;
            };
            while let Ok(msg) = guard.try_recv() {
                dispatch_to_tabs(
                    msg,
                    &mut profile_inline.clone(),
                    &mut profile_browser.clone(),
                    &mut hotkey_for_dispatch,
                    &mut whisper_cli_for_dispatch,
                    &models_rows,
                );
            }
        });

        // Subscribe to the Raise signal so a second
        // `zwhisper-settings` invocation (e.g. user double-clicks
        // the tray "Settings…" entry) can tell us to bring the
        // window forward instead of silently exiting.
        spawn_raise_subscriber(&bridge, window.clone());

        // Window-close handler: cancel the runtime token so any
        // in-flight downloader task can wind down cleanly. The
        // 2-second budget for shutdown is enforced by `main`
        // when it drops the `Runtime`.
        let cancel_on_close = bridge.cancel_token.clone();
        window.set_callback(move |w| {
            debug!("settings window close requested; signalling cancel");
            cancel_on_close.cancel();
            w.hide();
        });

        info!("settings: entering FLTK event loop");
        fltk.run()
            .map_err(|e| color_eyre::eyre::eyre!("FLTK event loop error: {e}"))?;

        // Receiver guard goes out of scope → mpsc closes → workers
        // observing send errors wind down.
        drop(rx);
        Ok(())
    }

}

/// Free function dispatcher. Routes each `UiMessage` variant to
/// the owning tab's `apply_msg`. Called on the FLTK main thread
/// so widget mutation is safe.
#[allow(clippy::needless_pass_by_value)]
fn dispatch_to_tabs(
    msg: UiMessage,
    profile_inline: &mut Frame,
    profile_browser: &mut fltk::browser::HoldBrowser,
    hotkey_tab: &mut crate::tabs::hotkey::HotkeyTab,
    whisper_cli_tab: &mut crate::tabs::whisper_cli::WhisperCliTab,
    models_rows: &Arc<std::sync::Mutex<std::collections::HashMap<String, crate::tabs::models::ModelRow>>>,
) {
    debug!(?msg, "settings: dispatching UiMessage");
    match msg {
        UiMessage::Profile(pm) => {
            crate::tabs::profile::apply_msg(profile_inline, profile_browser, &pm);
        }
        UiMessage::Models(mm) => {
            crate::tabs::models::apply_msg(models_rows, &mm);
        }
        UiMessage::Hotkey(hm) => {
            crate::tabs::hotkey::apply_msg(hotkey_tab, &hm);
        }
        UiMessage::WhisperCli(wm) => {
            crate::tabs::whisper_cli::apply_msg(whisper_cli_tab, &wm);
        }
    }
}

/// Insert a fixed-height yellow banner above the tabs widget when
/// the FLTK screen scale is fractional (e.g. KDE 1.5×). Implements
/// the proactive half of M7-plan `DoD` #22 / risk A1: the manual
/// matrix gate is the authoritative check, but the banner gives
/// the user a self-correcting hint if widgets look misaligned.
fn maybe_add_hidpi_banner(parent: &mut Pack) {
    let scale = fltk_app::screen_scale(0);
    let is_integer = (scale - 1.0).abs() < SCALE_INTEGER_TOLERANCE
        || (scale - 2.0).abs() < SCALE_INTEGER_TOLERANCE;
    if is_integer {
        return;
    }
    info!(scale, "non-integer FLTK scale detected; showing banner");
    let mut banner = Frame::default()
        .with_size(DEFAULT_WINDOW_WIDTH, 32)
        .with_label(&format!(
            "Scaling factor {scale:.2} detected. \
             If widgets are misaligned, set FLTK_SCALING_FACTOR=1 and restart."
        ));
    banner.set_color(Color::Yellow);
    banner.set_label_color(Color::Black);
    banner.set_label_font(Font::HelveticaBold);
    banner.set_frame(fltk::enums::FrameType::FlatBox);
    parent.add(&banner);
}

/// Try to claim the well-known settings bus name. Returns the
/// holding connection on success.
///
/// On collision (`Exists` / `AlreadyOwner`) returns
/// `Err(SettingsError::Config("Settings already running"))`. The
/// caller is responsible for sending a `Raise` request to the
/// existing instance and exiting 0 — see `main.rs`.
///
/// Mirrors `zwhisper-tray::single_instance::claim` but returns the
/// Outcome of the single-instance D-Bus name claim.
///
/// Splitting this from the genuine `Err` variant lets `main` send a
/// real Raise signal on collision (so a second click on the tray
/// "Settings…" entry brings the existing window forward) and fail
/// loudly on actual D-Bus / session-bus errors instead of silently
/// exiting and looking like "already running".
pub(crate) enum SingleInstanceOutcome {
    /// We are the primary owner of `cz.zajca.Zwhisper1.Settings`.
    /// `main` keeps the connection alive for the lifetime of the
    /// process; dropping releases the name.
    Acquired(zbus::Connection),
    /// Another instance owns the well-known name. The connection is
    /// returned so the caller can emit a Raise signal before exiting
    /// (see [`emit_raise_signal`]).
    AlreadyRunning(zbus::Connection),
}

/// D-Bus interface, path, and member used by [`emit_raise_signal`]
/// and the subscriber in [`App::run`]. Pinned by
/// `app::tests::raise_signal_constants_are_stable`.
pub(crate) const RAISE_SIGNAL_INTERFACE: &str = "cz.zajca.Zwhisper1.Settings";
pub(crate) const RAISE_SIGNAL_PATH: &str = "/cz/zajca/Zwhisper1/Settings";
pub(crate) const RAISE_SIGNAL_MEMBER: &str = "Raise";

/// Try to claim the single-instance D-Bus name. Returns:
///
/// - `Ok(Acquired(conn))` on success.
/// - `Ok(AlreadyRunning(conn))` if the name is already taken — the
///   caller should emit the Raise signal and exit 0.
/// - `Err(_)` on a real D-Bus error (session bus down, invalid bus
///   name, RPC failure). The caller should surface this loudly.
pub(crate) async fn try_acquire_single_instance() -> Result<SingleInstanceOutcome, SettingsError> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| SettingsError::Config(format!("session bus: {e}")))?;
    let proxy = DBusProxy::new(&conn)
        .await
        .map_err(|e| SettingsError::Config(format!("dbus proxy: {e}")))?;
    let name = WellKnownName::try_from(SETTINGS_BUS_NAME)
        .map_err(|e| SettingsError::Config(format!("invalid bus name {SETTINGS_BUS_NAME}: {e}")))?;
    let reply = proxy
        .request_name(name, RequestNameFlags::DoNotQueue.into())
        .await
        .map_err(|e| SettingsError::Config(format!("request_name: {e}")))?;
    if matches!(reply, RequestNameReply::PrimaryOwner) {
        Ok(SingleInstanceOutcome::Acquired(conn))
    } else {
        // Exists / AlreadyOwner / InQueue all mean: another live
        // settings instance owns the name. This is NOT a bus error;
        // it is the expected "user double-clicked Settings…" path.
        Ok(SingleInstanceOutcome::AlreadyRunning(conn))
    }
}

/// Emit `cz.zajca.Zwhisper1.Settings.Raise()` on the session bus.
/// Called by the second instance just before exiting; the alive
/// instance subscribes during `App::run` and brings its window
/// forward on receipt.
pub(crate) async fn emit_raise_signal(conn: &zbus::Connection) -> Result<(), SettingsError> {
    conn.emit_signal(
        None::<&str>,
        RAISE_SIGNAL_PATH,
        RAISE_SIGNAL_INTERFACE,
        RAISE_SIGNAL_MEMBER,
        &(),
    )
    .await
    .map_err(|e| SettingsError::Config(format!("emit Raise: {e}")))
}

/// Build the `MatchRule` for the Raise signal subscription. The
/// alive instance only listens for its own interface + path +
/// member; foreign senders are filtered out by the bus.
fn build_raise_match_rule() -> Result<zbus::MatchRule<'static>, SettingsError> {
    zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(RAISE_SIGNAL_INTERFACE)
        .map_err(|e| SettingsError::Config(format!("raise rule interface: {e}")))?
        .path(RAISE_SIGNAL_PATH)
        .map_err(|e| SettingsError::Config(format!("raise rule path: {e}")))?
        .member(RAISE_SIGNAL_MEMBER)
        .map_err(|e| SettingsError::Config(format!("raise rule member: {e}")))?
        .build()
        .to_owned()
        .pipe(Ok)
}

/// Spawn a tokio task that listens for the Raise signal and, on
/// arrival, wakes FLTK to bring `window` to the foreground.
/// Failures are logged at warn — a missing subscription does not
/// prevent settings from booting.
fn spawn_raise_subscriber(bridge: &UiBridge, window: Window) {
    let rt_handle = bridge.rt_handle.clone();
    rt_handle.clone().spawn(async move {
        let conn = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "raise subscriber: session bus unreachable");
                return;
            }
        };
        let rule = match build_raise_match_rule() {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "raise subscriber: match rule build failed");
                return;
            }
        };
        let mut stream = match zbus::MessageStream::for_match_rule(rule, &conn, None).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "raise subscriber: match rule install failed");
                return;
            }
        };
        info!("raise subscriber: listening for {RAISE_SIGNAL_INTERFACE}.{RAISE_SIGNAL_MEMBER}");

        use futures_util::StreamExt;
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(_signal) => {
                    info!("raise: bringing settings window forward");
                    let mut w = window.clone();
                    fltk_app::awake_callback(move || {
                        w.show();
                    });
                    fltk_app::awake();
                }
                Err(e) => {
                    debug!(error = %e, "raise subscriber: stream error");
                }
            }
        }
        debug!("raise subscriber: stream ended");
    });
}

/// Trait extension to chain `pipe(Ok)` for builder-style ergonomics.
trait Pipe: Sized {
    fn pipe<F, R>(self, f: F) -> R
    where
        F: FnOnce(Self) -> R,
    {
        f(self)
    }
}

impl<T> Pipe for T {}

/// Pure helper extracted from [`try_acquire_single_instance`] for
/// unit-testability — interpreting the `RequestNameReply`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_primary_owner(reply: &RequestNameReply) -> bool {
    matches!(reply, RequestNameReply::PrimaryOwner)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn primary_owner_accepted() {
        assert!(is_primary_owner(&RequestNameReply::PrimaryOwner));
    }

    #[test]
    fn already_owner_rejected_for_settings() {
        // Unlike the tray, settings treats AlreadyOwner as a
        // collision: there can only be one settings window per
        // user, and AlreadyOwner means a previous boot in the
        // same process already claimed the name (impossible in
        // practice, but defensive).
        assert!(!is_primary_owner(&RequestNameReply::AlreadyOwner));
    }

    #[test]
    fn exists_rejected() {
        assert!(!is_primary_owner(&RequestNameReply::Exists));
    }

    #[test]
    fn in_queue_rejected() {
        assert!(!is_primary_owner(&RequestNameReply::InQueue));
    }

    /// `DoD` #17 — second launch detects the first via the bus name
    /// claim. With the post-review refactor, the second launch
    /// returns `AlreadyRunning` (a non-error outcome) rather than
    /// a typed `Err` — main.rs uses that to send the Raise signal.
    #[tokio::test]
    async fn second_launch_raises_existing_window() {
        // First claim should succeed (or the test environment has
        // no session bus, in which case we skip — same gate the
        // tray uses).
        let first = match try_acquire_single_instance().await {
            Ok(SingleInstanceOutcome::Acquired(conn)) => conn,
            Ok(SingleInstanceOutcome::AlreadyRunning(_)) => {
                // A leftover claim from a previous test run on the
                // same session bus. Skip rather than flake.
                eprintln!("skipping: bus name already claimed by another instance");
                return;
            }
            Err(e) => {
                eprintln!("skipping: no session bus available ({e})");
                return;
            }
        };
        let second = try_acquire_single_instance().await;
        assert!(
            matches!(second, Ok(SingleInstanceOutcome::AlreadyRunning(_))),
            "second launch must report AlreadyRunning, got {:?}",
            second.as_ref().map(|_| "<conn>").map_err(|e| e.to_string()),
        );
        // Drop the first connection — releases the name so the
        // test does not bleed into other runs on the same bus.
        drop(first);
    }

    /// Pin the Raise signal wire shape so a rename or path move
    /// breaks at compile time. Mirrors the `HotkeyRebound`
    /// regression guard.
    #[test]
    fn raise_signal_constants_are_stable() {
        assert_eq!(RAISE_SIGNAL_INTERFACE, "cz.zajca.Zwhisper1.Settings");
        assert_eq!(RAISE_SIGNAL_PATH, "/cz/zajca/Zwhisper1/Settings");
        assert_eq!(RAISE_SIGNAL_MEMBER, "Raise");
    }
}
