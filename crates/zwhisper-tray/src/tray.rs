//! Task C — `ksni::Tray` implementation.
//!
//! Bridges the in-memory [`TrayState`] (owned by the signal pump and
//! published via `tokio::sync::watch`) into the synchronous ksni
//! `Tray` trait callbacks.
//!
//! ## Design
//!
//! - The renderer holds a snapshot of [`TrayState`] inside the
//!   [`ZwhisperTray`] struct. The supervisor task (see
//!   [`crate::supervisor`]) propagates new snapshots into this struct
//!   via `Handle::update(|tray| tray.set_state(snapshot))`.
//! - Menu callbacks are synchronous (`&mut Self`) and must NOT block;
//!   they push commands onto a pre-bound `tokio::sync::mpsc::Sender`
//!   which the P4 dispatcher will consume. P3 ships a stub consumer
//!   in `main.rs` that just logs and drops each command.
//! - The "Quit" menu item gets a separate `mpsc::Sender<()>` so the
//!   main task can distinguish a user-initiated shutdown (exit 0)
//!   from a ksni-died crash (exit 1, see [`crate::supervisor`]).
//!
//! ## Per-state menu enablement
//!
//! Per M4-plan § "Menu items" and `DoD` #21 (optimistic action lock),
//! Start/Stop are only enabled when the daemon is in a state where
//! they are valid AND there is no pending command in flight. The
//! enablement table is factored into the pure [`menu_flags_for`]
//! function so it can be unit-tested without spinning up a ksni
//! service. The actual `MenuItem<Self>` builder reads from that
//! struct.

use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{Category, MenuItem, Status, ToolTip, Tray};
use tokio::sync::mpsc;
use tracing::warn;

use crate::hotkey::HotkeyControl;
use crate::icon::{icon_for_state, state_label_for, tooltip_text};
use crate::state::{HotkeyMenuState, IconState, PendingCmd, TrayState};

// `COMMAND_CHANNEL_CAPACITY` lives in `crate::config` (per CLAUDE.md
// "all configuration in a dedicated module"); a re-export keeps
// existing import paths from main.rs intact while the canonical
// definition is in one place.
pub use crate::config::COMMAND_CHANNEL_CAPACITY;

/// One row in the profile submenu, computed by [`menu_flags_for`].
///
/// `active = true` produces a checked radio mark; `enabled = false`
/// dims the row (`DoD` #20 — switching profiles mid-recording is a
/// stress-test fix). `cloud = true` causes the rendered label to be
/// prefixed with `☁ ` (M5 `DoD` #8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileMenuEntry {
    pub name: String,
    pub active: bool,
    pub enabled: bool,
    pub cloud: bool,
}

/// Pure-data view of which menu items should be enabled and what the
/// header should say. Extracted so tests can assert per-state
/// behaviour without instantiating a `ksni::MenuItem` (whose generic
/// parameter and boxed callback are not directly inspectable).
///
/// The five boolean fields are independent menu-item flags — there
/// is no state machine that constrains their combination — so the
/// pedantic `struct_excessive_bools` lint is not appropriate here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuFlags {
    pub start_enabled: bool,
    pub stop_enabled: bool,
    pub open_last_recording_enabled: bool,
    pub open_last_transcript_enabled: bool,
    pub header_label: String,
    /// Profile rows for the "Profiles ►" submenu. Empty when the
    /// daemon has not yet returned a profile list (e.g. offline
    /// startup).
    pub profiles: Vec<ProfileMenuEntry>,
    /// Mirrors the "Profiles ►" submenu header `enabled` flag.
    /// `true` only when the daemon is fully idle and no command is
    /// in flight (`DoD` #20 + #21).
    pub profiles_submenu_enabled: bool,
}

/// Render the user-visible label for the "Hotkey: …" menu entry.
/// Pure function so the truth table per `DoD` #17 can be unit
/// tested without a ksni service.
#[must_use]
pub fn hotkey_menu_label(state: &HotkeyMenuState) -> String {
    match state {
        HotkeyMenuState::Unknown => "Hotkey: probing…".to_owned(),
        HotkeyMenuState::Unavailable { .. } => "Hotkey: unavailable".to_owned(),
        HotkeyMenuState::NotBound => "Hotkey: not bound — click to bind".to_owned(),
        HotkeyMenuState::Bound { display } => format!("Hotkey: {display}"),
    }
}

/// Whether the "Hotkey: …" entry is clickable. `Unavailable` and
/// `Unknown` are no-ops (the listener is the writer of the
/// state). `NotBound` and `Bound` both dispatch a `Bind` request
/// so the user can bind / re-bind from the same row.
#[must_use]
pub fn hotkey_menu_clickable(state: &HotkeyMenuState) -> bool {
    matches!(
        state,
        HotkeyMenuState::NotBound | HotkeyMenuState::Bound { .. }
    )
}

/// Returns `true` for any backend identifier that is not the local
/// `whisper-cpp` engine. Empty / unknown backends fall through as
/// `false` so a corrupted `Profiles1.list_v2` row never paints a
/// false ☁ marker. M5 `DoD` #8.
#[must_use]
pub fn is_cloud_backend(backend: &str) -> bool {
    !backend.is_empty() && backend != "whisper-cpp"
}

/// Compute [`MenuFlags`] for a given snapshot. See module docs for
/// the per-state enablement table.
#[must_use]
pub fn menu_flags_for(state: &TrayState) -> MenuFlags {
    // Optimistic action lock (DoD #21): once the user clicks Start
    // or Stop, both buttons are disabled until the daemon answers
    // with a matching `StateChanged`. The pump clears `pending_cmd`
    // there.
    let no_pending = state.pending_cmd.is_none();

    let start_enabled = no_pending && matches!(state.icon, IconState::Idle | IconState::Failed);
    let stop_enabled = no_pending && matches!(state.icon, IconState::Recording);

    let last = state.last_session.as_ref();
    let open_last_recording_enabled = last.is_some();
    let open_last_transcript_enabled = last.and_then(|l| l.transcript_path.as_ref()).is_some();

    let profile = if state.active_profile.is_empty() {
        "—"
    } else {
        state.active_profile.as_str()
    };
    let header_label = format!("● {} ({})", state_label_for(state.icon), profile);

    // DoD #20: switching profiles must be impossible while the
    // daemon is doing anything other than sitting idle. DoD #21:
    // also locked while the optimistic action lock is held.
    let profiles_submenu_enabled = matches!(state.icon, IconState::Idle) && no_pending;

    let profiles = state
        .profiles
        .iter()
        .map(|p| ProfileMenuEntry {
            name: p.name.clone(),
            active: p.name == state.active_profile,
            enabled: profiles_submenu_enabled,
            cloud: is_cloud_backend(&p.backend),
        })
        .collect();

    MenuFlags {
        start_enabled,
        stop_enabled,
        open_last_recording_enabled,
        open_last_transcript_enabled,
        header_label,
        profiles,
        profiles_submenu_enabled,
    }
}

/// The ksni-side renderer.
///
/// Construct one and call `tray.spawn().await` (from `TrayMethods`)
/// to register it on the session bus; the returned `ksni::Handle`
/// owns the lifetime of the service. See [`crate::supervisor`] for
/// the liveness watch.
pub struct ZwhisperTray {
    state: TrayState,
    cmd_tx: mpsc::Sender<PendingCmd>,
    quit_tx: mpsc::Sender<()>,
    /// M6: control surface for the hotkey listener task. When
    /// the bus is unreachable at startup the tray runs in
    /// degraded mode and this channel may have no consumer; the
    /// menu callbacks `try_send` regardless and rely on the
    /// bounded buffer + warn-log to keep the UX consistent
    /// across modes.
    hotkey_tx: mpsc::Sender<HotkeyControl>,
}

impl std::fmt::Debug for ZwhisperTray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZwhisperTray")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl ZwhisperTray {
    /// Construct the renderer with its outbound channels.
    ///
    /// `cmd_tx` carries `Start` / `Stop` / `SetActiveProfile` commands to
    /// the (P4) dispatcher; `quit_tx` carries the user's request to
    /// exit cleanly; `hotkey_tx` (M6) carries Bind / Unbind / Probe
    /// requests to the hotkey listener task.
    #[must_use]
    pub fn new(
        cmd_tx: mpsc::Sender<PendingCmd>,
        quit_tx: mpsc::Sender<()>,
        hotkey_tx: mpsc::Sender<HotkeyControl>,
    ) -> Self {
        Self {
            state: TrayState::default(),
            cmd_tx,
            quit_tx,
            hotkey_tx,
        }
    }

    /// Replace the current snapshot. Called from the supervisor task
    /// every time the watch channel publishes a new state.
    pub fn set_state(&mut self, new_state: TrayState) {
        self.state = new_state;
    }

    /// Read-only access to the current snapshot. Used by tests.
    #[must_use]
    pub fn state(&self) -> &TrayState {
        &self.state
    }
}

impl Tray for ZwhisperTray {
    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").into()
    }

    fn title(&self) -> String {
        "zwhisper".into()
    }

    fn icon_name(&self) -> String {
        icon_for_state(self.state.icon).to_string()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: tooltip_text(&self.state),
            ..Default::default()
        }
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // M4-plan § "Default left-click": open the most recent
        // recording. The dispatcher owns the actual `xdg-open` call;
        // here we only enqueue a `PendingCmd::OpenLastRecording`. If
        // there is nothing to open the dispatcher logs and drops it.
        if self.state.last_session.is_some() {
            if let Err(err) = self.cmd_tx.try_send(PendingCmd::OpenLastRecording) {
                warn!(error = %err, "tray activate: cmd channel full or closed");
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let flags = menu_flags_for(&self.state);

        // The callbacks need owned senders. Cloning is cheap and
        // keeps the closures `'static`.
        let start_tx = self.cmd_tx.clone();
        let stop_tx = self.cmd_tx.clone();
        let open_rec_tx = self.cmd_tx.clone();
        let open_tr_tx = self.cmd_tx.clone();
        let quit_tx = self.quit_tx.clone();

        // Profile submenu — one CheckmarkItem per profile, with the
        // active one checked. All entries are disabled together when
        // the daemon is not idle (DoD #20). When the profile list is
        // empty, the "Profiles ►" header is still shown so the user
        // gets visual feedback that the section exists.
        let profile_items: Vec<MenuItem<Self>> = flags
            .profiles
            .iter()
            .map(|p| {
                let name = p.name.clone();
                let cmd_tx = self.cmd_tx.clone();
                let label = if p.cloud {
                    format!("☁ {}", p.name)
                } else {
                    p.name.clone()
                };
                CheckmarkItem {
                    label,
                    enabled: p.enabled,
                    visible: true,
                    checked: p.active,
                    activate: Box::new(move |_this: &mut Self| {
                        let cmd = PendingCmd::SetActiveProfile { name: name.clone() };
                        if let Err(err) = cmd_tx.try_send(cmd) {
                            warn!(
                                error = %err,
                                "menu SetActiveProfile: cmd channel full or closed",
                            );
                        }
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();

        vec![
            // Header label — disabled, used as a "● state (profile)"
            // status line at the top of the menu.
            StandardItem {
                label: flags.header_label,
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Start recording".to_owned(),
                enabled: flags.start_enabled,
                activate: Box::new(move |_this: &mut Self| {
                    if let Err(err) = start_tx.try_send(PendingCmd::Start) {
                        warn!(error = %err, "menu Start: cmd channel full or closed");
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Stop recording".to_owned(),
                enabled: flags.stop_enabled,
                activate: Box::new(move |_this: &mut Self| {
                    if let Err(err) = stop_tx.try_send(PendingCmd::Stop) {
                        warn!(error = %err, "menu Stop: cmd channel full or closed");
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: "Profiles".to_owned(),
                enabled: flags.profiles_submenu_enabled,
                visible: true,
                submenu: profile_items,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            // M6: hotkey row. Always visible; click dispatches
            // `Bind` for `NotBound` / `Bound` and is a no-op for
            // `Unknown` / `Unavailable`. The label and `enabled`
            // flag are computed by pure helpers so the truth
            // table can be unit-tested.
            {
                let hotkey_state = self.state.hotkey.clone();
                let label = hotkey_menu_label(&hotkey_state);
                let enabled = hotkey_menu_clickable(&hotkey_state);
                let hotkey_tx = self.hotkey_tx.clone();
                StandardItem {
                    label,
                    enabled,
                    activate: Box::new(move |_this: &mut Self| {
                        if let Err(err) = hotkey_tx.try_send(HotkeyControl::Bind) {
                            warn!(
                                error = %err,
                                "menu Hotkey: ctl channel full or closed",
                            );
                        }
                    }),
                    ..Default::default()
                }
                .into()
            },
            MenuItem::Separator,
            StandardItem {
                label: "Open last recording".to_owned(),
                enabled: flags.open_last_recording_enabled,
                activate: Box::new(move |_this: &mut Self| {
                    if let Err(err) = open_rec_tx.try_send(PendingCmd::OpenLastRecording) {
                        warn!(
                            error = %err,
                            "menu Open-last-recording: cmd channel full or closed",
                        );
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open last transcript".to_owned(),
                enabled: flags.open_last_transcript_enabled,
                activate: Box::new(move |_this: &mut Self| {
                    if let Err(err) = open_tr_tx.try_send(PendingCmd::OpenLastTranscript) {
                        warn!(
                            error = %err,
                            "menu Open-last-transcript: cmd channel full or closed",
                        );
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Settings\u{2026}".to_owned(),
                enabled: true,
                activate: Box::new(move |_this: &mut Self| {
                    spawn_settings_binary();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".to_owned(),
                enabled: true,
                activate: Box::new(move |_this: &mut Self| {
                    if let Err(err) = quit_tx.try_send(()) {
                        warn!(error = %err, "menu Quit: quit channel full or closed");
                    }
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// M7: launch the on-demand `zwhisper-settings` binary. The
/// binary itself enforces single-instance via the
/// `cz.zajca.Zwhisper1.Settings` D-Bus name claim, so spawning
/// while one is already running is harmless — the second
/// instance exits 0 after raising the existing window.
///
/// We resolve via `$PATH` (`zwhisper-settings`) rather than an
/// absolute path so a sibling-built binary in `target/<profile>/`
/// works in dev without env tweaks: M8 packaging installs both
/// binaries into `/usr/bin`, so the lookup succeeds in production.
fn spawn_settings_binary() {
    use std::process::Command;

    let exe = settings_binary_path();
    match Command::new(&exe).spawn() {
        Ok(_child) => {
            tracing::info!(binary = %exe, "tray: launched zwhisper-settings");
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                binary = %exe,
                "tray: failed to launch zwhisper-settings — install it or build the workspace",
            );
        }
    }
}

/// Resolve the settings binary path. In dev the workspace target
/// directory is preferred so a `cargo build` produces a working
/// menu entry without a system install. In production the binary
/// name on `$PATH` is used.
fn settings_binary_path() -> String {
    // Dev convenience: if the tray itself was launched from
    // `target/<profile>/zwhisper-tray`, prefer the sibling
    // `zwhisper-settings` in the same directory.
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(parent) = self_exe.parent() {
            let candidate = parent.join("zwhisper-settings");
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "zwhisper-settings".to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::state::{HotkeyMenuState, LastCompleted};
    use std::path::PathBuf;

    // M6 `DoD` #17 truth table for the four hotkey menu states.
    #[test]
    fn hotkey_menu_label_unknown_says_probing() {
        assert_eq!(
            hotkey_menu_label(&HotkeyMenuState::Unknown),
            "Hotkey: probing…"
        );
        assert!(!hotkey_menu_clickable(&HotkeyMenuState::Unknown));
    }

    #[test]
    fn hotkey_menu_label_unavailable_renders_short_label() {
        let label = hotkey_menu_label(&HotkeyMenuState::Unavailable {
            reason: "no portal".to_owned(),
        });
        assert_eq!(label, "Hotkey: unavailable");
        assert!(!hotkey_menu_clickable(&HotkeyMenuState::Unavailable {
            reason: "no portal".to_owned(),
        }));
    }

    #[test]
    fn hotkey_menu_label_not_bound_invites_click() {
        assert_eq!(
            hotkey_menu_label(&HotkeyMenuState::NotBound),
            "Hotkey: not bound — click to bind"
        );
        assert!(hotkey_menu_clickable(&HotkeyMenuState::NotBound));
    }

    #[test]
    fn hotkey_menu_label_bound_shows_chord() {
        let bound = HotkeyMenuState::Bound {
            display: "Ctrl+Alt+R".to_owned(),
        };
        assert_eq!(hotkey_menu_label(&bound), "Hotkey: Ctrl+Alt+R");
        assert!(hotkey_menu_clickable(&bound));
    }

    fn empty_state(icon: IconState) -> TrayState {
        TrayState {
            icon,
            active_profile: "default".to_owned(),
            ..TrayState::default()
        }
    }

    fn audio_only_session() -> LastCompleted {
        LastCompleted {
            session_id: "sid".to_owned(),
            audio_path: PathBuf::from("/tmp/a.flac"),
            transcript_path: None,
            backend: None,
            completed_at_unix_ms: 0,
        }
    }

    fn full_session() -> LastCompleted {
        LastCompleted {
            session_id: "sid".to_owned(),
            audio_path: PathBuf::from("/tmp/a.flac"),
            transcript_path: Some(PathBuf::from("/tmp/a.flac.txt")),
            backend: Some("whisper-cli".to_owned()),
            completed_at_unix_ms: 0,
        }
    }

    #[test]
    fn menu_flags_idle_enables_start_only() {
        let flags = menu_flags_for(&empty_state(IconState::Idle));
        assert!(flags.start_enabled, "idle should enable Start");
        assert!(!flags.stop_enabled, "idle should not enable Stop");
    }

    #[test]
    fn menu_flags_recording_enables_stop_only() {
        let flags = menu_flags_for(&empty_state(IconState::Recording));
        assert!(!flags.start_enabled, "recording should not enable Start");
        assert!(flags.stop_enabled, "recording should enable Stop");
    }

    #[test]
    fn menu_flags_starting_enables_neither() {
        let flags = menu_flags_for(&empty_state(IconState::Starting));
        assert!(!flags.start_enabled);
        assert!(!flags.stop_enabled);
    }

    #[test]
    fn menu_flags_offline_enables_neither() {
        let flags = menu_flags_for(&empty_state(IconState::DaemonOffline));
        assert!(!flags.start_enabled);
        assert!(!flags.stop_enabled);
    }

    #[test]
    fn menu_flags_failed_enables_start() {
        // Failed is a recoverable terminal state — Start should be
        // possible to retry the recording.
        let flags = menu_flags_for(&empty_state(IconState::Failed));
        assert!(flags.start_enabled, "failed should re-enable Start");
        assert!(!flags.stop_enabled);
    }

    #[test]
    fn menu_flags_open_last_disabled_when_no_session() {
        let flags = menu_flags_for(&empty_state(IconState::Idle));
        assert!(!flags.open_last_recording_enabled);
        assert!(!flags.open_last_transcript_enabled);
    }

    #[test]
    fn menu_flags_open_last_enabled_when_audio_only() {
        let mut s = empty_state(IconState::Idle);
        s.last_session = Some(audio_only_session());
        let flags = menu_flags_for(&s);
        assert!(flags.open_last_recording_enabled);
    }

    #[test]
    fn menu_flags_open_last_transcript_disabled_when_audio_only() {
        let mut s = empty_state(IconState::Idle);
        s.last_session = Some(audio_only_session());
        let flags = menu_flags_for(&s);
        assert!(!flags.open_last_transcript_enabled);
    }

    #[test]
    fn menu_flags_open_last_transcript_enabled_when_full() {
        let mut s = empty_state(IconState::Idle);
        s.last_session = Some(full_session());
        let flags = menu_flags_for(&s);
        assert!(flags.open_last_recording_enabled);
        assert!(flags.open_last_transcript_enabled);
    }

    #[test]
    fn menu_flags_pending_cmd_disables_actions() {
        // DoD #21: optimistic action lock. While a command is in
        // flight neither Start nor Stop can be activated, regardless
        // of what the icon currently shows.
        let mut s = empty_state(IconState::Idle);
        s.pending_cmd = Some(PendingCmd::Start);
        let flags = menu_flags_for(&s);
        assert!(!flags.start_enabled, "pending Start must lock Start");
        assert!(!flags.stop_enabled, "pending Start must lock Stop");

        let mut s = empty_state(IconState::Recording);
        s.pending_cmd = Some(PendingCmd::Stop);
        let flags = menu_flags_for(&s);
        assert!(!flags.start_enabled, "pending Stop must lock Start");
        assert!(!flags.stop_enabled, "pending Stop must lock Stop");
    }

    #[test]
    fn menu_flags_header_label_includes_state_and_profile() {
        let s = empty_state(IconState::Recording);
        let flags = menu_flags_for(&s);
        assert!(
            flags.header_label.contains("recording"),
            "got {}",
            flags.header_label,
        );
        assert!(
            flags.header_label.contains("default"),
            "got {}",
            flags.header_label,
        );
    }

    #[test]
    fn menu_flags_header_label_dashes_empty_profile() {
        let s = TrayState {
            icon: IconState::Idle,
            active_profile: String::new(),
            ..TrayState::default()
        };
        let flags = menu_flags_for(&s);
        assert!(
            flags.header_label.contains("(—)"),
            "got {}",
            flags.header_label
        );
    }

    fn profile(name: &str) -> zwhisper_ipc::ProfileEntryV2 {
        profile_with_backend(name, "whisper-cpp")
    }

    fn profile_with_backend(name: &str, backend: &str) -> zwhisper_ipc::ProfileEntryV2 {
        zwhisper_ipc::ProfileEntryV2 {
            name: name.to_owned(),
            description: String::new(),
            schema_version: 1,
            backend: backend.to_owned(),
        }
    }

    #[test]
    fn is_cloud_backend_truth_table() {
        assert!(is_cloud_backend("deepgram"));
        assert!(is_cloud_backend("assemblyai"));
        assert!(!is_cloud_backend("whisper-cpp"));
        assert!(!is_cloud_backend(""));
    }

    #[test]
    fn cloud_marker_set_for_remote_backend_only() {
        let mut s = empty_state(IconState::Idle);
        s.profiles = vec![
            profile_with_backend("meeting", "whisper-cpp"),
            profile_with_backend("cloud-meeting", "deepgram"),
        ];
        s.active_profile = "meeting".to_owned();
        let flags = menu_flags_for(&s);
        assert_eq!(flags.profiles.len(), 2);
        let meeting = &flags.profiles[0];
        let cloud = &flags.profiles[1];
        assert!(
            !meeting.cloud,
            "whisper-cpp profile must not be marked cloud"
        );
        assert!(cloud.cloud, "deepgram profile must be marked cloud");
    }

    #[test]
    fn cloud_marker_clears_when_backend_switches_to_local() {
        let mut s = empty_state(IconState::Idle);
        s.profiles = vec![profile_with_backend("p", "deepgram")];
        let flags1 = menu_flags_for(&s);
        assert!(flags1.profiles[0].cloud);

        s.profiles = vec![profile_with_backend("p", "whisper-cpp")];
        let flags2 = menu_flags_for(&s);
        assert!(!flags2.profiles[0].cloud);
    }

    #[test]
    fn menu_flags_profile_submenu_disabled_when_recording() {
        let mut s = empty_state(IconState::Recording);
        s.profiles = vec![profile("default"), profile("alt")];
        let flags = menu_flags_for(&s);
        assert!(
            !flags.profiles_submenu_enabled,
            "profile submenu must be locked while recording (DoD #20)",
        );
        assert!(
            flags.profiles.iter().all(|p| !p.enabled),
            "every profile entry must be disabled when the submenu is locked",
        );
    }

    #[test]
    fn menu_flags_profile_submenu_enabled_when_idle() {
        let mut s = empty_state(IconState::Idle);
        s.profiles = vec![profile("default"), profile("alt")];
        let flags = menu_flags_for(&s);
        assert!(flags.profiles_submenu_enabled);
        assert!(flags.profiles.iter().all(|p| p.enabled));
    }

    #[test]
    fn menu_flags_profile_radio_active_only_for_match() {
        let mut s = empty_state(IconState::Idle);
        s.active_profile = "alt".to_owned();
        s.profiles = vec![profile("default"), profile("alt"), profile("third")];
        let flags = menu_flags_for(&s);

        let active: Vec<_> = flags.profiles.iter().filter(|p| p.active).collect();
        assert_eq!(
            active.len(),
            1,
            "exactly one profile entry should carry the radio mark",
        );
        assert_eq!(active[0].name, "alt");
    }

    #[test]
    fn menu_flags_profile_submenu_disabled_when_pending_cmd() {
        // DoD #21: any in-flight command also locks the profile
        // submenu — even if the icon is `Idle`, the action lock has
        // priority.
        let mut s = empty_state(IconState::Idle);
        s.profiles = vec![profile("default")];
        s.pending_cmd = Some(PendingCmd::Start);
        let flags = menu_flags_for(&s);
        assert!(!flags.profiles_submenu_enabled);
        assert!(flags.profiles.iter().all(|p| !p.enabled));
    }
}
