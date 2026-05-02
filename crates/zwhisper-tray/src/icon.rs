//! Tray icon mapping and tooltip rendering.
//!
//! The four [`TrayIconKind`] variants are mapped to freedesktop icon
//! names (`zwhisper-{idle,recording,busy,error}`) which the system
//! theme resolves at runtime. The actual SVG files ship in
//! `data/icons/` and will be installed to
//! `/usr/share/icons/hicolor/scalable/apps/` by M8 packaging.
//!
//! Developer workflow: symlink the SVGs into
//! `~/.local/share/icons/hicolor/scalable/apps/` and bump the icon
//! cache so KDE's tray picks them up. Icons are NOT loaded as raw
//! pixmaps in P3 — we rely on `icon_name` and the system theme.
//!
//! See M4-plan § "State machine: `RecorderState` → icon + menu state"
//! and § "Definition of done" item 4 (tooltip format).

use std::fmt;
use std::fmt::Write as _;
use std::time::Instant;

use crate::state::{IconState, TrayState};

/// The four user-visible icon shapes the tray can render.
///
/// `Idle` and `Recording` are unique states; `Busy` covers
/// `Starting`/`Stopping` (transient transitions); `Error` covers both
/// `Failed` (daemon-reported error) and `DaemonOffline` (no bus
/// owner) — visually they're indistinguishable from the user's
/// perspective: "something is wrong".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIconKind {
    Idle,
    Recording,
    Busy,
    Error,
}

impl fmt::Display for TrayIconKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Idle => "zwhisper-idle",
            Self::Recording => "zwhisper-recording",
            Self::Busy => "zwhisper-busy",
            Self::Error => "zwhisper-error",
        };
        f.write_str(name)
    }
}

/// Map an [`IconState`] to its visual [`TrayIconKind`] per M4-plan.
#[must_use]
pub fn icon_for_state(state: IconState) -> TrayIconKind {
    match state {
        IconState::Idle => TrayIconKind::Idle,
        IconState::Recording => TrayIconKind::Recording,
        IconState::Starting | IconState::Stopping => TrayIconKind::Busy,
        IconState::Failed | IconState::DaemonOffline => TrayIconKind::Error,
    }
}

/// Render the tooltip text for the current snapshot.
///
/// Format: `"zwhisper — {state} · profile: {active_profile}"`.
/// When the icon is `Recording` AND `recording_started_at` is set,
/// `" · MM:SS"` is appended (clamped to 99:59 to avoid overflow in
/// the formatted string).
#[must_use]
pub fn tooltip_text(state: &TrayState) -> String {
    let state_label = state_label_for(state.icon);
    let profile = if state.active_profile.is_empty() {
        "—"
    } else {
        state.active_profile.as_str()
    };
    let mut out = format!("zwhisper — {state_label} · profile: {profile}");

    if matches!(state.icon, IconState::Recording) {
        if let Some(started_at) = state.recording_started_at {
            let secs = duration_secs_since(started_at);
            let (mm, ss) = mm_ss_clamped(secs);
            // Writes into a `String` are infallible; the `let _` is
            // there to silence `unused_must_use` without `unwrap`.
            let _ = write!(out, " · {mm:02}:{ss:02}");
        }
    }

    out
}

/// Stable user-visible label for each [`IconState`]. Used by the
/// tooltip and by the menu header label.
#[must_use]
pub fn state_label_for(state: IconState) -> &'static str {
    match state {
        IconState::Idle => "idle",
        IconState::Starting => "starting",
        IconState::Recording => "recording",
        IconState::Stopping => "stopping",
        IconState::Failed => "failed",
        IconState::DaemonOffline => "daemon offline",
    }
}

fn duration_secs_since(started_at: Instant) -> u64 {
    started_at.elapsed().as_secs()
}

/// Split seconds into minutes and seconds. Anything beyond 99:59 is
/// clamped — the tooltip is for at-a-glance feedback, not a stopwatch.
fn mm_ss_clamped(total_secs: u64) -> (u64, u64) {
    let max = 99 * 60 + 59;
    let clamped = total_secs.min(max);
    (clamped / 60, clamped % 60)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn icon_for_state_idle_returns_idle() {
        assert_eq!(icon_for_state(IconState::Idle), TrayIconKind::Idle);
    }

    #[test]
    fn icon_for_state_starting_and_stopping_both_return_busy() {
        assert_eq!(icon_for_state(IconState::Starting), TrayIconKind::Busy);
        assert_eq!(icon_for_state(IconState::Stopping), TrayIconKind::Busy);
    }

    #[test]
    fn icon_for_state_recording_returns_recording() {
        assert_eq!(
            icon_for_state(IconState::Recording),
            TrayIconKind::Recording
        );
    }

    #[test]
    fn icon_for_state_failed_and_offline_return_error() {
        assert_eq!(icon_for_state(IconState::Failed), TrayIconKind::Error);
        assert_eq!(
            icon_for_state(IconState::DaemonOffline),
            TrayIconKind::Error
        );
    }

    #[test]
    fn tray_icon_kind_display_uses_freedesktop_names() {
        assert_eq!(TrayIconKind::Idle.to_string(), "zwhisper-idle");
        assert_eq!(TrayIconKind::Recording.to_string(), "zwhisper-recording");
        assert_eq!(TrayIconKind::Busy.to_string(), "zwhisper-busy");
        assert_eq!(TrayIconKind::Error.to_string(), "zwhisper-error");
    }

    #[test]
    fn tooltip_idle_omits_duration() {
        let s = TrayState {
            icon: IconState::Idle,
            active_profile: "default".to_owned(),
            ..TrayState::default()
        };
        let t = tooltip_text(&s);
        assert!(t.starts_with("zwhisper — idle"), "got {t}");
        assert!(t.contains("profile: default"), "got {t}");
        // The format always has exactly one " · " separator (between
        // state and profile). A duration suffix would add a second.
        let separator_count = t.matches(" · ").count();
        assert_eq!(separator_count, 1, "expected one separator, got {t}");
    }

    #[test]
    fn tooltip_recording_includes_mm_ss() {
        // We cannot inject a fake clock without overengineering, so
        // we exercise the path with `Instant::now()` minus a known
        // delta. `Instant::checked_sub` may return None on rare
        // systems where the instant is at zero — tolerate that and
        // still cover the format if subtraction succeeded.
        let started = Instant::now()
            .checked_sub(Duration::from_secs(125))
            .unwrap_or_else(Instant::now);
        let s = TrayState {
            icon: IconState::Recording,
            active_profile: "default".to_owned(),
            recording_started_at: Some(started),
            ..TrayState::default()
        };
        let t = tooltip_text(&s);
        assert!(t.contains("recording"), "got {t}");
        // The duration suffix introduces a second " · " separator
        // between profile and MM:SS. That's the witness we look for.
        let separator_count = t.matches(" · ").count();
        assert_eq!(separator_count, 2, "expected duration suffix, got {t}");
    }

    #[test]
    fn tooltip_recording_without_started_at_omits_duration() {
        let s = TrayState {
            icon: IconState::Recording,
            active_profile: "default".to_owned(),
            recording_started_at: None,
            ..TrayState::default()
        };
        let t = tooltip_text(&s);
        assert!(t.starts_with("zwhisper — recording"), "got {t}");
        // Same separator-count witness as the idle test: exactly one
        // " · " when the duration suffix is absent.
        let separator_count = t.matches(" · ").count();
        assert_eq!(separator_count, 1, "duration leaked: {t}");
    }

    #[test]
    fn tooltip_offline_uses_daemon_offline_label() {
        let s = TrayState::default();
        let t = tooltip_text(&s);
        assert!(t.contains("daemon offline"), "got {t}");
    }

    #[test]
    fn tooltip_empty_profile_renders_dash() {
        let s = TrayState {
            icon: IconState::Idle,
            active_profile: String::new(),
            ..TrayState::default()
        };
        let t = tooltip_text(&s);
        assert!(t.contains("profile: —"), "got {t}");
    }

    #[test]
    fn mm_ss_clamped_normal_values() {
        assert_eq!(mm_ss_clamped(0), (0, 0));
        assert_eq!(mm_ss_clamped(59), (0, 59));
        assert_eq!(mm_ss_clamped(60), (1, 0));
        assert_eq!(mm_ss_clamped(125), (2, 5));
    }

    #[test]
    fn mm_ss_clamped_caps_at_99_59() {
        assert_eq!(mm_ss_clamped(99 * 60 + 59), (99, 59));
        assert_eq!(mm_ss_clamped(u64::MAX), (99, 59));
    }
}
