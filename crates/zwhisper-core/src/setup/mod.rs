//! Guided microphone setup & calibration — core layer (RFC-mic-setup,
//! Phase 0).
//!
//! Everything the wizard needs — enumerate devices, read/set volume, set
//! the default source, and (CLI-side) meter raw PCM — is a *shell-out +
//! parse* problem against `pw-dump`, `wpctl`, and `pw-cat`. None of it
//! needs GStreamer, so this module sits behind the GStreamer-free
//! `setup` feature and the thin CLI can depend on it directly.
//!
//! All tooling goes through the `PipewireControl` trait so the analysis
//! is unit-testable with a `MockPipewire` (canned dumps and volume
//! strings) — no `PipeWire` daemon required. The production
//! `SystemPipewire` implements the trait via [`std::process::Command`]
//! with **no shell** (every argument is a separate `.arg(...)`),
//! numeric-id validation, finite/clamped volumes, and a size-capped
//! `pw-dump` read, per the RFC's security invariants.

pub mod config;
pub mod devices;
pub mod level;
pub mod volume;

pub use config::SetupConfig;
pub use devices::{AudioDevice, RawNode, build_devices, parse_dump};
pub use level::{LevelStats, analyze, recommend_volume, within_tolerance};
pub use volume::Volume;

use std::io::Read;
use std::process::Command;

use super::node_name;

/// Errors raised by the setup / calibration layer. One variant per
/// failure class so the CLI can dispatch without parsing display
/// strings.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// A `pw-dump` / `wpctl` invocation failed to spawn or exited
    /// non-zero. `tool` names the binary so diagnostics point at the
    /// right one.
    #[error("`{tool}` failed: {message}")]
    CommandFailed { tool: &'static str, message: String },

    /// External output (JSON, a volume line, a node name) could not be
    /// parsed. `what` labels the source.
    #[error("could not parse {what}: {message}")]
    Parse { what: &'static str, message: String },

    /// A user-supplied device selector (id / node name) was malformed.
    #[error("invalid device selector `{value}`: {reason}")]
    InvalidSelector { value: String, reason: &'static str },

    /// A metering capture exceeded its time budget.
    #[error("metering timed out after {seconds}s")]
    Timeout { seconds: u64 },

    /// The mic was still below the target peak at the volume cap — the
    /// calibration loop reports this instead of looping forever.
    #[error(
        "microphone too quiet: peak {measured_db:.1} dBFS still below target {target_db:.1} dBFS \
         at max volume {max_volume:.2}"
    )]
    TooQuiet {
        measured_db: f32,
        target_db: f32,
        max_volume: f32,
    },

    /// An I/O error reading a child's output, etc.
    #[error("i/o error: {0}")]
    Io(String),
}

/// Mockable indirection over the `PipeWire` CLI tools. Production wires
/// up [`SystemPipewire`]; tests use a `MockPipewire`.
pub trait PipewireControl: std::fmt::Debug + Send + Sync {
    /// `pw-dump` parsed to the audio nodes we care about.
    fn dump_nodes(&self) -> Result<Vec<RawNode>, SetupError>;
    /// `wpctl inspect @DEFAULT_AUDIO_SOURCE@` → its `node.name` (used to
    /// flag the default device).
    fn default_source_name(&self) -> Result<String, SetupError>;
    /// `wpctl get-volume <id>` → parsed [`Volume`].
    fn get_volume(&self, id: u32) -> Result<Volume, SetupError>;
    /// `wpctl set-volume <id> <linear>` with a clamped, finite value.
    fn set_volume(&self, id: u32, linear: f32) -> Result<(), SetupError>;
    /// `wpctl set-default <id>` — mutates global state; the CLI gates
    /// this behind an explicit flag / wizard confirmation.
    fn set_default(&self, id: u32) -> Result<(), SetupError>;
}

/// `wpctl` alias for the current default audio source.
const DEFAULT_SOURCE_ALIAS: &str = "@DEFAULT_AUDIO_SOURCE@";

/// Production [`PipewireControl`] backed by `pw-dump` / `wpctl` via
/// [`std::process::Command`] (no shell). Holds a [`SetupConfig`] for the
/// `pw-dump` size cap and the set-volume clamp bounds.
///
/// `Default` uses [`SetupConfig::default`] (the RFC-tuned values).
#[derive(Debug, Default)]
pub struct SystemPipewire {
    config: SetupConfig,
}

impl SystemPipewire {
    /// Construct with an explicit [`SetupConfig`] (size caps, clamp
    /// bounds). Use [`SystemPipewire::default`] for the RFC defaults.
    pub fn new(config: SetupConfig) -> Self {
        Self { config }
    }
}

/// Run a `wpctl` argv and return its trimmed stdout, mapping a spawn
/// failure or non-zero exit to a typed [`SetupError::CommandFailed`].
/// No shell: every token is a separate argv element.
fn run_wpctl(args: &[&str]) -> Result<String, SetupError> {
    let output =
        Command::new("wpctl")
            .args(args)
            .output()
            .map_err(|e| SetupError::CommandFailed {
                tool: "wpctl",
                message: format!("could not spawn `wpctl {}`: {e}", args.join(" ")),
            })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SetupError::CommandFailed {
            tool: "wpctl",
            message: format!(
                "`wpctl {}` exited with status {:?}: {stderr}",
                args.join(" "),
                output.status.code()
            ),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Extract `node.name = "<value>"` from a `wpctl inspect` body, tolerant
/// of the starred (`* node.name = …`) and plain forms, and skipping
/// `node.name.fallback`. Ported from `audio/devices.rs` so the `setup`
/// module does not depend on the GStreamer-gated `audio` module.
fn parse_inspect_node_name(body: &str) -> Result<String, SetupError> {
    for line in body.lines() {
        let trimmed = line.trim_start_matches([' ', '*', '\t']);
        if let Some(rest) = trimmed.strip_prefix("node.name") {
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
    Err(SetupError::Parse {
        what: "wpctl inspect node.name",
        message: format!("no `node.name = \"…\"` line in output:\n{body}"),
    })
}

impl PipewireControl for SystemPipewire {
    fn dump_nodes(&self) -> Result<Vec<RawNode>, SetupError> {
        // Spawn pw-dump with stdout piped and read it through a bounded
        // adapter: a hostile / runaway dump must not be buffered without
        // limit. We read up to `max_pw_dump_bytes + 1`; if the extra
        // byte materialises the dump is over the cap and we reject it
        // (and kill the child) rather than parse a truncated body.
        let mut child = Command::new("pw-dump")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| SetupError::CommandFailed {
                tool: "pw-dump",
                message: format!("could not spawn `pw-dump`: {e}"),
            })?;

        let cap = self.config.max_pw_dump_bytes;
        let mut buf = Vec::with_capacity(cap.min(64 * 1024));
        // `take(cap as u64 + 1)` so we can distinguish "exactly cap" from
        // "more than cap"; `+1` cannot overflow usize→u64 on supported
        // targets but saturate defensively anyway.
        let limit = (cap as u64).saturating_add(1);
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SetupError::Io(
                "pw-dump produced no stdout handle".to_owned(),
            ));
        };

        if let Err(e) = stdout.take(limit).read_to_end(&mut buf) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SetupError::Io(format!("reading pw-dump stdout: {e}")));
        }

        if buf.len() as u64 > cap as u64 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SetupError::CommandFailed {
                tool: "pw-dump",
                message: format!("output exceeded the {cap}-byte cap; refusing to parse"),
            });
        }

        // Reap the child and check its exit status.
        let status = child.wait().map_err(|e| SetupError::CommandFailed {
            tool: "pw-dump",
            message: format!("waiting on `pw-dump`: {e}"),
        })?;
        if !status.success() {
            return Err(SetupError::CommandFailed {
                tool: "pw-dump",
                message: format!("`pw-dump` exited with status {:?}", status.code()),
            });
        }

        let json = String::from_utf8_lossy(&buf);
        parse_dump(&json)
    }

    fn default_source_name(&self) -> Result<String, SetupError> {
        let body = run_wpctl(&["inspect", DEFAULT_SOURCE_ALIAS])?;
        if body.trim().is_empty() || body.contains("not found") {
            return Err(SetupError::CommandFailed {
                tool: "wpctl",
                message: format!("`wpctl inspect {DEFAULT_SOURCE_ALIAS}` returned no node: {body}"),
            });
        }
        parse_inspect_node_name(&body)
    }

    fn get_volume(&self, id: u32) -> Result<Volume, SetupError> {
        // `id` is a u32 formatted with `{}` — purely numeric, so it can
        // never be a flag or injection vector even though argv is used.
        let id_str = id.to_string();
        let body = run_wpctl(&["get-volume", &id_str])?;
        volume::parse_volume(&body)
    }

    fn set_volume(&self, id: u32, linear: f32) -> Result<(), SetupError> {
        // Clamp + finiteness-check BEFORE the value can reach wpctl: a
        // NaN/inf/negative/over-cap volume is never sent. The cap comes
        // from the config so it matches the recommender's bounds.
        if !linear.is_finite() {
            return Err(SetupError::InvalidSelector {
                value: linear.to_string(),
                reason: "volume must be finite",
            });
        }
        let clamped = linear.clamp(self.config.min_volume, self.config.max_volume);
        let id_str = id.to_string();
        let vol_str = volume::format_linear(clamped);
        run_wpctl(&["set-volume", &id_str, &vol_str]).map(|_| ())
    }

    fn set_default(&self, id: u32) -> Result<(), SetupError> {
        let id_str = id.to_string();
        run_wpctl(&["set-default", &id_str]).map(|_| ())
    }
}

/// Validate a user-supplied device selector string and resolve it to a
/// node id against a device list.
///
/// Resolution (per the RFC `<sel>` grammar):
/// - `"default"` → the id of the device flagged `is_default`;
/// - a bare integer → used directly as the id (validated purely
///   numeric, so it cannot be a flag);
/// - anything else → treated as a `node.name`, validated against the
///   shared allow-list, then looked up in `devices`.
///
/// Kept here (not in the CLI) so selector parsing has one tested home.
pub fn resolve_selector(selector: &str, devices: &[AudioDevice]) -> Result<u32, SetupError> {
    let sel = selector.trim();
    if sel.is_empty() {
        return Err(SetupError::InvalidSelector {
            value: selector.to_owned(),
            reason: "empty selector",
        });
    }

    if sel == "default" {
        return devices.iter().find(|d| d.is_default).map(|d| d.id).ok_or(
            SetupError::InvalidSelector {
                value: selector.to_owned(),
                reason: "no default source is set",
            },
        );
    }

    // A bare unsigned integer is a raw object id.
    if sel.chars().all(|c| c.is_ascii_digit()) {
        return sel.parse::<u32>().map_err(|_| SetupError::InvalidSelector {
            value: selector.to_owned(),
            reason: "numeric id does not fit in u32",
        });
    }

    // Otherwise it must be a valid node.name we can find in the dump.
    node_name::validate_node_name(sel).map_err(|e| SetupError::InvalidSelector {
        value: selector.to_owned(),
        reason: e.reason(),
    })?;
    devices
        .iter()
        .find(|d| d.node_name == sel)
        .map(|d| d.id)
        .ok_or(SetupError::InvalidSelector {
            value: selector.to_owned(),
            reason: "node name not found among audio devices",
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Canned [`PipewireControl`] for unit tests: fixed dump / default /
    /// volume, and a recorded log of every `set_*` call so tests can
    /// assert the calibration loop's side effects without a daemon.
    #[derive(Debug)]
    struct MockPipewire {
        nodes: Vec<RawNode>,
        default_name: String,
        volume: Volume,
        set_volume_calls: Mutex<Vec<(u32, f32)>>,
        set_default_calls: Mutex<Vec<u32>>,
    }

    impl MockPipewire {
        fn new() -> Self {
            Self {
                nodes: vec![
                    RawNode {
                        id: 68,
                        node_name: "alsa_input.mic".to_owned(),
                        description: "Built-in Mic".to_owned(),
                        media_class: "Audio/Source".to_owned(),
                        serial: Some(10),
                    },
                    RawNode {
                        id: 70,
                        node_name: "alsa_output.spk".to_owned(),
                        description: "Speakers".to_owned(),
                        media_class: "Audio/Sink".to_owned(),
                        serial: None,
                    },
                ],
                default_name: "alsa_input.mic".to_owned(),
                volume: Volume {
                    linear: 0.25,
                    muted: false,
                },
                set_volume_calls: Mutex::new(Vec::new()),
                set_default_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl PipewireControl for MockPipewire {
        fn dump_nodes(&self) -> Result<Vec<RawNode>, SetupError> {
            Ok(self.nodes.clone())
        }
        fn default_source_name(&self) -> Result<String, SetupError> {
            Ok(self.default_name.clone())
        }
        fn get_volume(&self, _id: u32) -> Result<Volume, SetupError> {
            Ok(self.volume)
        }
        fn set_volume(&self, id: u32, linear: f32) -> Result<(), SetupError> {
            self.set_volume_calls.lock().unwrap().push((id, linear));
            Ok(())
        }
        fn set_default(&self, id: u32) -> Result<(), SetupError> {
            self.set_default_calls.lock().unwrap().push(id);
            Ok(())
        }
    }

    #[test]
    fn mock_round_trips_through_build_devices() {
        let mock = MockPipewire::new();
        let raw = mock.dump_nodes().unwrap();
        let default = mock.default_source_name().unwrap();
        let devices = build_devices(&raw, &default);
        let mic = devices.iter().find(|d| d.id == 68).unwrap();
        assert!(mic.is_source && mic.is_default);
    }

    #[test]
    fn mock_records_set_calls() {
        let mock = MockPipewire::new();
        mock.set_volume(68, 0.42).unwrap();
        mock.set_default(68).unwrap();
        assert_eq!(*mock.set_volume_calls.lock().unwrap(), vec![(68, 0.42)]);
        assert_eq!(*mock.set_default_calls.lock().unwrap(), vec![68]);
    }

    fn devices_fixture() -> Vec<AudioDevice> {
        let mock = MockPipewire::new();
        let raw = mock.dump_nodes().unwrap();
        build_devices(&raw, &mock.default_name)
    }

    #[test]
    fn resolve_selector_default_picks_default_device() {
        let devices = devices_fixture();
        assert_eq!(resolve_selector("default", &devices).unwrap(), 68);
    }

    #[test]
    fn resolve_selector_numeric_id_passes_through() {
        let devices = devices_fixture();
        assert_eq!(resolve_selector("70", &devices).unwrap(), 70);
        // An id need not exist in the list — it is used directly.
        assert_eq!(resolve_selector("12345", &devices).unwrap(), 12345);
    }

    #[test]
    fn resolve_selector_node_name_looks_up_id() {
        let devices = devices_fixture();
        assert_eq!(resolve_selector("alsa_output.spk", &devices).unwrap(), 70);
    }

    #[test]
    fn resolve_selector_rejects_unknown_node_name() {
        let devices = devices_fixture();
        let err = resolve_selector("nope.node", &devices).unwrap_err();
        assert!(matches!(err, SetupError::InvalidSelector { .. }));
    }

    #[test]
    fn resolve_selector_rejects_malicious_node_name_before_lookup() {
        let devices = devices_fixture();
        // A name with DSL/shell metacharacters fails the allow-list, so
        // it never reaches a lookup or a command.
        for bad in ["a b", "a!b", "a;rm -rf", "$(x)"] {
            let err = resolve_selector(bad, &devices).unwrap_err();
            assert!(matches!(err, SetupError::InvalidSelector { .. }), "{bad}");
        }
    }

    #[test]
    fn resolve_selector_rejects_empty_and_default_when_none() {
        let devices = devices_fixture();
        assert!(matches!(
            resolve_selector("   ", &devices).unwrap_err(),
            SetupError::InvalidSelector { .. }
        ));

        // No default flagged → "default" selector errors rather than
        // guessing.
        let no_default: Vec<AudioDevice> = devices
            .into_iter()
            .map(|mut d| {
                d.is_default = false;
                d
            })
            .collect();
        assert!(matches!(
            resolve_selector("default", &no_default).unwrap_err(),
            SetupError::InvalidSelector { .. }
        ));
    }

    #[test]
    fn parse_inspect_node_name_handles_starred_and_plain() {
        assert_eq!(
            parse_inspect_node_name("id 68\n  * node.name = \"starred\"\n").unwrap(),
            "starred"
        );
        assert_eq!(
            parse_inspect_node_name("id 68\n    node.name = \"plain\"\n").unwrap(),
            "plain"
        );
    }

    #[test]
    fn parse_inspect_node_name_skips_fallback_and_errors_when_absent() {
        // `node.name.fallback` must not be grabbed; the real name wins.
        assert_eq!(
            parse_inspect_node_name("node.name.fallback = \"skip\"\n* node.name = \"real\"\n")
                .unwrap(),
            "real"
        );
        let err = parse_inspect_node_name("id 1\n  some.prop = \"x\"\n").unwrap_err();
        assert!(matches!(err, SetupError::Parse { .. }));
    }

    #[test]
    fn setup_error_messages_are_actionable() {
        let too_quiet = SetupError::TooQuiet {
            measured_db: -20.0,
            target_db: -7.5,
            max_volume: 1.0,
        };
        let msg = too_quiet.to_string();
        assert!(msg.contains("too quiet"), "{msg}");
        assert!(msg.contains("-20.0"), "{msg}");

        let timeout = SetupError::Timeout { seconds: 10 };
        assert!(timeout.to_string().contains("10s"));
    }
}
