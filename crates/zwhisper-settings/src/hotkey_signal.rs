//! M7 — D-Bus signal `cz.zajca.Zwhisper1.Settings.HotkeyRebound`
//! emitted by the settings binary on a successful rebind.
//!
//! Tray subscribes via `crates/zwhisper-tray/src/hotkey.rs` (Group
//! D's `D5` task). Rationale + decision recorded at `docs/M7-plan.md`
//! § "D7 — Hotkey rebind notification via dedicated D-Bus signal".
//!
//! We pick a *dedicated* signal rather than relying on the portal's
//! own `ShortcutsChanged` because the portal signal is not delivered
//! on every backend (M6 risk R3): GNOME's portal-frontend variant
//! historically swallows the broadcast in some configurations. The
//! dedicated signal is a known-good fallback; both can coexist.
//!
//! ### Wire shape (frozen)
//!
//! ```text
//! interface : cz.zajca.Zwhisper1.Settings
//! path      : /cz/zajca/Zwhisper1/Settings
//! member    : HotkeyRebound
//! payload   : (s) — the human-readable trigger description
//!             returned by the portal `bind` call (e.g.
//!             "Ctrl+Alt+R"). Empty string is permitted but
//!             discouraged.
//! ```

use tracing::{debug, warn};

use crate::error::SettingsError;

/// Interface name owned by the settings binary. Mirrors the format
/// used elsewhere in the workspace (`cz.zajca.Zwhisper1.<Iface>`).
pub(crate) const INTERFACE_NAME: &str = "cz.zajca.Zwhisper1.Settings";

/// Object path the signal is emitted from. Settings does not
/// register a server-side interface at this path (no methods are
/// needed) — the tray subscriber matches by `path` regardless.
pub(crate) const SIGNAL_PATH: &str = "/cz/zajca/Zwhisper1/Settings";

/// Member name of the broadcast signal. Stable — changing it
/// invalidates the tray's subscription and silently drops every
/// future rebind notification.
pub(crate) const SIGNAL_NAME: &str = "HotkeyRebound";

/// Owns the D-Bus connection used to broadcast `HotkeyRebound`
/// signals. The connection is the *session* bus; the tray and the
/// daemon both live on the same bus, so any subscriber can listen
/// without extra setup.
///
/// Intentionally separate from the `cz.zajca.Zwhisper1.Settings`
/// well-known name claim done by `app::try_acquire_single_instance`:
/// the emitter does not need to *own* the name to broadcast a
/// signal — anyone on the bus can emit any signal. Keeping the
/// emitter independent means a single-instance collision does not
/// silently break the rebind notification path.
pub(crate) struct HotkeyReboundEmitter {
    connection: zbus::Connection,
}

impl std::fmt::Debug for HotkeyReboundEmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotkeyReboundEmitter")
            .field("interface", &INTERFACE_NAME)
            .field("path", &SIGNAL_PATH)
            .field("member", &SIGNAL_NAME)
            .finish_non_exhaustive()
    }
}

impl HotkeyReboundEmitter {
    /// Connect to the session bus. Failure surfaces as
    /// [`SettingsError::Hotkey`] so callers can decide whether to
    /// degrade gracefully — a missing bus does not invalidate the
    /// rebind itself, only the cross-process notification.
    pub(crate) async fn new() -> Result<Self, SettingsError> {
        let connection = zbus::Connection::session().await.map_err(|e| {
            SettingsError::Hotkey(format!("hotkey-rebound emitter: session bus: {e}"))
        })?;
        debug!(
            interface = INTERFACE_NAME,
            path = SIGNAL_PATH,
            member = SIGNAL_NAME,
            "hotkey-rebound emitter connected to session bus"
        );
        Ok(Self { connection })
    }

    /// Broadcast a `HotkeyRebound` signal. The payload is the
    /// portal-supplied trigger description (e.g. `"Ctrl+Alt+R"`).
    /// Failure surfaces as [`SettingsError::Hotkey`]; the caller
    /// is expected to log and continue — the rebind itself has
    /// already succeeded by the time we hit this path.
    pub(crate) async fn emit(&self, description: &str) -> Result<(), SettingsError> {
        match self
            .connection
            .emit_signal(
                None::<&str>,
                SIGNAL_PATH,
                INTERFACE_NAME,
                SIGNAL_NAME,
                &(description,),
            )
            .await
        {
            Ok(()) => {
                debug!(description, "hotkey-rebound signal emitted");
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "hotkey-rebound signal emit failed");
                Err(SettingsError::Hotkey(format!("hotkey-rebound emit: {e}")))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// `DoD` #16 (settings half) — the emitter constructs
    /// successfully when the session bus is reachable. We do NOT
    /// require a session bus inside CI sandboxes; if none is
    /// available the test is skipped with a warning that mirrors
    /// the pattern used in `app::tests::second_launch_raises_existing_window`.
    ///
    /// The full subscriber-receives-signal contract is exercised
    /// end-to-end in
    /// `crates/zwhisper-tray/src/hotkey.rs::tests::tray_picks_up_settings_rebind_signal`,
    /// which spins up a dedicated bus and asserts the round trip.
    #[tokio::test]
    async fn emit_succeeds_on_session_bus() {
        let emitter = match HotkeyReboundEmitter::new().await {
            Ok(e) => e,
            Err(err) => {
                eprintln!("skipping: no session bus available ({err})");
                return;
            }
        };
        // Emit twice to confirm the connection is reusable. We
        // cannot self-subscribe inside this process without setting
        // up a separate connection, but the round-trip success
        // result already proves the wire-level send.
        emitter.emit("Ctrl+Alt+R").await.expect("first emit");
        emitter.emit("").await.expect("empty payload accepted");
    }

    #[test]
    fn constants_match_plan_wire_shape() {
        // The signal name + path + interface are part of the
        // M7 wire surface (`docs/M7-plan.md` § "Wire-surface
        // contract") — pin them so a renaming PR fails CI.
        assert_eq!(INTERFACE_NAME, "cz.zajca.Zwhisper1.Settings");
        assert_eq!(SIGNAL_PATH, "/cz/zajca/Zwhisper1/Settings");
        assert_eq!(SIGNAL_NAME, "HotkeyRebound");
    }
}
