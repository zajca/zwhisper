use std::process::Command;

use tracing::debug;

use super::error::DeviceError;

/// Maximum length of a `PipeWire` node name we will accept. Real
/// names observed on Arch are well under 100 chars; this just keeps
/// us safe from runaway inputs.
const MAX_NODE_NAME_LEN: usize = 256;

/// Resolved `PipeWire` node names ready to feed into
/// `pipewiresrc target-object=…`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceSelection {
    pub mic_node: String,
    pub monitor_node: String,
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
            .map_err(|e| DeviceError::WpctlFailed {
                message: format!("could not spawn `wpctl inspect {alias}`: {e}"),
            })?;

        // wpctl prints "Object 'X' not found" to stdout and still exits 0,
        // so a successful exit is not enough — we must inspect the body.
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            return Err(DeviceError::WpctlFailed {
                message: format!(
                    "`wpctl inspect {alias}` exited with status {:?}: {stderr}",
                    output.status.code()
                ),
            });
        }

        if stdout.trim().is_empty() || stdout.contains("not found") {
            return Err(DeviceError::WpctlFailed {
                message: format!("`wpctl inspect {alias}` returned no node: {stdout}{stderr}"),
            });
        }

        Ok(stdout)
    }

    fn list_node_names(&self) -> Result<Vec<String>, DeviceError> {
        let output = Command::new("pw-cli").args(["ls", "Node"]).output().map_err(
            |e| DeviceError::WpctlFailed {
                message: format!("could not spawn `pw-cli ls Node`: {e}"),
            },
        )?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DeviceError::WpctlFailed {
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

    let monitor_node = if monitor_arg == "default" {
        let body = runner.inspect("@DEFAULT_AUDIO_SINK@")?;
        let sink_name = parse_node_name(&body, "@DEFAULT_AUDIO_SINK@")?;
        validate_node_name(&sink_name, &sink_name)?;
        format!("{sink_name}.monitor")
    } else {
        let candidate = validate_explicit(monitor_arg, "monitor")?;
        // Accept either the literal node we are pointed at, or — in
        // the common case of `<sink>.monitor` — the parent sink. If
        // neither exists we fail fast with a typed error rather than
        // letting GStreamer surface a vague "target not found".
        let candidate_parent = candidate.strip_suffix(".monitor");
        ensure_one_of_exists(runner, [&candidate, candidate_parent.unwrap_or("")], "monitor")?;
        candidate
    };

    debug!(
        mic = %mic_node,
        monitor = %monitor_node,
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
        value: format!("{name} (available {kind} candidates: {})", sample_names(&names)),
        reason: "node name not found in `pw-cli ls Node`",
    })
}

fn ensure_one_of_exists<'a>(
    runner: &impl WpctlRunner,
    candidates: impl IntoIterator<Item = &'a str>,
    kind: &'static str,
) -> Result<(), DeviceError> {
    let candidates: Vec<&str> = candidates.into_iter().filter(|c| !c.is_empty()).collect();
    let names = runner.list_node_names()?;
    if candidates.iter().any(|c| names.iter().any(|n| n == *c)) {
        return Ok(());
    }
    Err(DeviceError::InvalidArgument {
        value: format!(
            "{} (available {kind} candidates: {})",
            candidates.join(" or "),
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

/// Allow-list validation for `PipeWire` node names. The `gst-launch`
/// DSL grammar (consumed by `gst::parse::launch`) treats `!`, `.`,
/// `=`, `,`, `(`, `)` and quotes as syntactically meaningful.
/// `PipeWire`
/// node names in the wild are restricted to alphanumerics, dots,
/// underscores, hyphens, and `:` (for media-class qualifiers). We
/// accept exactly that set so a malicious or malformed name cannot
/// inject elements into the pipeline string.
fn validate_node_name(trimmed: &str, original: &str) -> Result<(), DeviceError> {
    if trimmed.is_empty() {
        return Err(DeviceError::InvalidArgument {
            value: original.to_owned(),
            reason: "empty value",
        });
    }
    if trimmed.len() > MAX_NODE_NAME_LEN {
        return Err(DeviceError::InvalidArgument {
            value: original.to_owned(),
            reason: "node name exceeds 256 characters",
        });
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
    {
        return Err(DeviceError::InvalidArgument {
            value: original.to_owned(),
            reason: "node names must match [A-Za-z0-9._:-]+",
        });
    }
    Ok(())
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
                other => Err(DeviceError::WpctlFailed {
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
                Self::WpctlFailed { message } => Self::WpctlFailed {
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
            selection.monitor_node,
            "alsa_output.usb-Generic_PHL_34B1U5601-00.analog-stereo.monitor"
        );
    }

    #[test]
    fn explicit_arguments_pass_through_unchanged() {
        let selection = resolve(&happy_runner(), "my.mic.node", "my.sink.node.monitor").unwrap();
        assert_eq!(selection.mic_node, "my.mic.node");
        assert_eq!(selection.monitor_node, "my.sink.node.monitor");
    }

    #[test]
    fn explicit_overrides_skip_wpctl_for_that_field() {
        // Sink lookup would fail, but explicit mic should still resolve.
        let runner = MockRunner {
            source_body: Ok(SOURCE_FIXTURE.to_owned()),
            sink_body: Err(DeviceError::WpctlFailed {
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
        assert_eq!(selection.monitor_node, "explicit.monitor");
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
            source_body: Ok("id 62, type PipeWire:Interface:Node\n  some.other.prop = \"x\"\n".into()),
            sink_body: Ok(SINK_FIXTURE.to_owned()),
            node_names: Ok(vec![]),
        };
        let err = resolve(&runner, "default", "default").unwrap_err();
        assert!(matches!(err, DeviceError::NodeNameMissing { .. }));
    }

    #[test]
    fn wpctl_failure_propagates() {
        let runner = MockRunner {
            source_body: Err(DeviceError::WpctlFailed {
                message: "exit 1".into(),
            }),
            sink_body: Ok(SINK_FIXTURE.to_owned()),
            node_names: Ok(vec![]),
        };
        let err = resolve(&runner, "default", "default").unwrap_err();
        assert!(matches!(err, DeviceError::WpctlFailed { .. }));
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
    fn explicit_monitor_accepts_parent_sink_with_monitor_suffix() {
        let runner = MockRunner {
            source_body: Ok(SOURCE_FIXTURE.to_owned()),
            sink_body: Ok(SINK_FIXTURE.to_owned()),
            node_names: Ok(vec![
                "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo".to_owned(),
                "my.real.sink".to_owned(),
            ]),
        };
        let selection = resolve(&runner, "default", "my.real.sink.monitor").unwrap();
        assert_eq!(selection.monitor_node, "my.real.sink.monitor");
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
