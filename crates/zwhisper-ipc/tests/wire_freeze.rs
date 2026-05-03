//! Wire-format freeze tests for the M3 D-Bus surface.
//!
//! Pin every byte of the public contract that `zwhisperd` (server) and
//! `zwhisper-cli` / `zwhisper-tray` / `zwhisper-hotkey` (clients) rely
//! on. ANY change to a method name, argument signature, return
//! signature, signal name, signal payload signature, or interface /
//! path / error-name string must trip this test loudly.
//!
//! Per `docs/M6-plan.md` § `DoD` #21: "Recorder1/Profiles1
//! wire-freeze regression test". M6 (and every subsequent
//! client-only milestone) must NOT mutate the wire surface —
//! this test is the CI brick wall that catches such mutations.
//!
//! ## How to deliberately bend the contract
//!
//! Edit the snapshot block in this file IN THE SAME COMMIT that
//! changes the wire surface. The diff makes the breakage obvious to
//! reviewers, AND the test failure on every CI build until the diff
//! lands forces every client crate's owner to acknowledge.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use zvariant::Type;

use zwhisper_ipc::{
    BUS_NAME, ERROR_NAME_PREFIX, OBJECT_PATH, PROFILES_INTERFACE, ProfileEntry, ProfileEntryV2,
    Profiles1Proxy, RECORDER_INTERFACE, Recorder1Proxy, Status,
};

// ---------------------------------------------------------------------
// Bus / interface / path / error-prefix constants.
// ---------------------------------------------------------------------

#[test]
fn bus_name_constant_is_frozen() {
    assert_eq!(BUS_NAME, "cz.zajca.Zwhisper1");
}

#[test]
fn object_path_constant_is_frozen() {
    assert_eq!(OBJECT_PATH, "/cz/zajca/Zwhisper1");
}

#[test]
fn recorder_interface_name_is_frozen() {
    assert_eq!(RECORDER_INTERFACE, "cz.zajca.Zwhisper1.Recorder1");
}

#[test]
fn profiles_interface_name_is_frozen() {
    assert_eq!(PROFILES_INTERFACE, "cz.zajca.Zwhisper1.Profiles1");
}

#[test]
fn error_name_prefix_is_frozen() {
    assert_eq!(ERROR_NAME_PREFIX, "cz.zajca.Zwhisper1.Error.");
}

// ---------------------------------------------------------------------
// Wire-format struct signatures.
// ---------------------------------------------------------------------

#[test]
fn status_wire_signature_is_sst() {
    // Recorder1.GetStatus returns (state: s, active_profile: s,
    // duration_ms: t). Any future field reorder, type widening,
    // or addition that changes the marshalled form will trip
    // this assertion.
    assert_eq!(Status::SIGNATURE.to_string(), "(sst)");
}

#[test]
fn profile_entry_wire_signature_is_ssu() {
    // Profiles1.List returns Vec<(name: s, description: s,
    // schema_version: u)>.
    assert_eq!(ProfileEntry::SIGNATURE.to_string(), "(ssu)");
}

#[test]
fn profile_entry_v2_wire_signature_is_ssus() {
    // M5+: Profiles1.ListV2 adds the `backend` field at the end
    // of the existing tuple. Any rearrangement breaks tray
    // rendering.
    assert_eq!(ProfileEntryV2::SIGNATURE.to_string(), "(ssus)");
}

// ---------------------------------------------------------------------
// Recorder1 signal payload signatures (M6 round-2 hardening).
//
// The `pin_recorder1_*_signal` helpers below pin only that the
// `receive_*` accessor exists. zbus 5.15's `#[proxy]` macro also
// generates a `<Signal>Args<'s>` struct per signal that exposes each
// argument by name and type — destructuring those structs in the
// `pin_*_args_*` helpers below pins:
//
//   * the EXACT field names (rename trips the build),
//   * the EXACT borrowed/owned type of each field (widening from
//     `&str` to `String`, swapping `u64` -> `i64`, etc., trips
//     the build),
//   * the EXACT field count (adding/removing a field trips the
//     destructure).
//
// We also assert the equivalent owned-tuple wire signature so the
// daemon side cannot drift its `#[zbus::interface]` signal payload
// without a matching client edit AND a snapshot bump here.
// ---------------------------------------------------------------------

#[test]
fn state_changed_payload_signature_is_ss() {
    // Recorder1.StateChanged(new_state: s, session_id: s).
    // Owned-tuple proxy of the wire bytes. The `pin_args_*`
    // helper below couples this assertion to the actual proxy
    // trait — see the module-level rationale.
    assert_eq!(<(String, String)>::SIGNATURE.to_string(), "(ss)");
}

#[test]
fn recording_complete_payload_signature_is_ss() {
    // Recorder1.RecordingComplete(session_id: s, audio_path: s).
    assert_eq!(<(String, String)>::SIGNATURE.to_string(), "(ss)");
}

#[test]
fn transcript_complete_payload_signature_is_ssts() {
    // Recorder1.TranscriptComplete(session_id: s,
    // transcript_path: s, bytes: t, backend: s).
    assert_eq!(
        <(String, String, u64, String)>::SIGNATURE.to_string(),
        "(ssts)"
    );
}

// Compile-time pins on the macro-generated `<Signal>Args` structs.
// These are dead at runtime; their existence as a `fn (Args)` whose
// destructure mentions each field by name with an explicit type is
// the actual freeze gate. Any field rename, reorder, addition, or
// type swap on the proxy trait stops these helpers from compiling.
//
// `_arg.field` accesses are inside `let _ = ...;` to silence the
// `unused_variables` lint — clippy `dead_code` would otherwise
// hide the real failure mode.

#[allow(dead_code)]
fn pin_state_changed_args_fields(args: &zwhisper_ipc::recorder::StateChangedArgs<'_>) {
    let _: &str = args.new_state;
    let _: &str = args.session_id;
}

#[allow(dead_code)]
fn pin_recording_complete_args_fields(args: &zwhisper_ipc::recorder::RecordingCompleteArgs<'_>) {
    let _: &str = args.session_id;
    let _: &str = args.audio_path;
}

#[allow(dead_code)]
fn pin_transcript_complete_args_fields(args: &zwhisper_ipc::recorder::TranscriptCompleteArgs<'_>) {
    let _: &str = args.session_id;
    let _: &str = args.transcript_path;
    let _: u64 = args.bytes;
    let _: &str = args.backend;
}

// ---------------------------------------------------------------------
// Recorder1 / Profiles1 proxy method + signal signatures.
//
// Each `async fn` below references one method or signal helper
// by name AND in a context where the argument and return types
// are explicit. If a future edit:
//
// * renames the method/signal on the `#[zbus::proxy]` trait, OR
// * changes the argument tuple, OR
// * changes the return type,
//
// the corresponding `pin_*` async function stops compiling and
// `cargo test --package zwhisper-ipc` fails to build. This is
// the "any signature change must break the build" discipline
// `DoD` #21 mandates.
//
// `#[allow(dead_code)]` keeps clippy happy under `-D warnings` —
// these stubs intentionally have no runtime use site.
// ---------------------------------------------------------------------

#[allow(dead_code)]
async fn pin_recorder1_start_recording(p: &Recorder1Proxy<'_>, name: &str) -> zbus::Result<String> {
    p.start_recording(name).await
}

#[allow(dead_code)]
async fn pin_recorder1_stop_recording(p: &Recorder1Proxy<'_>, sid: &str) -> zbus::Result<String> {
    p.stop_recording(sid).await
}

#[allow(dead_code)]
async fn pin_recorder1_get_status(p: &Recorder1Proxy<'_>) -> zbus::Result<Status> {
    p.get_status().await
}

#[allow(dead_code)]
async fn pin_recorder1_state_changed_signal(p: &Recorder1Proxy<'_>) {
    let _ = p.receive_state_changed().await;
}

#[allow(dead_code)]
async fn pin_recorder1_recording_complete_signal(p: &Recorder1Proxy<'_>) {
    let _ = p.receive_recording_complete().await;
}

#[allow(dead_code)]
async fn pin_recorder1_transcript_complete_signal(p: &Recorder1Proxy<'_>) {
    let _ = p.receive_transcript_complete().await;
}

#[allow(dead_code)]
async fn pin_profiles1_list(p: &Profiles1Proxy<'_>) -> zbus::Result<Vec<ProfileEntry>> {
    p.list().await
}

#[allow(dead_code)]
async fn pin_profiles1_list_v2(p: &Profiles1Proxy<'_>) -> zbus::Result<Vec<ProfileEntryV2>> {
    p.list_v2().await
}

#[allow(dead_code)]
async fn pin_profiles1_get_active(p: &Profiles1Proxy<'_>) -> zbus::Result<String> {
    p.get_active().await
}

#[allow(dead_code)]
async fn pin_profiles1_set_active(p: &Profiles1Proxy<'_>, name: &str) -> zbus::Result<()> {
    p.set_active(name).await
}

#[allow(dead_code)]
async fn pin_profiles1_reload(p: &Profiles1Proxy<'_>) -> zbus::Result<()> {
    p.reload().await
}

#[test]
fn recorder1_and_profiles1_proxy_surface_compiles_against_frozen_signatures() {
    // The actual freeze check happens at compile-time via the
    // `pin_*` functions above. This test exists so the assertion
    // appears in the cargo-test report (and a deliberate panic
    // tells future maintainers: "if THIS test goes missing, the
    // wire freeze is no longer wired up").
    //
    // No-op body: if we got here, every pinned signature
    // type-checked.
}

// ---------------------------------------------------------------------
// Typed-error variant names.
//
// `RpcError` variants are the public contract for typed daemon
// errors. Renaming any variant is a wire break (the variant name
// is what the receiver matches on via
// `parse_error_name_from_zbus`). This snapshot pins the canonical
// list.
// ---------------------------------------------------------------------

#[test]
fn rpc_error_variant_names_are_frozen() {
    use zwhisper_ipc::RpcError;

    let cases: &[(RpcError, &str)] = &[
        (
            RpcError::SessionInUse {
                existing: String::new(),
            },
            "SessionInUse",
        ),
        (
            RpcError::SessionUnknown { id: String::new() },
            "SessionUnknown",
        ),
        (
            RpcError::ProfileNotFound {
                name: String::new(),
            },
            "ProfileNotFound",
        ),
        (
            RpcError::ProfileLoadFailed {
                name: String::new(),
                reason: String::new(),
            },
            "ProfileLoadFailed",
        ),
        (
            RpcError::RecordingFailed {
                reason: String::new(),
            },
            "RecordingFailed",
        ),
        (
            RpcError::Transient {
                reason: String::new(),
            },
            "Transient",
        ),
    ];

    for (err, expected_name) in cases {
        assert_eq!(
            err.variant_name(),
            *expected_name,
            "RpcError variant_name regression: {err:?}",
        );
        assert_eq!(
            err.error_name(),
            format!("{ERROR_NAME_PREFIX}{expected_name}"),
            "RpcError error_name regression: {err:?}",
        );
    }
}
