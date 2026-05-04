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

// `PROTOCOL_VERSION`, `PROTOCOL_VERSION_PROPERTY`, and
// `ProtocolMismatch` are defined further down in this file (after the
// other wire constants) so they live next to `BUS_NAME`, `OBJECT_PATH`,
// `RECORDER_INTERFACE`, etc. — the canonical wire-surface block. They
// are intentionally not re-exported here since they are top-level
// items already.

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

/// Workspace-wide protocol version, exposed over D-Bus as the
/// read-only `cz.zajca.Zwhisper1.Recorder1.ProtocolVersion` property
/// (M8 DoD #9, #11).
///
/// This is the wire-level contract between the daemon and every
/// client (`zwhisper-cli`, `zwhisper-tray`, `zwhisper-settings`).
/// Bumped in lockstep with `Cargo.toml` `workspace.package.version`
/// so a partial upgrade — e.g. a new daemon shipped without a
/// matching tray — is detected at the first RPC instead of producing
/// confusing wire-format errors deeper in the call stack.
///
/// `env!("CARGO_PKG_VERSION")` reads the value at compile time from
/// the same `Cargo.toml` field that drives every other crate's
/// version, so cross-crate drift is impossible by construction.
pub const PROTOCOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// D-Bus property name on the `Recorder1` interface that returns
/// [`PROTOCOL_VERSION`]. Hardcoded on the daemon side (in the
/// `#[zbus(property)]` attribute) and on the client side (every
/// pre-flight handshake reads this exact name). Recorded here so
/// the typo-budget is one place, not three.
pub const PROTOCOL_VERSION_PROPERTY: &str = "ProtocolVersion";

/// Typed error reported when a client's compile-time
/// [`PROTOCOL_VERSION`] does not match the value the daemon advertises
/// at run time. Distinct from [`RpcError`] because a mismatch is a
/// pre-flight refusal — no server-side state was touched.
///
/// The `got` field carries the literal daemon string, including the
/// sentinel value `"pre-0.1.0"` that clients substitute when the
/// daemon is so old it does not implement the property at all
/// (zbus surfaces the call as `MethodCallNotImplemented` /
/// `UnknownProperty`).
#[derive(Debug, Clone, thiserror::Error)]
#[error("daemon protocol mismatch: expected {expected}, got {got}")]
pub struct ProtocolMismatch {
    /// Compile-time [`PROTOCOL_VERSION`] of the running client.
    pub expected: String,
    /// Run-time `ProtocolVersion` reported by the daemon, or the
    /// sentinel `"pre-0.1.0"` for daemons that lack the property.
    pub got: String,
}

impl ProtocolMismatch {
    /// Sentinel value substituted for `got` when the daemon does not
    /// implement [`PROTOCOL_VERSION_PROPERTY`] at all (legacy
    /// pre-0.1.0 daemon). Distinct from a real version string so the
    /// CLI can render a tailored "reinstall the daemon" hint.
    pub const LEGACY_DAEMON_SENTINEL: &'static str = "pre-0.1.0";

    /// Build a mismatch error for a daemon that returned a concrete
    /// (but wrong) version string.
    #[must_use]
    pub fn new(got: impl Into<String>) -> Self {
        Self {
            expected: PROTOCOL_VERSION.to_owned(),
            got: got.into(),
        }
    }

    /// Build a mismatch error for a legacy daemon that did not
    /// implement the property.
    #[must_use]
    pub fn legacy_daemon() -> Self {
        Self {
            expected: PROTOCOL_VERSION.to_owned(),
            got: Self::LEGACY_DAEMON_SENTINEL.to_owned(),
        }
    }

    /// `true` when the daemon is older than the protocol-version
    /// rollout — i.e. did not implement the property. Lets clients
    /// render a tailored "reinstall the daemon" hint instead of the
    /// generic mismatch message.
    #[must_use]
    pub fn is_legacy_daemon(&self) -> bool {
        self.got == Self::LEGACY_DAEMON_SENTINEL
    }
}

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
