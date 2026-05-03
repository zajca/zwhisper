//! M8 — pre-flight protocol-version handshake (DoD #9, #10).
//!
//! These tests pin the wire-level invariants that every client and
//! the daemon rely on:
//!
//! 1. `PROTOCOL_VERSION` reads from the same `CARGO_PKG_VERSION` that
//!    every other crate in the workspace inherits via
//!    `version.workspace = true`. A divergence here would produce
//!    silent "old client talks to old daemon and both think they're
//!    fine" failures — exactly what the M8 handshake is meant to
//!    catch (DoD #9).
//!
//! 2. `ProtocolMismatch` renders the canonical user-facing string
//!    so the CLI / tray / settings can hand the rendered message
//!    straight to stderr / a notification / a banner without their
//!    own format glue (DoD #10).

use zwhisper_ipc::{PROTOCOL_VERSION, PROTOCOL_VERSION_PROPERTY, ProtocolMismatch};

#[test]
fn const_matches_workspace_version() {
    // Both sides of this assertion call `env!("CARGO_PKG_VERSION")`,
    // but each is resolved at the compile time of its own crate. If
    // a future workspace.package.version bump misses zwhisper-ipc
    // (e.g. the crate's own `version.workspace = true` accidentally
    // becomes `version = "..."`), the two values diverge and this
    // test fails before the workspace ever ships.
    assert_eq!(PROTOCOL_VERSION, env!("CARGO_PKG_VERSION"));
}

#[test]
fn protocol_version_property_name_is_stable() {
    // M3 surface freeze: this string appears in
    // `#[zbus(property)]` on the daemon and in
    // `proxy.get_property("ProtocolVersion")` on every client.
    // A typo here is a wire break.
    assert_eq!(PROTOCOL_VERSION_PROPERTY, "ProtocolVersion");
}

#[test]
fn mismatch_error_displays_expected_got() {
    let err = ProtocolMismatch {
        expected: "0.1.0".to_owned(),
        got: "0.2.0".to_owned(),
    };
    assert_eq!(
        err.to_string(),
        "daemon protocol mismatch: expected 0.1.0, got 0.2.0"
    );
}

#[test]
fn new_helper_uses_compile_time_expected() {
    let err = ProtocolMismatch::new("99.0.0");
    assert_eq!(err.expected, PROTOCOL_VERSION);
    assert_eq!(err.got, "99.0.0");
    assert!(!err.is_legacy_daemon());
}

#[test]
fn legacy_daemon_sentinel_is_distinct_from_real_versions() {
    let legacy = ProtocolMismatch::legacy_daemon();
    assert_eq!(legacy.expected, PROTOCOL_VERSION);
    assert_eq!(legacy.got, "pre-0.1.0");
    assert!(legacy.is_legacy_daemon());

    // A daemon that genuinely reports `"0.0.0"` is treated as a
    // mismatched-but-modern daemon, NOT as legacy. The legacy path
    // is reserved for "property does not exist" — the CLI tailors
    // its hint message based on this distinction.
    let zero_zero_zero = ProtocolMismatch::new("0.0.0");
    assert!(!zero_zero_zero.is_legacy_daemon());
}

#[test]
fn mismatch_error_implements_std_error() {
    fn assert_error<E: std::error::Error>(_: &E) {}
    let err = ProtocolMismatch::new("99.0.0");
    assert_error(&err);
}
