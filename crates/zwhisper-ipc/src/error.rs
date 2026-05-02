//! Typed RPC errors and their D-Bus mapping.
//!
//! zbus 5.15 does not document a stable `#[derive(DBusError)]` for
//! preserving custom error names through the wire, so we go through
//! [`zbus::fdo::Error::Failed`] and encode the name as a `"<name>: <msg>"`
//! prefix in the body string. Both the daemon (server-side) and the
//! CLI (client-side) live in this same crate and use [`parse_error_name`]
//! to recover the typed variant on the receiving end.
//!
//! The `Failed` variant is the only [`zbus::fdo::Error`] case used here:
//! `fdo::Error` exposes a fixed set of well-known names, none of which
//! are application-specific, so all `RpcError` variants funnel through
//! `Failed` regardless of suffix.

use crate::ERROR_NAME_PREFIX;

/// Application-level RPC errors surfaced on both `Recorder1` and
/// `Profiles1` interfaces. See M3 plan § 20 and stress-test corrections
/// C8 / C11 for the full mapping.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RpcError {
    /// A recording session is already active. The daemon enforces a
    /// single-session policy; the CLI surfaces this as exit-code 2
    /// "session-busy".
    #[error("a recording session is already active (id={existing})")]
    SessionInUse { existing: String },

    /// The supplied `session_id` does not match the active session
    /// (e.g. it referred to a session that already terminated, or a
    /// different daemon instance).
    #[error("session id {id} is not active")]
    SessionUnknown { id: String },

    /// The named profile was not found on disk. Per C11, an empty
    /// `name` is normalised to `"(empty)"` before being raised.
    #[error("profile {name:?} not found")]
    ProfileNotFound { name: String },

    /// The profile file exists but cannot be parsed or migrated.
    #[error("failed to load profile {name:?}: {reason}")]
    ProfileLoadFailed { name: String, reason: String },

    /// The audio pipeline failed to start, errored mid-recording, or
    /// the post-process step (whisper-cli, ffmpeg) returned non-zero.
    #[error("recording failed: {reason}")]
    RecordingFailed { reason: String },

    /// A transient lower-level fault, retryable from the caller's
    /// perspective. Currently used as a catch-all when the cause is
    /// unknown but a retry might succeed.
    #[error("transient error: {reason}")]
    Transient { reason: String },
}

impl RpcError {
    /// The unprefixed variant name, used as the suffix after
    /// [`ERROR_NAME_PREFIX`] when encoding into [`zbus::fdo::Error`].
    #[must_use]
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::SessionInUse { .. } => "SessionInUse",
            Self::SessionUnknown { .. } => "SessionUnknown",
            Self::ProfileNotFound { .. } => "ProfileNotFound",
            Self::ProfileLoadFailed { .. } => "ProfileLoadFailed",
            Self::RecordingFailed { .. } => "RecordingFailed",
            Self::Transient { .. } => "Transient",
        }
    }

    /// The fully-qualified D-Bus error name (e.g.
    /// `cz.zajca.Zwhisper1.Error.SessionInUse`).
    #[must_use]
    pub fn error_name(&self) -> String {
        format!("{ERROR_NAME_PREFIX}{}", self.variant_name())
    }
}

impl From<RpcError> for zbus::fdo::Error {
    fn from(err: RpcError) -> Self {
        // zbus 5.15 `fdo::Error` does not expose an arbitrary-name
        // variant. Encoding the typed name as a `"<name>: <msg>"`
        // prefix keeps the information round-trippable via
        // [`parse_error_name`] without forking the upstream enum.
        let name = err.error_name();
        Self::Failed(format!("{name}: {err}"))
    }
}

/// Reverse of [`From<RpcError> for zbus::fdo::Error`]: given a
/// [`zbus::fdo::Error`], return the unprefixed variant name (e.g.
/// `"SessionInUse"`) when the error was emitted by `zwhisperd`.
///
/// Returns `None` for any other variant (including `Failed` payloads
/// that do not start with [`ERROR_NAME_PREFIX`]). The CLI uses this
/// for typed exit-code mapping; mismatches fall through to the generic
/// "unknown error" exit code.
#[must_use]
pub fn parse_error_name(err: &zbus::fdo::Error) -> Option<&'static str> {
    let zbus::fdo::Error::Failed(msg) = err else {
        return None;
    };
    parse_typed_suffix(msg)
}

/// Wire-level reverse of [`From<RpcError> for zbus::fdo::Error`].
///
/// `parse_error_name` only sees an `fdo::Error`. In practice a client
/// receives `RpcError` payloads as `zbus::Error::MethodError(name,
/// body, ...)` where `name == "org.freedesktop.DBus.Error.Failed"`
/// and the typed `cz.zajca.Zwhisper1.Error.<Variant>: <msg>` prefix
/// rides in `body`. Some zbus call sites surface the same payload
/// through `zbus::Error::FDO(Box<fdo::Error::Failed(...)>)` instead.
/// This helper handles both shapes plus the (rare) case where a
/// future daemon learns to set the typed wire name directly.
///
/// Returns `None` when the error did not originate from `zwhisperd`
/// or the body does not carry a recognised variant suffix.
#[must_use]
pub fn parse_error_name_from_zbus(err: &zbus::Error) -> Option<&'static str> {
    match err {
        zbus::Error::MethodError(name, body, _) => {
            // Future-proofing: if the daemon ever escapes the
            // `Failed` workaround and sets the typed wire name
            // directly, accept it.
            let raw_name: &str = name.as_str();
            if let Some(suffix) = raw_name.strip_prefix(ERROR_NAME_PREFIX) {
                return match_known_variant(suffix);
            }
            if raw_name == "org.freedesktop.DBus.Error.Failed" {
                return parse_typed_suffix(body.as_deref()?);
            }
            None
        }
        zbus::Error::FDO(boxed) => match boxed.as_ref() {
            zbus::fdo::Error::Failed(msg) => parse_typed_suffix(msg),
            _ => None,
        },
        _ => None,
    }
}

fn parse_typed_suffix(msg: &str) -> Option<&'static str> {
    let suffix = msg.strip_prefix(ERROR_NAME_PREFIX)?;
    let (name, _rest) = suffix.split_once(':')?;
    match_known_variant(name)
}

fn match_known_variant(name: &str) -> Option<&'static str> {
    match name {
        "SessionInUse" => Some("SessionInUse"),
        "SessionUnknown" => Some("SessionUnknown"),
        "ProfileNotFound" => Some("ProfileNotFound"),
        "ProfileLoadFailed" => Some("ProfileLoadFailed"),
        "RecordingFailed" => Some("RecordingFailed"),
        "Transient" => Some("Transient"),
        _ => None,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::match_wildcard_for_single_variants
)]
mod tests {
    use super::*;

    /// All variants, instantiated with realistic payloads, for
    /// table-driven tests.
    fn all_variants() -> Vec<(RpcError, &'static str)> {
        vec![
            (
                RpcError::SessionInUse {
                    existing: "abc".into(),
                },
                "SessionInUse",
            ),
            (
                RpcError::SessionUnknown { id: "xyz".into() },
                "SessionUnknown",
            ),
            (
                RpcError::ProfileNotFound {
                    name: "missing".into(),
                },
                "ProfileNotFound",
            ),
            (
                RpcError::ProfileLoadFailed {
                    name: "broken".into(),
                    reason: "bad toml".into(),
                },
                "ProfileLoadFailed",
            ),
            (
                RpcError::RecordingFailed {
                    reason: "gst pipeline EOS".into(),
                },
                "RecordingFailed",
            ),
            (
                RpcError::Transient {
                    reason: "pipewire not ready".into(),
                },
                "Transient",
            ),
        ]
    }

    #[test]
    fn rpc_error_session_in_use_round_trips_through_fdo() {
        let original = RpcError::SessionInUse {
            existing: "abc".into(),
        };
        let fdo: zbus::fdo::Error = original.into();
        assert_eq!(parse_error_name(&fdo), Some("SessionInUse"));
    }

    #[test]
    fn rpc_error_each_variant_uses_the_prefix() {
        for (err, expected) in all_variants() {
            assert_eq!(err.variant_name(), expected, "variant_name mismatch");
            let fdo: zbus::fdo::Error = err.clone().into();
            // `assert!(matches!(…))` instead of an `else { panic!() }`
            // — keeps the `clippy::panic` workspace lint clean.
            assert!(
                matches!(&fdo, zbus::fdo::Error::Failed(_)),
                "expected Failed variant, got {fdo:?}",
            );
            if let zbus::fdo::Error::Failed(msg) = &fdo {
                assert!(
                    msg.starts_with(ERROR_NAME_PREFIX),
                    "msg {msg:?} does not start with {ERROR_NAME_PREFIX:?}",
                );
            }
            assert_eq!(parse_error_name(&fdo), Some(expected));
        }
    }

    #[test]
    fn parse_error_name_returns_none_for_unrelated_failed_error() {
        let fdo = zbus::fdo::Error::Failed("something else entirely".into());
        assert_eq!(parse_error_name(&fdo), None);
    }

    #[test]
    fn parse_error_name_returns_none_for_non_failed_variant() {
        let fdo = zbus::fdo::Error::AccessDenied("nope".into());
        assert_eq!(parse_error_name(&fdo), None);
    }

    #[test]
    fn parse_error_name_returns_none_for_unknown_variant_after_prefix() {
        // A future RpcError variant we do not yet recognise should
        // surface as `None` rather than a wrong typed name. This
        // protects the CLI exit-code mapper from silent drift.
        let fdo = zbus::fdo::Error::Failed(format!(
            "{ERROR_NAME_PREFIX}WhoKnowsWhat: some message",
        ));
        assert_eq!(parse_error_name(&fdo), None);
    }

    /// Build a `zbus::Error::MethodError` whose wire shape matches
    /// what the daemon actually sends after `From<RpcError> for
    /// fdo::Error::Failed`: name = `org.freedesktop.DBus.Error.Failed`,
    /// body = `"cz.zajca.Zwhisper1.Error.<Variant>: <msg>"`.
    fn typed_method_error(variant: &str, msg: &str) -> zbus::Error {
        let placeholder = zbus::Message::signal("/", "test.Iface", "Sig")
            .expect("builder")
            .build(&())
            .expect("build");
        let body = format!("{ERROR_NAME_PREFIX}{variant}: {msg}");
        zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from("org.freedesktop.DBus.Error.Failed")
                .expect("valid name"),
            Some(body),
            placeholder,
        )
    }

    #[test]
    fn parse_from_zbus_decodes_real_wire_method_error() {
        for variant in ["RecordingFailed", "SessionInUse", "ProfileNotFound"] {
            let err = typed_method_error(variant, "details here");
            assert_eq!(
                parse_error_name_from_zbus(&err),
                Some(variant),
                "variant {variant} did not round-trip through MethodError",
            );
        }
    }

    #[test]
    fn parse_from_zbus_decodes_fdo_wrapped_error() {
        let fdo = zbus::fdo::Error::Failed(format!(
            "{ERROR_NAME_PREFIX}RecordingFailed: gst init failed",
        ));
        let err = zbus::Error::FDO(Box::new(fdo));
        assert_eq!(parse_error_name_from_zbus(&err), Some("RecordingFailed"));
    }

    #[test]
    fn parse_from_zbus_returns_none_for_unrelated_method_error() {
        let placeholder = zbus::Message::signal("/", "test.Iface", "Sig")
            .expect("builder")
            .build(&())
            .expect("build");
        let err = zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from("org.example.Other").expect("valid"),
            Some("nope".into()),
            placeholder,
        );
        assert_eq!(parse_error_name_from_zbus(&err), None);
    }

    #[test]
    fn parse_from_zbus_decodes_typed_wire_name_directly() {
        // Forward-compat: if the daemon ever escapes the `Failed`
        // workaround, the typed wire name is also accepted.
        let placeholder = zbus::Message::signal("/", "test.Iface", "Sig")
            .expect("builder")
            .build(&())
            .expect("build");
        let err = zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from("cz.zajca.Zwhisper1.Error.SessionInUse")
                .expect("valid"),
            None,
            placeholder,
        );
        assert_eq!(parse_error_name_from_zbus(&err), Some("SessionInUse"));
    }

    #[test]
    fn parse_from_zbus_returns_none_for_non_method_error_variants() {
        // Address parse failure is a non-MethodError, non-FDO branch.
        let err = zbus::Error::Address("bad address".into());
        assert_eq!(parse_error_name_from_zbus(&err), None);
    }
}
