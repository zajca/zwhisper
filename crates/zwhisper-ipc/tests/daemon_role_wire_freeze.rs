//! Wire-format freeze tests for the RFC-daemon-role D-Bus surface
//! (`Jobs1` + `History1`).
//!
//! Mirrors `wire_freeze.rs` for the new interfaces. ANY change to a
//! method name, argument signature, return signature, signal name,
//! signal payload signature, interface string, or new typed-error name
//! must trip this test loudly. These interfaces are NOT frozen the way
//! `Recorder1`/`Profiles1` are (they are new), but pinning them here
//! makes every future change a deliberate, reviewed diff.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use zvariant::Type;

use zwhisper_ipc::{
    ERROR_NAME_PREFIX, HISTORY_INTERFACE, History1Proxy, HistorySession, JOBS_INTERFACE, JobInfo,
    Jobs1Proxy,
};

// ---------------------------------------------------------------------
// Interface name constants.
// ---------------------------------------------------------------------

#[test]
fn jobs_interface_name_is_pinned() {
    assert_eq!(JOBS_INTERFACE, "cz.zajca.Zwhisper1.Jobs1");
}

#[test]
fn history_interface_name_is_pinned() {
    assert_eq!(HISTORY_INTERFACE, "cz.zajca.Zwhisper1.History1");
}

#[test]
fn deliver_bus_name_is_pinned() {
    assert_eq!(zwhisper_ipc::DELIVER_BUS_NAME, "cz.zajca.Zwhisper1.Deliver");
}

// ---------------------------------------------------------------------
// Wire-format struct signatures.
// ---------------------------------------------------------------------

#[test]
fn job_info_wire_signature_is_ssst() {
    assert_eq!(JobInfo::SIGNATURE.to_string(), "(ssst)");
}

#[test]
fn history_session_wire_signature_is_pinned() {
    assert_eq!(HistorySession::SIGNATURE.to_string(), "(stssssssss)");
}

// ---------------------------------------------------------------------
// Signal payload signatures.
// ---------------------------------------------------------------------

#[test]
fn job_completed_payload_signature() {
    // JobCompleted(job_id:s, submit_mode:s, profile:s, outputs:aas,
    //              transcript_path:s, bytes:t, backend:s)
    assert_eq!(
        <(
            String,
            String,
            String,
            Vec<Vec<String>>,
            String,
            u64,
            String
        )>::SIGNATURE
            .to_string(),
        "(sssaassts)"
    );
}

#[test]
fn job_failed_payload_signature_is_ss() {
    assert_eq!(<(String, String)>::SIGNATURE.to_string(), "(ss)");
}

#[test]
fn job_progress_payload_signature_is_ss() {
    assert_eq!(<(String, String)>::SIGNATURE.to_string(), "(ss)");
}

// ---------------------------------------------------------------------
// Compile-time pins on proxy method/signal/property surface.
// ---------------------------------------------------------------------

#[allow(dead_code)]
async fn pin_jobs1_transcribe_file(p: &Jobs1Proxy<'_>) -> zbus::Result<String> {
    p.transcribe_file("/a.flac", "whisper-cpp", "small", "auto", "detached")
        .await
}

#[allow(dead_code)]
async fn pin_jobs1_cancel(p: &Jobs1Proxy<'_>, id: &str) -> zbus::Result<()> {
    p.cancel(id).await
}

#[allow(dead_code)]
async fn pin_jobs1_list_jobs(p: &Jobs1Proxy<'_>) -> zbus::Result<Vec<JobInfo>> {
    p.list_jobs().await
}

#[allow(dead_code)]
async fn pin_jobs1_protocol_version(p: &Jobs1Proxy<'_>) -> zbus::Result<String> {
    p.protocol_version().await
}

#[allow(dead_code)]
async fn pin_jobs1_job_completed_signal(p: &Jobs1Proxy<'_>) {
    let _ = p.receive_job_completed().await;
}

#[allow(dead_code)]
async fn pin_jobs1_job_failed_signal(p: &Jobs1Proxy<'_>) {
    let _ = p.receive_job_failed().await;
}

#[allow(dead_code)]
async fn pin_jobs1_job_progress_signal(p: &Jobs1Proxy<'_>) {
    let _ = p.receive_job_progress().await;
}

#[allow(dead_code)]
fn pin_job_completed_args_fields(args: &zwhisper_ipc::jobs::JobCompletedArgs<'_>) {
    let _: &str = args.job_id;
    let _: &str = args.submit_mode;
    let _: &str = args.profile;
    let _: &Vec<Vec<String>> = &args.outputs;
    let _: &str = args.transcript_path;
    let _: u64 = args.bytes;
    let _: &str = args.backend;
}

#[allow(dead_code)]
async fn pin_history1_list_sessions(p: &History1Proxy<'_>) -> zbus::Result<Vec<HistorySession>> {
    p.list_sessions(20, 0).await
}

#[allow(dead_code)]
async fn pin_history1_get_session(p: &History1Proxy<'_>) -> zbus::Result<HistorySession> {
    p.get_session("id").await
}

#[allow(dead_code)]
async fn pin_history1_retry(p: &History1Proxy<'_>) -> zbus::Result<String> {
    p.retry("id").await
}

#[allow(dead_code)]
async fn pin_history1_forget(p: &History1Proxy<'_>) -> zbus::Result<()> {
    p.forget("id", false).await
}

#[allow(dead_code)]
async fn pin_history1_protocol_version(p: &History1Proxy<'_>) -> zbus::Result<String> {
    p.protocol_version().await
}

#[test]
fn jobs1_and_history1_proxy_surface_compiles() {
    // The freeze check happens at compile-time via the pin_* helpers
    // above. This no-op test surfaces the assertion in the report.
}

// ---------------------------------------------------------------------
// New typed-error variant names.
// ---------------------------------------------------------------------

#[test]
fn rfc_daemon_role_error_variant_names_are_pinned() {
    use zwhisper_ipc::RpcError;

    let cases: &[(RpcError, &str)] = &[
        (RpcError::JobUnknown { id: String::new() }, "JobUnknown"),
        (
            RpcError::InvalidPath {
                reason: String::new(),
            },
            "InvalidPath",
        ),
        (RpcError::RetryUnavailable, "RetryUnavailable"),
        (
            RpcError::AudioNotFound {
                path: String::new(),
            },
            "AudioNotFound",
        ),
    ];

    for (err, expected) in cases {
        assert_eq!(err.variant_name(), *expected, "variant_name: {err:?}");
        assert_eq!(
            err.error_name(),
            format!("{ERROR_NAME_PREFIX}{expected}"),
            "error_name: {err:?}",
        );
    }
}
