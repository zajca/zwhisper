//! M8 — daemon-side `Recorder1.ProtocolVersion` property (DoD #11).
//!
//! These tests pin two invariants over a real (private) D-Bus:
//!
//! 1. The property reads back the workspace's compile-time
//!    `PROTOCOL_VERSION` byte-for-byte. A deserialisation regression
//!    in zbus or a typo in `recorder_service.rs` would surface here
//!    before any client wires the handshake.
//!
//! 2. The property is readable from a fresh connection without an
//!    active recording session — clients perform the handshake as a
//!    pre-flight before any other RPC, so it must work in the idle
//!    `state="idle"` state.
//!
//! Skip-on-no-bus follows the existing `DbusFixture` pattern (M3
//! `tests/rpc.rs`) so CI runners without `dbus-daemon` keep the
//! suite green.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::pedantic
)]

mod common;

use common::{DbusFixture, FixtureSkip};
use zwhisper_ipc::PROTOCOL_VERSION;

async fn try_fixture(test_name: &str) -> Option<DbusFixture> {
    let mut fixture = match DbusFixture::try_new() {
        Ok(f) => f,
        Err(skip @ (FixtureSkip::NoDbusDaemon | FixtureSkip::NoDbusConfig)) => {
            eprintln!("[SKIP] {test_name}: {skip}");
            return None;
        }
        Err(FixtureSkip::Other(msg)) => {
            eprintln!("[SKIP] {test_name}: fixture unavailable — {msg}");
            return None;
        }
    };
    if let Err(e) = fixture.spawn_zwhisperd().await {
        eprintln!("[SKIP] {test_name}: zwhisperd failed to claim bus: {e}");
        return None;
    }
    Some(fixture)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn property_returns_workspace_version() {
    let Some(fixture) = try_fixture("property_returns_workspace_version").await else {
        return;
    };
    let proxy = fixture.proxy_recorder().await.expect("Recorder1 proxy");

    let got = proxy
        .protocol_version()
        .await
        .expect("ProtocolVersion property must be readable on a fresh daemon");

    assert_eq!(
        got, PROTOCOL_VERSION,
        "daemon must return the same workspace.package.version that \
         the test crate sees through env!(\"CARGO_PKG_VERSION\")"
    );
}

/// The handshake fires *before* any other RPC on every client. The
/// property must therefore be readable without a `StartRecording`
/// having ever happened — i.e. with the daemon in its initial idle
/// state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn property_is_readable_on_idle_daemon() {
    let Some(fixture) = try_fixture("property_is_readable_on_idle_daemon").await else {
        return;
    };
    let proxy = fixture.proxy_recorder().await.expect("Recorder1 proxy");

    let status = proxy
        .get_status()
        .await
        .expect("daemon should answer GetStatus on a fresh boot");
    assert_eq!(status.state, "idle", "fixture daemon must boot idle");

    let got = proxy
        .protocol_version()
        .await
        .expect("ProtocolVersion property must coexist with an idle daemon");
    assert_eq!(got, PROTOCOL_VERSION);
}

/// A client that calls the property twice in a row through two
/// independent connections must see the same value both times.
/// Pins out the (unlikely) regression where a state-bearing handler
/// accidentally consumes the value or rotates it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn property_is_stable_across_reconnects() {
    let Some(fixture) = try_fixture("property_is_stable_across_reconnects").await else {
        return;
    };

    let first = {
        let p = fixture.proxy_recorder().await.expect("Recorder1 proxy");
        p.protocol_version().await.expect("first read")
    };
    let second = {
        let p = fixture.proxy_recorder().await.expect("Recorder1 proxy");
        p.protocol_version().await.expect("second read")
    };

    assert_eq!(first, second);
    assert_eq!(first, PROTOCOL_VERSION);
}
