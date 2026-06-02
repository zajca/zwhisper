//! Diagnostic mapping for `ksni::Error` returned from `Tray::spawn`.
//!
//! ksni's three error variants collapse two fundamentally different
//! environment problems (no D-Bus, no SNI watcher, no SNI host) into
//! the same `failed to register …` log line. Users on Wayland
//! compositors such as Sway or Hyprland — which often rely on an
//! external StatusNotifierWatcher — see the `ServiceUnknown: The name
//! is not activatable` D-Bus error and have no idea what to install.
//!
//! This module turns each variant into an actionable diagnostic
//! (one-line summary plus a bullet list of next steps) so the binary
//! can log a useful message before exiting. Pure-function design
//! mirrors `session_env` so the mapping is unit-testable without a
//! live D-Bus session.
//!
//! See `docs/M8-plan.md` follow-up notes; the mapping is intentionally
//! conservative — when a new ksni variant lands (`#[non_exhaustive]`),
//! the wildcard arm degrades to a generic message instead of a
//! compile-time break.

use ksni::Error;

/// Coarse category of the underlying environment failure.
///
/// The category drives whether the operator should fix D-Bus
/// itself, install a tray plugin, or wait for the desktop to
/// finish initializing. The variants correspond 1:1 to the three
/// `ksni::Error` cases as of ksni 0.3.4 plus an `Unknown` arm that
/// absorbs future additions.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Category {
    /// Session bus itself is unreachable. Almost always a sandbox
    /// or systemd-user misconfiguration; nothing to do with the
    /// tray spec.
    Dbus,
    /// Session bus works, but no `org.kde.StatusNotifierWatcher`
    /// is activatable. Typical on Sway, Hyprland, and bare wlroots
    /// without a panel.
    NoWatcher,
    /// Watcher exists but reports zero registered hosts (panels).
    /// Rare; usually a startup race against the panel.
    NoHost,
    /// Future ksni variant. Logged verbatim with generic guidance.
    Unknown,
}

/// Structured diagnostic produced from a `ksni::Error`.
///
/// `summary` is intended for the top-level `error!` log line and
/// the user-facing `eyre` chain; `next_steps` is a curated bullet
/// list that gets logged at `error!` level (one line per step) so
/// it shows up in journalctl output without extra fan-out.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Diagnostic {
    pub category: Category,
    pub summary: &'static str,
    pub next_steps: &'static [&'static str],
}

/// Map a `ksni::Error` to a `Diagnostic`.
///
/// The function takes `&Error` so callers can keep the original
/// error around for the `eyre` source chain. Every arm returns a
/// `&'static` payload — diagnostics never allocate, which keeps
/// the failure path predictable on a stressed system.
pub fn diagnose(err: &Error) -> Diagnostic {
    match err {
        Error::Dbus(_) => Diagnostic {
            category: Category::Dbus,
            summary: "cannot reach the session D-Bus daemon",
            next_steps: &[
                "verify $DBUS_SESSION_BUS_ADDRESS is set (run `systemctl --user status dbus`)",
                "if running under sudo or in a sandbox, re-run inside your user session",
                "for flatpak/snap setups, allow access to the session bus in the manifest",
            ],
        },
        Error::Watcher(_) => Diagnostic {
            category: Category::NoWatcher,
            summary: "no StatusNotifierWatcher is registered on the session bus \
                      (your desktop has no system tray host)",
            next_steps: &[
                "Sway: install `waybar` and add `\"tray\"` to its modules-right",
                "Hyprland: run Waybar with the tray module, or another StatusNotifierItem host",
                "GNOME: install the AppIndicator extension \
                 (gnome-shell-extension-appindicator)",
                "verify with: `busctl --user list | grep StatusNotifierWatcher`",
                "the daemon and `zwhisper` CLI keep working without a tray icon",
            ],
        },
        Error::WontShow => Diagnostic {
            category: Category::NoHost,
            summary: "tray registered but no StatusNotifierHost (panel) is listening — \
                      icon will not be shown",
            next_steps: &[
                "this usually means the tray started before the panel finished initializing",
                "wait for the desktop session to settle and restart `zwhisper-tray`",
                "if it persists, the panel does not implement StatusNotifierHost \
                 — install one of the trays listed under the no-watcher case",
            ],
        },
        // ksni::Error is `#[non_exhaustive]` — keep the build green
        // when upstream adds a new variant. The original error keeps
        // its Display impl in the eyre chain for raw context.
        _ => Diagnostic {
            category: Category::Unknown,
            summary: "tray registration failed for an unrecognized reason",
            next_steps: &["report the error verbatim at https://github.com/zajca/zwhisper/issues"],
        },
    }
}

/// Whether registration should be retried instead of treated as a
/// fatal startup error.
///
/// Missing watcher/host is common on tiling WM startup because the
/// tray host is an external process. D-Bus failures and unknown
/// upstream errors stay fatal so the process does not spin on
/// misconfiguration that cannot be fixed by starting a panel later.
#[must_use]
pub fn registration_failure_is_retryable(category: Category) -> bool {
    matches!(category, Category::NoWatcher | Category::NoHost)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    // The non_exhaustive Error type is awkward to construct by
    // hand, but `Error::WontShow` is a unit variant we can build
    // directly. The other two arms wrap zbus errors; building a
    // synthetic zbus error means going through its public API.
    //
    // For unit testing the category mapping we only need the Watcher
    // and Dbus arms via concrete zbus errors, but constructing those
    // requires an actual bus connection. We therefore exercise:
    //   - WontShow directly,
    //   - Dbus / Watcher via runtime-built fakes using `zbus::Error`'s
    //     public From impls,
    //   - the wildcard arm via the documented #[non_exhaustive]
    //     contract — covered by the type system, not a runtime test.

    #[test]
    fn wont_show_maps_to_no_host_category() {
        let diag = diagnose(&Error::WontShow);
        assert_eq!(diag.category, Category::NoHost);
        assert!(!diag.next_steps.is_empty());
        assert!(diag.summary.contains("no StatusNotifierHost"));
    }

    #[test]
    fn dbus_arm_maps_to_dbus_category() {
        // zbus::Error has a public `Address` variant we can build
        // without a live bus.
        let zbus_err = zbus::Error::Address("synthetic".to_string());
        let diag = diagnose(&Error::Dbus(zbus_err));
        assert_eq!(diag.category, Category::Dbus);
        assert!(diag.summary.contains("session D-Bus daemon"));
        assert!(
            diag.next_steps
                .iter()
                .any(|s| s.contains("DBUS_SESSION_BUS_ADDRESS"))
        );
    }

    #[test]
    fn watcher_arm_maps_to_no_watcher_category() {
        // zbus::fdo::Error::ServiceUnknown matches the real-world
        // failure on Sway/Hyprland (`name is not activatable`).
        let fdo_err = zbus::fdo::Error::ServiceUnknown("synthetic".to_string());
        let diag = diagnose(&Error::Watcher(fdo_err));
        assert_eq!(diag.category, Category::NoWatcher);
        assert!(diag.summary.contains("StatusNotifierWatcher"));
        assert!(diag.next_steps.iter().any(|s| s.contains("waybar")));
        assert!(diag.next_steps.iter().any(|s| s.contains("Hyprland")));
        assert!(
            !diag
                .next_steps
                .iter()
                .any(|s| s.contains("snixembed") || s.contains("XEmbed"))
        );
        assert!(
            diag.next_steps
                .iter()
                .any(|s| s.contains("CLI keep working"))
        );
    }

    #[test]
    fn next_steps_are_non_empty_for_known_categories() {
        for diag in [
            diagnose(&Error::WontShow),
            diagnose(&Error::Dbus(zbus::Error::Address("x".into()))),
            diagnose(&Error::Watcher(zbus::fdo::Error::ServiceUnknown(
                "x".into(),
            ))),
        ] {
            assert!(
                !diag.next_steps.is_empty(),
                "missing next_steps for {:?}",
                diag.category
            );
            for step in diag.next_steps {
                assert!(!step.is_empty(), "empty step in {:?}", diag.category);
            }
        }
    }

    #[test]
    fn only_missing_watcher_or_host_is_retryable() {
        assert!(registration_failure_is_retryable(Category::NoWatcher));
        assert!(registration_failure_is_retryable(Category::NoHost));
        assert!(!registration_failure_is_retryable(Category::Dbus));
        assert!(!registration_failure_is_retryable(Category::Unknown));
    }
}
