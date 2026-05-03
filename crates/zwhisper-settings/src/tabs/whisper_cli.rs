//! M7 — Whisper-CLI detector tab (`DoD` #14).
//!
//! Owns three FLTK widgets (status label, hint frame, refresh
//! button) plus a once-only auto-detect that fires at tab build.
//! The detection logic is the shared `zwhisper_core::transcribe::
//! discovery::detect_whisper_cli` (M7-plan § D1) — wrapping it in
//! `tokio::task::spawn_blocking` because the underlying lookup is
//! sync filesystem + `which::which` work and must not block the
//! shared async runtime's worker.
//!
//! ### State surface
//!
//! `Detected(Ok(path))` → green "Detected at /path/to/whisper-cli".
//! `Detected(Err(msg))` → yellow status + multiline install hints
//! sourced from `IDEA.md § 4` (pacman / GitHub releases / env var
//! override).
//!
//! ### Plan deviation (documented)
//!
//! The plan's `MultipleFound{paths}` state is intentionally NOT
//! rendered: the production locator returns the first hit per
//! [`zwhisper_core::transcribe::discovery::locate_with`] precedence
//! order, so the "multiple installations" signal is unobservable
//! without a second locator pass. Rendering only `Found` /
//! `NotFound` keeps the UI honest about what we can actually
//! detect today.

use std::path::PathBuf;

use fltk::{
    button::Button,
    enums::{Color, Font, FrameType},
    frame::Frame,
    group::{Group, Tabs},
    prelude::*,
};
use tracing::{debug, warn};
use zwhisper_core::transcribe::discovery::detect_whisper_cli;

use crate::app::UiMessage;
use crate::runtime::UiBridge;

/// Pixel size of every full-width row inside the tab. FLTK
/// y-coordinates are absolute, so we keep them as named constants.
const ROW_HEIGHT: i32 = 28;

/// Vertical padding between rows.
const ROW_GAP: i32 = 8;

/// Inset from the parent group's edges.
const PADDING: i32 = 12;

/// Pixel height of the multi-line install-hint frame. Chosen to
/// fit four lines at FLTK's default font metrics without the
/// banner being clipped at 1.5× scaling.
const HINT_HEIGHT: i32 = 110;

/// Refresh button width.
const BUTTON_WIDTH: i32 = 110;

/// Static install hints from `IDEA.md § 4`. Rendered when the
/// detector fails — gives the user three independent recovery
/// paths without leaving the tab.
const NOT_FOUND_HINT: &str = "Install one of:\n\
     - Arch: pacman -S whisper.cpp\n\
     - Manual: download from https://github.com/ggml-org/whisper.cpp/releases\n\
     - Override: export ZWHISPER_WHISPER_CLI=/path/to/whisper-cli";

/// Cross-thread messages produced by the whisper-cli detector.
///
/// `Detected` carries the resolver outcome: `Ok(path)` means an
/// executable was located; `Err(message)` carries a human-readable
/// reason (the formatted [`zwhisper_core::transcribe::error::TranscribeError`]).
/// `RefreshRequested` is fired by the Refresh button; the
/// dispatcher reschedules the detector task.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "fields read by the app.rs dispatcher in a follow-up wiring task"
)]
pub(crate) enum WhisperCliMsg {
    /// Detector finished — render `Found` or `NotFound`.
    Detected(Result<PathBuf, String>),
    /// User clicked Refresh — re-run the detector on a blocking
    /// task.
    RefreshRequested,
}

/// Holds the FLTK widgets belonging to the whisper-cli tab.
///
/// Widgets are kept on the struct so future updates (Group D's
/// dispatcher in `app.rs`) can locate them without re-querying
/// the FLTK widget tree.
#[derive(Clone, Debug)]
pub(crate) struct WhisperCliTab {
    #[allow(dead_code, reason = "kept alive for the FLTK widget tree")]
    group: Group,
    #[allow(dead_code, reason = "updated via UiMessage::WhisperCli dispatcher")]
    status_label: Frame,
    #[allow(dead_code, reason = "updated via UiMessage::WhisperCli dispatcher")]
    hint_frame: Frame,
    #[allow(dead_code, reason = "wired through callback closure")]
    refresh_button: Button,
}

/// Construct the whisper-cli tab and dispatch one initial detect
/// task. The button's callback re-spawns the detector via the
/// shared `UiBridge` runtime handle.
#[allow(
    clippy::needless_pass_by_value,
    reason = "build() takes UiBridge by value to match the sibling tab signatures (profile.rs, models.rs, hotkey.rs)"
)]
pub(crate) fn build(parent: &mut Tabs, bridge: UiBridge) -> WhisperCliTab {
    let (gx, gy, gw, gh) = parent.client_area();
    let group = Group::new(gx, gy, gw, gh, "Whisper-CLI");

    let inner_w = gw - PADDING * 2;
    let mut y = gy + PADDING;

    // Status label — initial state announces an in-flight detect.
    let mut status_label = Frame::new(
        gx + PADDING,
        y,
        inner_w,
        ROW_HEIGHT,
        "whisper-cli: detecting…",
    );
    status_label.set_label_font(Font::Helvetica);
    status_label.set_frame(FrameType::FlatBox);
    status_label.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);
    y += ROW_HEIGHT + ROW_GAP;

    // Hint frame — multiline install instructions, hidden until
    // the detector reports `NotFound`.
    let mut hint_frame = Frame::new(gx + PADDING, y, inner_w, HINT_HEIGHT, "");
    hint_frame.set_frame(FrameType::FlatBox);
    hint_frame.set_label_font(Font::Courier);
    hint_frame
        .set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside | fltk::enums::Align::Top);
    hint_frame.hide();
    y += HINT_HEIGHT + ROW_GAP;

    // Refresh button — left-aligned, fixed width.
    let mut refresh_button = Button::new(gx + PADDING, y, BUTTON_WIDTH, ROW_HEIGHT, "Refresh");
    let cb_bridge = bridge.clone();
    refresh_button.set_callback(move |_btn| {
        if let Err(send_err) = cb_bridge
            .tx
            .send(UiMessage::WhisperCli(WhisperCliMsg::RefreshRequested))
        {
            warn!(error = %send_err, "whisper-cli: refresh enqueue failed");
        }
    });

    group.end();
    parent.add(&group);

    // Kick off the initial detection. Same code path as Refresh
    // so the first paint has parity with subsequent re-runs.
    spawn_detect_task(&bridge, detect_whisper_cli_via_default);

    WhisperCliTab {
        group,
        status_label,
        hint_frame,
        refresh_button,
    }
}

/// Spawn a `spawn_blocking` task that runs the discovery function
/// and pushes the outcome onto the cross-thread channel. The
/// `detect_fn` parameter is what makes the tab unit-testable — the
/// production caller passes [`detect_whisper_cli_via_default`]
/// (a thin wrapper around [`detect_whisper_cli`]); tests pass an
/// arbitrary `Fn() -> Result<PathBuf, String>` to drive deterministic
/// outcomes without spawning a real runtime.
pub(crate) fn spawn_detect_task<F>(bridge: &UiBridge, detect_fn: F)
where
    F: Fn() -> Result<PathBuf, String> + Send + Sync + 'static,
{
    let tx = bridge.tx.clone();
    let _join = bridge.rt_handle.spawn(async move {
        let outcome = tokio::task::spawn_blocking(detect_fn).await;
        let mapped = match outcome {
            Ok(Ok(path)) => Ok(path),
            Ok(Err(msg)) => Err(msg),
            Err(join_err) => Err(format!("whisper-cli detection task panicked: {join_err}")),
        };
        if let Err(send_err) = tx.send(UiMessage::WhisperCli(WhisperCliMsg::Detected(mapped))) {
            debug!(error = %send_err, "whisper-cli: receiver gone, dropping result");
        }
        // Nudge FLTK so the main loop drains the channel on next
        // iteration. Safe to call from any thread.
        fltk::app::awake();
    });
}

/// Production detect closure — wraps [`detect_whisper_cli`] and
/// stringifies the error so the worker only emits `Send` payloads.
fn detect_whisper_cli_via_default() -> Result<PathBuf, String> {
    detect_whisper_cli().map_err(|err| err.to_string())
}

/// Pure render helper — takes a state and produces the
/// (`label_text`, `label_color`, `hint_text_or_empty`) triple the
/// dispatcher should paint into the widgets. Factored out so unit
/// tests can assert the visible behaviour without touching FLTK.
#[must_use]
pub(crate) fn render_state(state: &Result<PathBuf, String>) -> (String, Color, &'static str) {
    match state {
        Ok(path) => (
            format!("whisper-cli: Detected at {}", path.display()),
            Color::DarkGreen,
            "",
        ),
        Err(_) => (
            "whisper-cli: not found".to_owned(),
            Color::DarkYellow,
            NOT_FOUND_HINT,
        ),
    }
}

/// Dispatch entry: `app.rs` calls this for every
/// `UiMessage::WhisperCli`. `RefreshRequested` is handled inside
/// the button callback (see `build`) — here we only need to render
/// freshly arrived `Detected` outcomes.
pub(crate) fn apply_msg(tab: &mut WhisperCliTab, msg: &WhisperCliMsg) {
    if let WhisperCliMsg::Detected(state) = msg {
        apply_state(tab, state);
    }
}

/// Apply [`render_state`] output to a tab's widgets. The
/// dispatcher in `app.rs` will call this once `UiMessage::WhisperCli`
/// arrives. Kept on the tab so the widget references do not have
/// to leak across module boundaries.
pub(crate) fn apply_state(tab: &mut WhisperCliTab, state: &Result<PathBuf, String>) {
    let (label, colour, hint) = render_state(state);
    tab.status_label.set_label(&label);
    tab.status_label.set_label_color(colour);
    if hint.is_empty() {
        tab.hint_frame.hide();
        tab.hint_frame.set_label("");
    } else {
        tab.hint_frame.set_label(hint);
        tab.hint_frame.set_label_color(Color::Black);
        tab.hint_frame.show();
    }
    if let Some(mut parent) = tab.status_label.parent() {
        parent.redraw();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::app::UiMessage;
    use crate::runtime::UiBridge;

    /// Build a `UiBridge` against the *current* tokio runtime — the
    /// tests below run inside `#[tokio::test]`, which already
    /// installs a multi-thread runtime. Reusing that runtime via
    /// `Handle::current()` avoids the "Cannot drop a runtime in a
    /// context where blocking is not allowed" panic that occurs
    /// when a nested runtime is dropped from inside `block_on`.
    fn fixture() -> (UiBridge, mpsc::UnboundedReceiver<UiMessage>) {
        let (tx, rx) = mpsc::unbounded_channel::<UiMessage>();
        let bridge = UiBridge {
            tx,
            rt_handle: tokio::runtime::Handle::current(),
            cancel_token: CancellationToken::new(),
        };
        (bridge, rx)
    }

    /// Pull the next `WhisperCliMsg` off the rx, panicking on
    /// timeout. Wraps the raw `UiMessage::WhisperCli` envelope.
    async fn expect_whisper_msg(rx: &mut mpsc::UnboundedReceiver<UiMessage>) -> WhisperCliMsg {
        let envelope = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("whisper-cli msg timed out")
            .expect("channel closed");
        match envelope {
            UiMessage::WhisperCli(msg) => msg,
            other => panic!("expected WhisperCli, got {other:?}"),
        }
    }

    #[test]
    fn render_state_found_uses_green_and_empty_hint() {
        let path = PathBuf::from("/usr/bin/whisper-cli");
        let (label, colour, hint) = render_state(&Ok(path));
        assert!(
            label.contains("Detected at /usr/bin/whisper-cli"),
            "{label}"
        );
        assert_eq!(colour, Color::DarkGreen);
        assert!(hint.is_empty());
    }

    #[test]
    fn render_state_not_found_uses_yellow_and_renders_hints() {
        let (label, colour, hint) = render_state(&Err("backend unavailable".to_owned()));
        assert!(label.contains("not found"), "{label}");
        assert_eq!(colour, Color::DarkYellow);
        assert!(hint.contains("pacman -S whisper.cpp"), "{hint}");
        assert!(
            hint.contains("https://github.com/ggml-org/whisper.cpp/releases"),
            "{hint}"
        );
        assert!(hint.contains("ZWHISPER_WHISPER_CLI"), "{hint}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_picks_up_late_install() {
        // `DoD` #14 — first detection fails, user installs the
        // binary, presses Refresh, second detection succeeds.
        let (bridge, mut rx) = fixture();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let detect = move || {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err("not installed".to_owned())
            } else {
                Ok(PathBuf::from("/usr/local/bin/whisper-cli"))
            }
        };

        // First detection — must surface as Err.
        spawn_detect_task(&bridge, detect.clone());
        let first = expect_whisper_msg(&mut rx).await;
        match first {
            WhisperCliMsg::Detected(Err(msg)) => {
                assert!(msg.contains("not installed"), "{msg}");
            }
            other => panic!("expected Detected(Err(..)), got {other:?}"),
        }

        // Refresh fires a second detection — must surface as Ok.
        spawn_detect_task(&bridge, detect);
        let second = expect_whisper_msg(&mut rx).await;
        match second {
            WhisperCliMsg::Detected(Ok(path)) => {
                assert_eq!(path, PathBuf::from("/usr/local/bin/whisper-cli"));
            }
            other => panic!("expected Detected(Ok(..)), got {other:?}"),
        }

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initial_state_shows_detecting_then_resolved() {
        // Smoke: a single detect call lands on the channel as
        // `Detected(...)`. Confirms the spawn-blocking + awake
        // pipeline ferries the outcome end-to-end.
        let (bridge, mut rx) = fixture();
        spawn_detect_task(&bridge, || Ok(PathBuf::from("/opt/whisper-cli")));
        let msg = expect_whisper_msg(&mut rx).await;
        match msg {
            WhisperCliMsg::Detected(Ok(path)) => {
                assert_eq!(path, PathBuf::from("/opt/whisper-cli"));
            }
            other => panic!("expected Detected(Ok(..)), got {other:?}"),
        }
    }
}
