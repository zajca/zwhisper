use std::process::Command;

use tracing::debug;

use super::error::DeviceError;
use crate::node_name;

/// Resolved `PipeWire` node names ready to feed into
/// `pipewiresrc target-object=…`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceSelection {
    pub mic_node: String,
    /// Resolved sink-monitor node, or `None` for **mic-only** capture
    /// (RFC-mic-setup Phase 5): an empty `monitor_arg` yields `None`
    /// here and the pipeline builds a single-source mic graph with no
    /// `audiomixer`. The empty argument is honoured verbatim — never
    /// coerced to `"default"` (the M2 review's High finding caught that
    /// silent coercion capturing system audio against the user's
    /// intent). `Some(name)` is a concrete monitor node (mic + sink
    /// monitor mono mix).
    pub monitor_node: Option<String>,
}

/// Indirection over `wpctl` and `pw-cli` so the resolver can be unit
/// tested without a running `PipeWire` daemon. Production wires up
/// [`WpctlCommandRunner`].
pub(crate) trait WpctlRunner {
    /// Body of `wpctl inspect <alias>` — used to resolve
    /// `@DEFAULT_AUDIO_SOURCE@` / `@DEFAULT_AUDIO_SINK@` (the
    /// `wpctl` aliases for current `PipeWire` defaults).
    fn inspect(&self, alias: &str) -> Result<String, DeviceError>;

    /// Enumeration of every `node.name` known to `PipeWire`. Backed by
    /// `pw-cli ls Node` because `wpctl status` only prints
    /// human-readable descriptions, not the canonical names that
    /// `pipewiresrc target-object` consumes.
    fn list_node_names(&self) -> Result<Vec<String>, DeviceError>;
}

#[derive(Debug, Default)]
pub(crate) struct WpctlCommandRunner;

impl WpctlRunner for WpctlCommandRunner {
    fn inspect(&self, alias: &str) -> Result<String, DeviceError> {
        let output = Command::new("wpctl")
            .args(["inspect", alias])
            .output()
            .map_err(|e| DeviceError::CommandFailed {
                tool: "wpctl",
                message: format!("could not spawn `wpctl inspect {alias}`: {e}"),
            })?;

        // wpctl prints "Object 'X' not found" to stdout and still exits 0,
        // so a successful exit is not enough — we must inspect the body.
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            return Err(DeviceError::CommandFailed {
                tool: "wpctl",
                message: format!(
                    "`wpctl inspect {alias}` exited with status {:?}: {stderr}",
                    output.status.code()
                ),
            });
        }

        if stdout.trim().is_empty() || stdout.contains("not found") {
            return Err(DeviceError::CommandFailed {
                tool: "wpctl",
                message: format!("`wpctl inspect {alias}` returned no node: {stdout}{stderr}"),
            });
        }

        Ok(stdout)
    }

    fn list_node_names(&self) -> Result<Vec<String>, DeviceError> {
        let output = Command::new("pw-cli")
            .args(["ls", "Node"])
            .output()
            .map_err(|e| DeviceError::CommandFailed {
                tool: "pw-cli",
                message: format!("could not spawn `pw-cli ls Node`: {e}"),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DeviceError::CommandFailed {
                tool: "pw-cli",
                message: format!(
                    "`pw-cli ls Node` exited with status {:?}: {stderr}",
                    output.status.code()
                ),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_node_names(&stdout))
    }
}

/// Extract every `node.name = "<value>"` line from a `pw-cli ls Node`
/// dump. Lines that look like `node.name.foo = …` are skipped to
/// avoid grabbing fallback aliases.
fn parse_node_names(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in body.lines() {
        let trimmed = raw.trim_start_matches([' ', '*', '\t']);
        let Some(rest) = trimmed.strip_prefix("node.name") else {
            continue;
        };
        let after_eq = rest.trim_start();
        let Some(value) = after_eq.strip_prefix('=') else {
            continue;
        };
        let value = value.trim();
        if let Some(stripped) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            if !stripped.is_empty() {
                out.push(stripped.to_owned());
            }
        }
    }
    out
}

/// Resolve the user-provided `--mic` and `--monitor` strings into
/// concrete `PipeWire` node names suitable for `pipewiresrc`.
///
/// - `"default"` triggers a `wpctl inspect @DEFAULT_AUDIO_SOURCE@`
///   (mic) or `@DEFAULT_AUDIO_SINK@` lookup; the monitor name is the
///   sink's `node.name` with `.monitor` appended.
/// - Anything else is taken verbatim. Validation happens at pipeline
///   pre-roll time — invalid names surface as
///   [`crate::audio::RecordingError::PipelineFailed`].
pub(crate) fn resolve(
    runner: &impl WpctlRunner,
    mic_arg: &str,
    monitor_arg: &str,
) -> Result<DeviceSelection, DeviceError> {
    let mic_node = if mic_arg == "default" {
        let body = runner.inspect("@DEFAULT_AUDIO_SOURCE@")?;
        let resolved = parse_node_name(&body, "@DEFAULT_AUDIO_SOURCE@")?;
        validate_node_name(&resolved, &resolved)?;
        resolved
    } else {
        let candidate = validate_explicit(mic_arg, "mic")?;
        ensure_node_exists(runner, &candidate, "mic")?;
        candidate
    };

    let monitor_node = if monitor_arg.is_empty() {
        // Mic-only mode (RFC-mic-setup Phase 5): an empty monitor means
        // "no system audio". Return `None` so the pipeline builds a
        // single-source mic graph. Critically, the empty value is NOT
        // coerced to "default" — the M2 review's High finding caught
        // that silent coercion capturing system audio against the
        // user's intent.
        None
    } else if monitor_arg == "default" {
        let body = runner.inspect("@DEFAULT_AUDIO_SINK@")?;
        let sink_name = parse_node_name(&body, "@DEFAULT_AUDIO_SINK@")?;
        validate_node_name(&sink_name, &sink_name)?;
        Some(format!("{sink_name}.monitor"))
    } else {
        let candidate = validate_explicit(monitor_arg, "monitor")?;
        // Require the literal node name to appear in `pw-cli ls Node`.
        // We do not accept a parent sink as proof that its
        // `.monitor` exists: PipeWire publishes monitor sources as
        // their own Node objects, and accepting the parent would
        // silently hand `pipewiresrc` a name it cannot resolve,
        // turning a fast-fail into a vague preroll-time
        // "target not found".
        ensure_node_exists(runner, &candidate, "monitor")?;
        Some(candidate)
    };

    debug!(
        mic = %mic_node,
        monitor = monitor_node.as_deref().unwrap_or("(mic-only)"),
        "resolved PipeWire nodes"
    );

    Ok(DeviceSelection {
        mic_node,
        monitor_node,
    })
}

/// Extract `node.name = "<value>"` from a `wpctl inspect` body. Both
/// the starred form (`* node.name = "..."`) and the plain form are
/// accepted — older `wpctl` versions omit the asterisk for some
/// properties. Lines like `node.name.fallback = "..."` are skipped.
fn parse_node_name(body: &str, alias: &str) -> Result<String, DeviceError> {
    for line in body.lines() {
        let trimmed = line.trim_start_matches([' ', '*', '\t']);
        if let Some(rest) = trimmed.strip_prefix("node.name") {
            // Match `node.name = "value"` exactly to avoid grabbing
            // properties like `node.name.fallback` if they ever appear.
            let after_eq = rest.trim_start();
            if let Some(value) = after_eq.strip_prefix('=') {
                let value = value.trim();
                if let Some(stripped) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                    if !stripped.is_empty() {
                        return Ok(stripped.to_owned());
                    }
                }
            }
        }
    }

    Err(DeviceError::NodeNameMissing {
        alias: alias.to_owned(),
        output: body.to_owned(),
    })
}

/// Confirm that `name` appears in the live `pw-cli ls Node`
/// enumeration. Surfaces a typed `DeviceError::InvalidArgument` with
/// a sample of the available names — better diagnostics than letting
/// `pipewiresrc` emit a generic "target not found" at preroll.
fn ensure_node_exists(
    runner: &impl WpctlRunner,
    name: &str,
    kind: &'static str,
) -> Result<(), DeviceError> {
    let names = runner.list_node_names()?;
    if names.iter().any(|n| n == name) {
        return Ok(());
    }
    Err(DeviceError::InvalidArgument {
        value: format!(
            "{name} (available {kind} candidates: {})",
            sample_names(&names)
        ),
        reason: "node name not found in `pw-cli ls Node`",
    })
}

fn sample_names(names: &[String]) -> String {
    const MAX: usize = 8;
    let head: Vec<&str> = names.iter().take(MAX).map(String::as_str).collect();
    if names.len() > MAX {
        format!("{} … ({} more)", head.join(", "), names.len() - MAX)
    } else {
        head.join(", ")
    }
}

fn validate_explicit(value: &str, kind: &'static str) -> Result<String, DeviceError> {
    let trimmed = value.trim();
    validate_node_name(trimmed, value)?;
    debug!(kind, name = %trimmed, "using explicit device argument");
    Ok(trimmed.to_owned())
}

/// Allow-list validation for `PipeWire` node names, delegated to the
/// shared [`crate::node_name`] validator (the single source of truth
/// reused by the GStreamer-free `setup` module). A rejection maps to a
/// typed [`DeviceError::InvalidArgument`] carrying the *original*
/// (untrimmed) value and the validator's stable reason string — the
/// same messages this module produced before the extraction, so callers
/// asserting on them are unaffected.
fn validate_node_name(trimmed: &str, original: &str) -> Result<(), DeviceError> {
    node_name::validate_node_name(trimmed).map_err(|e| DeviceError::InvalidArgument {
        value: original.to_owned(),
        reason: e.reason(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    struct MockRunner {
        source_body: Result<String, DeviceError>,
        sink_body: Result<String, DeviceError>,
        node_names: Result<Vec<String>, DeviceError>,
    }

    impl WpctlRunner for MockRunner {
        fn inspect(&self, alias: &str) -> Result<String, DeviceError> {
            match alias {
                "@DEFAULT_AUDIO_SOURCE@" => self.source_body.clone(),
                "@DEFAULT_AUDIO_SINK@" => self.sink_body.clone(),
                other => Err(DeviceError::CommandFailed {
                    tool: "wpctl",
                    message: format!("unexpected alias `{other}` in test"),
                }),
            }
        }

        fn list_node_names(&self) -> Result<Vec<String>, DeviceError> {
            self.node_names.clone()
        }
    }

    impl Clone for DeviceError {
        fn clone(&self) -> Self {
            match self {
                Self::CommandFailed { tool, message } => Self::CommandFailed {
                    tool,
                    message: message.clone(),
                },
                Self::NodeNameMissing { alias, output } => Self::NodeNameMissing {
                    alias: alias.clone(),
                    output: output.clone(),
                },
                Self::InvalidArgument { value, reason } => Self::InvalidArgument {
                    value: value.clone(),
                    reason,
                },
            }
        }
    }

    const SOURCE_FIXTURE: &str = r#"id 62, type PipeWire:Interface:Node
    alsa.card = "2"
  * node.description = "PHL 34B1U5601 Analog Stereo"
  * node.name = "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo"
    media.class = "Audio/Source"
"#;

    const SINK_FIXTURE: &str = r#"id 74, type PipeWire:Interface:Node
  * node.name = "alsa_output.usb-Generic_PHL_34B1U5601-00.analog-stereo"
    media.class = "Audio/Sink"
"#;

    fn happy_runner() -> MockRunner {
        MockRunner {
            source_body: Ok(SOURCE_FIXTURE.to_owned()),
            sink_body: Ok(SINK_FIXTURE.to_owned()),
            node_names: Ok(vec![
                "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo".to_owned(),
                "alsa_output.usb-Generic_PHL_34B1U5601-00.analog-stereo".to_owned(),
                "my.mic.node".to_owned(),
                "my.sink.node".to_owned(),
                "my.sink.node.monitor".to_owned(),
                "explicit.monitor".to_owned(),
            ]),
        }
    }

    #[test]
    fn resolves_defaults_via_wpctl() {
        let selection = resolve(&happy_runner(), "default", "default").unwrap();
        assert_eq!(
            selection.mic_node,
            "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo"
        );
        assert_eq!(
            selection.monitor_node.as_deref(),
            Some("alsa_output.usb-Generic_PHL_34B1U5601-00.analog-stereo.monitor")
        );
    }

    #[test]
    fn explicit_arguments_pass_through_unchanged() {
        let selection = resolve(&happy_runner(), "my.mic.node", "my.sink.node.monitor").unwrap();
        assert_eq!(selection.mic_node, "my.mic.node");
        assert_eq!(
            selection.monitor_node.as_deref(),
            Some("my.sink.node.monitor")
        );
    }

    #[test]
    fn empty_monitor_arg_yields_mic_only_selection() {
        // RFC-mic-setup Phase 5: an empty monitor means mic-only —
        // `monitor_node` is `None` and the sink is never inspected.
        // The empty value must NOT be coerced to "default" (the M2
        // review's High finding caught that silent coercion capturing
        // system audio against the user's intent).
        let runner = MockRunner {
            source_body: Ok(SOURCE_FIXTURE.to_owned()),
            sink_body: Err(DeviceError::CommandFailed {
                tool: "wpctl",
                message: "must not be called when monitor_arg is empty".into(),
            }),
            node_names: Ok(vec![
                "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo".to_owned(),
            ]),
        };
        let selection = resolve(&runner, "default", "").unwrap();
        assert_eq!(
            selection.mic_node,
            "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo"
        );
        assert_eq!(
            selection.monitor_node, None,
            "empty monitor must resolve to mic-only (None), not \"default\""
        );
    }

    #[test]
    fn explicit_overrides_skip_wpctl_for_that_field() {
        // Sink lookup would fail, but explicit mic should still resolve.
        let runner = MockRunner {
            source_body: Ok(SOURCE_FIXTURE.to_owned()),
            sink_body: Err(DeviceError::CommandFailed {
                tool: "wpctl",
                message: "should not be called".into(),
            }),
            node_names: Ok(vec![
                "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo".to_owned(),
                "explicit.monitor".to_owned(),
            ]),
        };
        let selection = resolve(&runner, "default", "explicit.monitor").unwrap();
        assert_eq!(
            selection.mic_node,
            "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo"
        );
        assert_eq!(selection.monitor_node.as_deref(), Some("explicit.monitor"));
    }

    #[test]
    fn empty_explicit_argument_is_rejected() {
        let err = resolve(&happy_runner(), "  ", "default").unwrap_err();
        assert!(matches!(err, DeviceError::InvalidArgument { .. }));
    }

    #[test]
    fn whitespace_in_explicit_argument_is_rejected() {
        let err = resolve(&happy_runner(), "default", "has space").unwrap_err();
        assert!(matches!(err, DeviceError::InvalidArgument { .. }));
    }

    #[test]
    fn missing_node_name_in_wpctl_output_surfaces_error() {
        let runner = MockRunner {
            source_body: Ok(
                "id 62, type PipeWire:Interface:Node\n  some.other.prop = \"x\"\n".into(),
            ),
            sink_body: Ok(SINK_FIXTURE.to_owned()),
            node_names: Ok(vec![]),
        };
        let err = resolve(&runner, "default", "default").unwrap_err();
        assert!(matches!(err, DeviceError::NodeNameMissing { .. }));
    }

    #[test]
    fn wpctl_failure_propagates_with_tool_name() {
        let runner = MockRunner {
            source_body: Err(DeviceError::CommandFailed {
                tool: "wpctl",
                message: "exit 1".into(),
            }),
            sink_body: Ok(SINK_FIXTURE.to_owned()),
            node_names: Ok(vec![]),
        };
        let err = resolve(&runner, "default", "default").unwrap_err();
        match err {
            DeviceError::CommandFailed { tool, .. } => assert_eq!(tool, "wpctl"),
            other => panic!("expected CommandFailed{{tool=wpctl}}, got {other:?}"),
        }
    }

    #[test]
    fn pw_cli_failure_is_attributed_to_pw_cli_not_wpctl() {
        // Regression: previously a `pw-cli ls Node` failure surfaced
        // through `WpctlFailed`, sending the user to debug the wrong
        // binary. The error must name `pw-cli` so the diagnostics
        // are actionable.
        let runner = MockRunner {
            source_body: Ok(SOURCE_FIXTURE.to_owned()),
            sink_body: Ok(SINK_FIXTURE.to_owned()),
            node_names: Err(DeviceError::CommandFailed {
                tool: "pw-cli",
                message: "could not spawn `pw-cli ls Node`: No such file or directory".into(),
            }),
        };
        let err = resolve(&runner, "explicit.mic", "default").unwrap_err();
        match err {
            DeviceError::CommandFailed { tool, message } => {
                assert_eq!(tool, "pw-cli");
                assert!(message.contains("pw-cli"), "unexpected message: {message}");
            }
            other => panic!("expected CommandFailed{{tool=pw-cli}}, got {other:?}"),
        }
    }

    #[test]
    fn explicit_mic_not_in_pipewire_is_rejected() {
        let runner = MockRunner {
            source_body: Ok(SOURCE_FIXTURE.to_owned()),
            sink_body: Ok(SINK_FIXTURE.to_owned()),
            node_names: Ok(vec!["only.this.exists".to_owned()]),
        };
        let err = resolve(&runner, "missing.mic", "default").unwrap_err();
        match err {
            DeviceError::InvalidArgument { reason, .. } => {
                assert!(reason.contains("not found"), "unexpected reason: {reason}");
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn explicit_monitor_requires_literal_node_in_pw_cli() {
        // Parent sink alone is not proof the `.monitor` source
        // exists — accepting it would silently hand `pipewiresrc`
        // a name it cannot resolve.
        let runner = MockRunner {
            source_body: Ok(SOURCE_FIXTURE.to_owned()),
            sink_body: Ok(SINK_FIXTURE.to_owned()),
            node_names: Ok(vec![
                "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo".to_owned(),
                "my.real.sink".to_owned(),
            ]),
        };
        let err = resolve(&runner, "default", "my.real.sink.monitor").unwrap_err();
        match err {
            DeviceError::InvalidArgument { reason, .. } => {
                assert!(reason.contains("not found"), "unexpected reason: {reason}");
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn explicit_monitor_passes_when_literal_node_present() {
        let runner = MockRunner {
            source_body: Ok(SOURCE_FIXTURE.to_owned()),
            sink_body: Ok(SINK_FIXTURE.to_owned()),
            node_names: Ok(vec![
                "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo".to_owned(),
                "my.real.sink".to_owned(),
                "my.real.sink.monitor".to_owned(),
            ]),
        };
        let selection = resolve(&runner, "default", "my.real.sink.monitor").unwrap();
        assert_eq!(
            selection.monitor_node.as_deref(),
            Some("my.real.sink.monitor")
        );
    }

    #[test]
    fn parse_node_names_picks_only_canonical_lines() {
        let body = "\
id 1, type PipeWire:Interface:Node
  node.name = \"foo\"
  node.name.fallback = \"skip\"
  * node.name = \"bar\"
";
        let names = parse_node_names(body);
        assert_eq!(names, vec!["foo".to_owned(), "bar".to_owned()]);
    }

    #[test]
    fn parses_node_name_without_asterisk() {
        let body = "id 62\n    node.name = \"plain.node\"\n";
        let name = parse_node_name(body, "@TEST@").unwrap();
        assert_eq!(name, "plain.node");
    }

    #[test]
    fn parses_node_name_with_asterisk() {
        let body = "id 62\n  * node.name = \"starred.node\"\n";
        let name = parse_node_name(body, "@TEST@").unwrap();
        assert_eq!(name, "starred.node");
    }

    #[test]
    fn does_not_match_node_name_fallback() {
        let body = "id 62\n    node.name.fallback = \"wrong\"\n    node.name = \"right\"\n";
        let name = parse_node_name(body, "@TEST@").unwrap();
        assert_eq!(name, "right");
    }
}
