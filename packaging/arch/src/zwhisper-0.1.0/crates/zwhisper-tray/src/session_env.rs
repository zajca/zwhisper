//! Startup graphical-session probe.
//!
//! M4-plan stress-test fix M4: the systemd unit uses
//! `After=graphical-session.target` (not `Requisite=`) so the unit
//! starts even on wlroots setups that never activate the target. To
//! keep the no-graphical-session case from running a useless tray
//! process, the binary checks `WAYLAND_DISPLAY` / `DISPLAY` at
//! startup and exits cleanly when neither is set.
//!
//! This is a pure-function module so the decision logic is
//! testable without env mutation.

#[derive(Debug, PartialEq, Eq)]
pub enum SessionProbe {
    /// At least one of `WAYLAND_DISPLAY` / `DISPLAY` is set; tray
    /// proceeds.
    Available,
    /// Neither is set; tray exits with a clear error message.
    Unavailable,
}

/// Pure decision: returns `Available` iff one of the two env values
/// is `Some(non-empty)`.
pub fn classify(wayland_display: Option<&str>, x_display: Option<&str>) -> SessionProbe {
    let has_wayland = wayland_display.is_some_and(|s| !s.is_empty());
    let has_x = x_display.is_some_and(|s| !s.is_empty());
    if has_wayland || has_x {
        SessionProbe::Available
    } else {
        SessionProbe::Unavailable
    }
}

/// Read the env vars and classify. Convenience wrapper used by
/// `main.rs`.
pub fn probe() -> SessionProbe {
    let wayland = std::env::var("WAYLAND_DISPLAY").ok();
    let x = std::env::var("DISPLAY").ok();
    classify(wayland.as_deref(), x.as_deref())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn classify_wayland_only_is_available() {
        assert_eq!(classify(Some("wayland-0"), None), SessionProbe::Available);
    }

    #[test]
    fn classify_x_only_is_available() {
        assert_eq!(classify(None, Some(":0")), SessionProbe::Available);
    }

    #[test]
    fn classify_both_set_is_available() {
        assert_eq!(
            classify(Some("wayland-0"), Some(":0")),
            SessionProbe::Available
        );
    }

    #[test]
    fn classify_none_is_unavailable() {
        assert_eq!(classify(None, None), SessionProbe::Unavailable);
    }

    #[test]
    fn classify_empty_string_treated_as_missing() {
        // systemd may pass through `WAYLAND_DISPLAY=` with no value;
        // treat that the same as unset.
        assert_eq!(classify(Some(""), Some("")), SessionProbe::Unavailable);
        assert_eq!(classify(Some(""), None), SessionProbe::Unavailable);
    }
}
