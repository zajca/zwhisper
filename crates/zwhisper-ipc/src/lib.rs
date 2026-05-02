//! `zwhisper-ipc` — D-Bus contract shared between `zwhisperd`
//! (server) and `zwhisper-cli` (client).
//!
//! This crate is the **single source of truth** for the wire format:
//! the proxy traits, wire-format structs, and typed errors all live
//! here. Both `zwhisperd` and `zwhisper-cli` depend on it; neither
//! re-declares any of it.
//!
//! ## What ships in M3
//!
//! - [`Recorder1Proxy`] — async client for the `Recorder1` interface
//!   (`StartRecording`, `StopRecording`, `GetStatus`, plus three
//!   signals).
//! - [`Profiles1Proxy`] — async client for the `Profiles1` interface
//!   (`List`, `GetActive`, `SetActive`, `Reload`).
//! - [`Status`] / [`ProfileEntry`] — wire-format structs with frozen
//!   `(sst)` / `(ssu)` signatures.
//! - [`RpcError`] — typed application-level errors with manual
//!   [`zbus::fdo::Error`] mapping (zbus 5.15 has no documented
//!   `DBusError` derive in stable).
//!
//! ## Architectural choice: proxy-only, server `#[interface]` in the daemon
//!
//! `zbus 5.15` ships two complementary macros:
//!
//! - `#[zbus::proxy]` decorates a free `trait` and emits a typed
//!   client struct (`Recorder1Proxy<'_>`).
//! - `#[zbus::interface]` decorates an `impl` block on a server-owned
//!   struct that holds state.
//!
//! There is no canonical way to share a single trait between both
//! sides because the server side has no business living in a free
//! `trait` (it owns mutable state). The pattern recommended by the
//! [zbus interface docs](https://docs.rs/zbus/5.15.0/zbus/attr.interface.html)
//! and the [zbus proxy docs](https://docs.rs/zbus/5.15.0/zbus/attr.proxy.html)
//! is to keep the proxy trait in a shared crate, then have the daemon
//! mirror the method set verbatim in its own `#[interface]` impl
//! (Phase 3 owns that mirror).
//!
//! To keep the two sides byte-identical, the wire-level interface
//! names are exposed here as constants ([`RECORDER_INTERFACE`],
//! [`PROFILES_INTERFACE`]) — Phase 3's `#[interface(name = …)]`
//! attribute reads from these.
//!
//! ## Frozen surface
//!
//! See `docs/M3-plan.md` § "Public API rules" — bus name, object
//! path, interface names, and wire signatures are locked. Future
//! widening goes through `Recorder2` / `Profiles2`, never an
//! incompatible mutation of `Recorder1` / `Profiles1`.

pub mod error;
pub mod profiles;
pub mod recorder;
pub mod types;

pub use error::{RpcError, parse_error_name, parse_error_name_from_zbus};
pub use profiles::Profiles1Proxy;
pub use recorder::Recorder1Proxy;
pub use types::{ProfileEntry, ProfileEntryV2, Status};

/// Well-known D-Bus name registered by `zwhisperd` on the session bus.
pub const BUS_NAME: &str = "cz.zajca.Zwhisper1";

/// Single object path the daemon serves. M3 has one object instance;
/// future major revisions go through new paths, not sub-paths.
pub const OBJECT_PATH: &str = "/cz/zajca/Zwhisper1";

/// Prefix for typed error names emitted by `RpcError`. The full name
/// is `{ERROR_NAME_PREFIX}{Variant}` (e.g.
/// `cz.zajca.Zwhisper1.Error.SessionInUse`). The mapping is the only
/// place this prefix is hardcoded — see [`error::parse_error_name`]
/// for the reverse direction.
pub const ERROR_NAME_PREFIX: &str = "cz.zajca.Zwhisper1.Error.";

/// D-Bus interface name for the `Recorder1` proxy. Phase 3 must use
/// this exact string in its `#[zbus::interface(name = …)]` attribute.
pub const RECORDER_INTERFACE: &str = "cz.zajca.Zwhisper1.Recorder1";

/// D-Bus interface name for the `Profiles1` proxy. Phase 3 must use
/// this exact string in its `#[zbus::interface(name = …)]` attribute.
pub const PROFILES_INTERFACE: &str = "cz.zajca.Zwhisper1.Profiles1";

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity check on the constants — these strings appear in service
    /// activation files (`/usr/share/dbus-1/services/…`), introspection
    /// XML, and CLI exit-code mappers. A typo here is hard to recover
    /// from once the activation file ships.
    #[test]
    fn constants_match_frozen_surface() {
        assert_eq!(BUS_NAME, "cz.zajca.Zwhisper1");
        assert_eq!(OBJECT_PATH, "/cz/zajca/Zwhisper1");
        assert_eq!(ERROR_NAME_PREFIX, "cz.zajca.Zwhisper1.Error.");
        assert_eq!(RECORDER_INTERFACE, "cz.zajca.Zwhisper1.Recorder1");
        assert_eq!(PROFILES_INTERFACE, "cz.zajca.Zwhisper1.Profiles1");
    }
}
