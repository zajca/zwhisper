//! Per-command runtime dispatchers for `zwhisper`.
//!
//! Each module owns one CLI subcommand:
//! - [`record`] — D-Bus client to `Recorder1.StartRecording`, with
//!   full signal-driven lifecycle (C3 + C4).
//! - [`transcribe`] — local-only invocation of `whisper-cli` via
//!   `zwhisper-core::transcribe`. The daemon does not yet expose a
//!   transcribe-only RPC.
//! - [`profile`] — `list` + `show` go through `Profiles1.List`, with
//!   a graceful local fallback when the daemon is down. `clone` +
//!   `migrate` stay local.
//! - [`status`] — `Recorder1.GetStatus` + actionable hint when the
//!   daemon is not on the bus.
//!
//! ## Exit-code map (frozen for M3+, see `DoD` #12)
//!
//! | Exit | Trigger                                                           |
//! |------|-------------------------------------------------------------------|
//! | `0`  | clean stop, optional transcript delivered                         |
//! | `1`  | recording or transcribe failure (`StateChanged "failed"` / typed) |
//! | `2`  | user-facing protocol error (daemon down, profile-not-found, …)    |
//! | `3`  | IPC failure (unclassifiable zbus error)                           |
//!
//! [`classify_error`] maps a `zbus::Error` to the corresponding exit
//! code; record/status/profile dispatchers consult it before bailing
//! to keep the table single-sourced.

pub(crate) mod backend;
pub(crate) mod hotkey;
pub(crate) mod profile;
pub(crate) mod record;
pub(crate) mod status;
pub(crate) mod toggle;
pub(crate) mod transcribe;

/// Exit code for a clean run.
pub(crate) const EXIT_OK: i32 = 0;
/// Exit code for a recording/transcribe failure (`StateChanged "failed"`
/// or a typed `RpcError::RecordingFailed`).
pub(crate) const EXIT_RECORDING_FAILED: i32 = 1;
/// Exit code for any user-facing protocol error: daemon not running,
/// profile not found, session in use, bad arguments.
pub(crate) const EXIT_PROTOCOL_ERROR: i32 = 2;
/// Exit code for an IPC-level failure that is neither (1) nor (2):
/// bus disconnect, unparseable reply, transport error.
pub(crate) const EXIT_IPC_FAILURE: i32 = 3;
/// M8 — the daemon's `Recorder1.ProtocolVersion` does not match this
/// CLI's compile-time [`zwhisper_ipc::PROTOCOL_VERSION`]. Distinct
/// from [`EXIT_PROTOCOL_ERROR`] (which means "the daemon talked back
/// fine, but said no") so packagers and CI can detect partial
/// upgrades without parsing stderr.
pub(crate) const EXIT_VERSION_MISMATCH: i32 = 4;

/// Well-known D-Bus error names for the daemon-not-running case. We
/// hit `ServiceUnknown` when the bus has no activation entry, and
/// `NameHasNoOwner` when the daemon crashed mid-call. Both surface as
/// exit code 2 with the same actionable hint.
pub(crate) const ERR_SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";
pub(crate) const ERR_NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";

/// Hint shown to the user when the daemon cannot be reached.
pub(crate) const DAEMON_DOWN_HINT: &str = "daemon not running. Start it manually: `systemctl --user start zwhisperd`. Or run any zwhisper command and the D-Bus activation file at `/usr/share/dbus-1/services/cz.zajca.Zwhisper1.service` will spawn it on first call.";

/// Map a `zbus::Error` to one of the exit codes from the `DoD` #12
/// table.
///
/// - `MethodError` whose name is `ServiceUnknown` / `NameHasNoOwner`
///   means the daemon is not on the bus → exit 2.
/// - Any error whose body carries the typed
///   `cz.zajca.Zwhisper1.Error.<Variant>` prefix is a `RpcError`.
///   `RecordingFailed` → 1, everything else (`SessionInUse`,
///   `ProfileNotFound`, `ProfileLoadFailed`, `SessionUnknown`,
///   `Transient`) → 2.
/// - Any other `zbus::Error` (transport, marshalling, address) → 3.
///
/// On the wire the daemon emits typed errors as
/// `org.freedesktop.DBus.Error.Failed` with the `cz.zajca…:` prefix
/// stuffed in the message body (zbus 5.15 `fdo::Error` has no
/// arbitrary-name variant). The `MethodError.name` field is therefore
/// almost never the typed name — the typed name lives in the body.
/// [`zwhisper_ipc::parse_error_name_from_zbus`] handles both wire
/// shapes (and the future possibility that the daemon sets the typed
/// wire name directly).
#[must_use]
pub(crate) fn classify_error(err: &zbus::Error) -> i32 {
    if is_daemon_down(err) {
        return EXIT_PROTOCOL_ERROR;
    }
    if let Some(variant) = zwhisper_ipc::parse_error_name_from_zbus(err) {
        return match variant {
            "RecordingFailed" => EXIT_RECORDING_FAILED,
            _ => EXIT_PROTOCOL_ERROR,
        };
    }
    EXIT_IPC_FAILURE
}

/// True when the error is a `MethodError` carrying the daemon-down
/// well-known name. Callers print [`DAEMON_DOWN_HINT`] in that case.
#[must_use]
pub(crate) fn is_daemon_down(err: &zbus::Error) -> bool {
    if let zbus::Error::MethodError(name, ..) = err {
        let name_str: &str = name.as_str();
        return name_str == ERR_SERVICE_UNKNOWN || name_str == ERR_NAME_HAS_NO_OWNER;
    }
    false
}

/// Outcome of the M8 pre-flight handshake (DoD #12).
///
/// Three states matter to the caller:
///
/// - `Match` — daemon advertises the same `PROTOCOL_VERSION` as the
///   client. The caller proceeds with its real RPC.
/// - `Mismatch(...)` — daemon answered, but with a different version
///   string OR the legacy "no such property" sentinel. The caller
///   exits [`EXIT_VERSION_MISMATCH`] after rendering the canonical
///   user-facing message.
/// - `DaemonDown` — the property call failed with `ServiceUnknown` /
///   `NameHasNoOwner`. The caller falls back to its existing
///   daemon-down code path so the user sees [`DAEMON_DOWN_HINT`]
///   instead of a confusing "version mismatch" message.
#[derive(Debug)]
pub(crate) enum HandshakeOutcome {
    Match,
    Mismatch(zwhisper_ipc::ProtocolMismatch),
    DaemonDown,
}

/// M8 pre-flight handshake. Reads
/// `Recorder1.ProtocolVersion` and classifies the result against the
/// CLI's compile-time [`zwhisper_ipc::PROTOCOL_VERSION`].
pub(crate) async fn verify_protocol(proxy: &zwhisper_ipc::Recorder1Proxy<'_>) -> HandshakeOutcome {
    match proxy.protocol_version().await {
        Ok(daemon_version) => {
            if daemon_version == zwhisper_ipc::PROTOCOL_VERSION {
                HandshakeOutcome::Match
            } else {
                HandshakeOutcome::Mismatch(zwhisper_ipc::ProtocolMismatch::new(daemon_version))
            }
        }
        Err(err) if is_daemon_down(&err) => HandshakeOutcome::DaemonDown,
        Err(zbus::Error::FDO(boxed)) => match *boxed {
            // zbus 5.15 surfaces "no such property / method / interface"
            // when the daemon is older than M8 and never grew the
            // ProtocolVersion property. We treat each of these as
            // the legacy-daemon case.
            zbus::fdo::Error::UnknownMethod(_)
            | zbus::fdo::Error::UnknownProperty(_)
            | zbus::fdo::Error::UnknownInterface(_) => {
                HandshakeOutcome::Mismatch(zwhisper_ipc::ProtocolMismatch::legacy_daemon())
            }
            other => HandshakeOutcome::Mismatch(zwhisper_ipc::ProtocolMismatch::new(format!(
                "fdo error: {other}"
            ))),
        },
        Err(other) => HandshakeOutcome::Mismatch(zwhisper_ipc::ProtocolMismatch::new(format!(
            "unexpected protocol-version error: {other}"
        ))),
    }
}

/// Print the canonical user-facing message for a [`verify_protocol`]
/// failure to stderr. Pulled out so every command surfaces the same
/// wording (DoD #12 contract). Returns the exit code the caller
/// should propagate.
#[allow(clippy::print_stderr)]
pub(crate) fn report_protocol_mismatch(err: &zwhisper_ipc::ProtocolMismatch) -> i32 {
    if err.is_legacy_daemon() {
        eprintln!(
            "{err}\nThe running daemon predates the protocol-version handshake \
             (added in 0.1.0). Reinstall zwhisperd to match this client."
        );
    } else {
        eprintln!(
            "{err}\nThe daemon and this client were built from different \
             zwhisper releases. Reinstall the matching zwhisperd or zwhisper \
             package."
        );
    }
    EXIT_VERSION_MISMATCH
}

/// Build a current-thread tokio runtime for a one-shot CLI dispatch.
/// Each command builds its own runtime so the synchronous file-I/O
/// commands (`profile clone`, `profile migrate`) do not have to enter
/// one they will not use. See `main.rs` rustdoc for the full
/// rationale.
pub(crate) fn build_runtime() -> color_eyre::Result<tokio::runtime::Runtime> {
    use color_eyre::eyre::eyre;

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| eyre!("failed to build tokio runtime: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Build a synthetic `MethodError` with a chosen name. zbus 5.15
    /// exposes the variant as a public tuple so tests can fabricate
    /// one without a live bus. The mapper only consults the name —
    /// we use a placeholder signal `Message` so the third tuple
    /// element parses without a real reply.
    fn synthetic_method_error(name: &'static str, body: Option<String>) -> zbus::Error {
        let placeholder = zbus::Message::signal("/", "test.Iface", "Sig")
            .expect("builder")
            .build(&())
            .expect("build");
        zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from(name).expect("valid name"),
            body,
            placeholder,
        )
    }

    /// The realistic wire shape: daemon sends typed errors as
    /// `org.freedesktop.DBus.Error.Failed` with
    /// `cz.zajca.Zwhisper1.Error.<Variant>: <msg>` in the body.
    /// Mirrors `From<RpcError> for zbus::fdo::Error` exactly so unit
    /// tests catch the bug that misled the original M3 review.
    fn typed_method_error(variant: &str, msg: &str) -> zbus::Error {
        let body = format!("{}{variant}: {msg}", zwhisper_ipc::ERROR_NAME_PREFIX);
        synthetic_method_error("org.freedesktop.DBus.Error.Failed", Some(body))
    }

    #[test]
    fn classify_service_unknown_is_protocol_error() {
        let err = synthetic_method_error("org.freedesktop.DBus.Error.ServiceUnknown", None);
        assert_eq!(classify_error(&err), EXIT_PROTOCOL_ERROR);
        assert!(is_daemon_down(&err));
    }

    #[test]
    fn classify_name_has_no_owner_is_protocol_error() {
        let err = synthetic_method_error("org.freedesktop.DBus.Error.NameHasNoOwner", None);
        assert_eq!(classify_error(&err), EXIT_PROTOCOL_ERROR);
        assert!(is_daemon_down(&err));
    }

    #[test]
    fn classify_recording_failed_is_exit_1() {
        let err = typed_method_error("RecordingFailed", "gstreamer init: …");
        assert_eq!(classify_error(&err), EXIT_RECORDING_FAILED);
        assert!(!is_daemon_down(&err));
    }

    #[test]
    fn classify_session_in_use_is_protocol_error() {
        let err = typed_method_error("SessionInUse", "id=abc");
        assert_eq!(classify_error(&err), EXIT_PROTOCOL_ERROR);
    }

    #[test]
    fn classify_profile_not_found_is_protocol_error() {
        let err = typed_method_error("ProfileNotFound", "name=\"meeting\"");
        assert_eq!(classify_error(&err), EXIT_PROTOCOL_ERROR);
    }

    #[test]
    fn classify_unknown_zwhisper_variant_falls_back_to_ipc_failure() {
        // A future RpcError variant the CLI does not yet recognise
        // is not in the suffix table → returns None from
        // parse_error_name_from_zbus → exit 3. Forward-compat is
        // explicit: add the new variant to the IPC crate first, then
        // the CLI table.
        let err = typed_method_error("WhoKnowsWhat", "future variant");
        assert_eq!(classify_error(&err), EXIT_IPC_FAILURE);
    }

    #[test]
    fn classify_unrelated_method_error_is_ipc_failure() {
        let err = synthetic_method_error("org.example.Other", Some("nope".into()));
        assert_eq!(classify_error(&err), EXIT_IPC_FAILURE);
        assert!(!is_daemon_down(&err));
    }

    /// Regression: even if the daemon ever switches to setting the
    /// typed wire name directly (skipping the `Failed` workaround)
    /// the classifier must still pick it up.
    #[test]
    fn classify_typed_wire_name_directly_is_recognised() {
        let err = synthetic_method_error("cz.zajca.Zwhisper1.Error.RecordingFailed", None);
        assert_eq!(classify_error(&err), EXIT_RECORDING_FAILED);
    }
}
