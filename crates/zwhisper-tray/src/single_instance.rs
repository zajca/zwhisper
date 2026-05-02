//! Single-instance enforcement via D-Bus name claim.
//!
//! M4-plan § "Single-instance enforcement" + `DoD` #12: the tray
//! claims the well-known name `cz.zajca.Zwhisper1.Tray` on the
//! session bus. The claim is a presence marker only — there is no
//! `Tray1` server-side interface in M4 (deferred to M7).
//!
//! ## Why D-Bus over a lockfile
//!
//! - D-Bus is already a runtime dependency.
//! - Survives stale lockfile after `kill -9` (the daemon
//!   automatically releases the name when the connection drops).
//! - Future: a settings GUI in M7 can talk to the running tray
//!   via this same name.
//!
//! ## API contract
//!
//! [`claim`] returns:
//! - `Ok(true)` when the caller is the primary owner — the tray
//!   should keep running.
//! - `Ok(false)` when another instance already owns the name —
//!   `main.rs` logs and exits 0 cleanly.
//! - `Err(_)` when the bus call itself failed — the tray treats
//!   this as a soft failure (logs warn, keeps running) so a
//!   transient bus glitch does not kill the process before it has
//!   even started.
//!
//! The `cz.zajca.Zwhisper1.Tray` name is held for the lifetime of
//! the supplied [`zbus::Connection`]; dropping the connection
//! automatically releases the name (zbus 5.15 contract). The
//! caller is responsible for keeping the connection alive — in
//! M4's `main.rs` the dispatcher's connection is the natural
//! holder.

use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::names::WellKnownName;

/// Well-known name claimed by the tray to enforce single-instance.
/// This is **not** the daemon's name (`cz.zajca.Zwhisper1`); both
/// can coexist on the same connection because zbus 5.15 lets a
/// single connection own multiple well-known names.
pub const TRAY_BUS_NAME: &str = "cz.zajca.Zwhisper1.Tray";

#[derive(Debug, thiserror::Error)]
pub enum SingleInstanceError {
    #[error("dbus proxy: {0}")]
    Proxy(#[from] zbus::Error),
    #[error("invalid bus name {name}: {source}")]
    InvalidName {
        name: String,
        #[source]
        source: zbus::names::Error,
    },
    #[error("fdo error: {0}")]
    Fdo(#[from] zbus::fdo::Error),
}

/// Try to claim the tray's well-known name. See module docs for
/// the return-value semantics.
pub async fn claim(conn: &zbus::Connection) -> Result<bool, SingleInstanceError> {
    let proxy = DBusProxy::new(conn).await?;
    let name = WellKnownName::try_from(TRAY_BUS_NAME).map_err(|source| {
        SingleInstanceError::InvalidName {
            name: TRAY_BUS_NAME.to_owned(),
            source,
        }
    })?;
    let reply = proxy
        .request_name(name, RequestNameFlags::DoNotQueue.into())
        .await?;
    Ok(classify(&reply))
}

/// Pure helper extracted for testability.
pub fn classify(reply: &RequestNameReply) -> bool {
    match reply {
        // Both "we got it now" and "we already had it" are happy
        // paths from the tray's perspective.
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => true,
        // `InQueue` would only appear without `DoNotQueue`; we
        // pass `DoNotQueue` so it should not happen, but treat it
        // as "another instance owns it" defensively.
        RequestNameReply::Exists | RequestNameReply::InQueue => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn classify_primary_owner_returns_true() {
        assert!(classify(&RequestNameReply::PrimaryOwner));
    }

    #[test]
    fn classify_already_owner_returns_true() {
        assert!(classify(&RequestNameReply::AlreadyOwner));
    }

    #[test]
    fn classify_exists_returns_false() {
        assert!(!classify(&RequestNameReply::Exists));
    }

    #[test]
    fn classify_in_queue_returns_false_defensive() {
        assert!(!classify(&RequestNameReply::InQueue));
    }

    #[test]
    fn tray_bus_name_is_dotted_subpath_of_daemon_name() {
        // Sanity: the tray name must vendor-prefix-match the daemon
        // name so any future tooling can discover both via a
        // common prefix.
        assert!(TRAY_BUS_NAME.starts_with("cz.zajca.Zwhisper1"));
        assert_ne!(TRAY_BUS_NAME, "cz.zajca.Zwhisper1");
    }
}
