//! `pw-dump` JSON → audio-device list.
//!
//! `pw-dump` emits a JSON array of PipeWire objects. We care only about
//! `PipeWire:Interface:Node` objects that carry a `node.name` and a
//! `media.class`; everything else (factories, ports, links, devices,
//! clients) is ignored. The parse is **tolerant** — `info` / `props`
//! may be absent, and unknown fields are ignored by serde — but
//! **strict** on the two fields we actually use (`id`, `node.name`)
//! plus `media.class` for classification.
//!
//! [`parse_dump`] returns the raw nodes; [`build_devices`] filters them
//! to the `Audio/Source` + `Audio/Sink` devices the wizard offers and
//! cross-references the default source name for the `is_default` flag.

use serde::Deserialize;

use super::SetupError;
use super::volume::Volume;

/// `media.class` of a capture (microphone) node.
const MEDIA_CLASS_SOURCE: &str = "Audio/Source";
/// `media.class` of a playback (speaker/headphone) node.
const MEDIA_CLASS_SINK: &str = "Audio/Sink";
/// Suffix that marks a sink-monitor source (loopback of a sink).
const MONITOR_SUFFIX: &str = ".monitor";
/// The PipeWire object type we enumerate.
const NODE_TYPE: &str = "PipeWire:Interface:Node";

/// A PipeWire node as parsed from `pw-dump`, before audio-device
/// filtering. Carries exactly the fields the wizard needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawNode {
    /// `object.id` — required by `wpctl` and `pw-cat --target`.
    pub id: u32,
    /// `node.name` — the canonical name `pipewiresrc target-object`
    /// consumes.
    pub node_name: String,
    /// `node.description` — the human-readable label; empty when the
    /// node did not advertise one.
    pub description: String,
    /// `media.class`, e.g. `Audio/Source` / `Audio/Sink`.
    pub media_class: String,
    /// `object.serial` when present (monotonic per-object id; absent on
    /// some nodes, hence `Option`).
    pub serial: Option<u64>,
}

/// An audio device the wizard can offer: a microphone (`Audio/Source`)
/// or a speaker/headphone (`Audio/Sink`), with the flags the UI renders.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioDevice {
    /// `object.id` — required by `wpctl` + `pw-cat --target`.
    pub id: u32,
    /// `node.name` — required by `pipewiresrc target-object`.
    pub node_name: String,
    /// `node.description` — what the user reads.
    pub description: String,
    /// `media.class == "Audio/Source"` (vs a sink).
    pub is_source: bool,
    /// `node.name` ends with `.monitor` (a sink-monitor source).
    pub is_monitor: bool,
    /// `node.name == default_source_name` passed to [`build_devices`].
    pub is_default: bool,
    /// Current volume. Left `None` by this pure layer; the CLI enriches
    /// it via `PipewireControl::get_volume`.
    pub volume: Option<Volume>,
}

// ---- serde shapes for the tolerant pw-dump parse ----------------------

/// One top-level `pw-dump` array element. `info`/`props` are optional;
/// unknown keys (e.g. `version`, `permissions`) are ignored.
#[derive(Debug, Deserialize)]
struct DumpObject {
    #[serde(default)]
    id: Option<u32>,
    #[serde(rename = "type", default)]
    object_type: Option<String>,
    #[serde(default)]
    info: Option<DumpInfo>,
}

#[derive(Debug, Deserialize)]
struct DumpInfo {
    #[serde(default)]
    props: Option<DumpProps>,
}

#[derive(Debug, Deserialize)]
struct DumpProps {
    #[serde(rename = "node.name", default)]
    node_name: Option<String>,
    #[serde(rename = "node.description", default)]
    node_description: Option<String>,
    #[serde(rename = "media.class", default)]
    media_class: Option<String>,
    #[serde(rename = "object.serial", default)]
    object_serial: Option<u64>,
}

/// Parse a `pw-dump` JSON body into the audio nodes we recognise.
///
/// Returns every `PipeWire:Interface:Node` that has both a `node.name`
/// and a `media.class`. Objects of any other `type`, and nodes missing
/// either required prop (or the `info`/`props` block entirely), are
/// skipped silently — a partial dump still yields the usable nodes. A
/// body that is not a JSON array is a typed [`SetupError::Parse`].
pub fn parse_dump(json: &str) -> Result<Vec<RawNode>, SetupError> {
    let objects: Vec<DumpObject> = serde_json::from_str(json).map_err(|e| SetupError::Parse {
        what: "pw-dump JSON",
        message: e.to_string(),
    })?;

    let mut out = Vec::new();
    for obj in objects {
        // Only PipeWire nodes; ignore ports/links/factories/etc. A
        // missing `type` is treated as "not a node we care about".
        if obj.object_type.as_deref() != Some(NODE_TYPE) {
            continue;
        }
        let Some(id) = obj.id else {
            continue;
        };
        let Some(props) = obj.info.and_then(|i| i.props) else {
            continue;
        };
        // Required fields: a node without a name or media class is not
        // something we can target or classify.
        let (Some(node_name), Some(media_class)) = (props.node_name, props.media_class) else {
            continue;
        };
        if node_name.is_empty() {
            continue;
        }
        out.push(RawNode {
            id,
            node_name,
            description: props.node_description.unwrap_or_default(),
            media_class,
            serial: props.object_serial,
        });
    }
    Ok(out)
}

/// Filter raw nodes to the audio **devices** (`Audio/Source` +
/// `Audio/Sink`) and set the UI flags.
///
/// `default_source_name` is the `node.name` of the current default
/// source (from `PipewireControl::default_source_name`); the device
/// whose name matches gets `is_default = true`. `Stream/*`, `Video/*`,
/// `Midi/*` and any other class are excluded — they are application
/// streams or non-audio nodes, not selectable capture/playback devices.
pub fn build_devices(raw: &[RawNode], default_source_name: &str) -> Vec<AudioDevice> {
    raw.iter()
        .filter(|n| n.media_class == MEDIA_CLASS_SOURCE || n.media_class == MEDIA_CLASS_SINK)
        .map(|n| AudioDevice {
            id: n.id,
            node_name: n.node_name.clone(),
            description: n.description.clone(),
            is_source: n.media_class == MEDIA_CLASS_SOURCE,
            is_monitor: n.node_name.ends_with(MONITOR_SUFFIX),
            is_default: n.node_name == default_source_name,
            volume: None,
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A representative `pw-dump` slice: a real mic, a sink, a stream
    /// (must be excluded), a video source (excluded), a node missing
    /// its props (skipped), and a synthetic `.monitor` source.
    const DUMP: &str = r#"
[
  {
    "id": 68,
    "type": "PipeWire:Interface:Node",
    "info": {
      "props": {
        "node.name": "alsa_input.pci-0000_00_1f.3.analog-stereo",
        "node.description": "Built-in Audio Analog Stereo",
        "media.class": "Audio/Source",
        "object.serial": 1234
      }
    }
  },
  {
    "id": 70,
    "type": "PipeWire:Interface:Node",
    "info": {
      "props": {
        "node.name": "alsa_output.pci-0000_00_1f.3.analog-stereo",
        "node.description": "Built-in Audio Analog Stereo Output",
        "media.class": "Audio/Sink"
      }
    }
  },
  {
    "id": 90,
    "type": "PipeWire:Interface:Node",
    "info": {
      "props": {
        "node.name": "my-app-capture",
        "media.class": "Stream/Input/Audio"
      }
    }
  },
  {
    "id": 91,
    "type": "PipeWire:Interface:Node",
    "info": {
      "props": {
        "node.name": "webcam",
        "media.class": "Video/Source"
      }
    }
  },
  {
    "id": 92,
    "type": "PipeWire:Interface:Node",
    "info": {}
  },
  {
    "id": 93,
    "type": "PipeWire:Interface:Node",
    "info": {
      "props": {
        "node.name": "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor",
        "node.description": "Monitor of Built-in Audio",
        "media.class": "Audio/Source"
      }
    }
  },
  {
    "id": 99,
    "type": "PipeWire:Interface:Factory",
    "info": {
      "props": { "factory.name": "client-node" }
    }
  }
]
"#;

    #[test]
    fn parse_dump_keeps_only_named_nodes() {
        let nodes = parse_dump(DUMP).unwrap();
        // mic, sink, stream, video, monitor = 5 nodes with name+class;
        // the props-less node (id 92) and the Factory (id 99) are gone.
        let ids: Vec<u32> = nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![68, 70, 90, 91, 93], "{nodes:#?}");
    }

    #[test]
    fn parse_dump_extracts_fields_including_optional_serial() {
        let nodes = parse_dump(DUMP).unwrap();
        let mic = nodes.iter().find(|n| n.id == 68).unwrap();
        assert_eq!(mic.node_name, "alsa_input.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(mic.description, "Built-in Audio Analog Stereo");
        assert_eq!(mic.media_class, "Audio/Source");
        assert_eq!(mic.serial, Some(1234));

        // Sink advertised no object.serial → None; description present.
        let sink = nodes.iter().find(|n| n.id == 70).unwrap();
        assert_eq!(sink.serial, None);
        assert_eq!(sink.media_class, "Audio/Sink");
    }

    #[test]
    fn build_devices_includes_only_audio_source_and_sink() {
        let nodes = parse_dump(DUMP).unwrap();
        let devices = build_devices(&nodes, "alsa_input.pci-0000_00_1f.3.analog-stereo");
        let ids: Vec<u32> = devices.iter().map(|d| d.id).collect();
        // Stream (90) and Video (91) excluded; mic (68), sink (70),
        // monitor source (93) kept.
        assert_eq!(ids, vec![68, 70, 93], "{devices:#?}");
    }

    #[test]
    fn build_devices_sets_source_monitor_default_flags() {
        let nodes = parse_dump(DUMP).unwrap();
        let devices = build_devices(&nodes, "alsa_input.pci-0000_00_1f.3.analog-stereo");

        let mic = devices.iter().find(|d| d.id == 68).unwrap();
        assert!(mic.is_source);
        assert!(!mic.is_monitor);
        assert!(mic.is_default);
        assert!(mic.volume.is_none());

        let sink = devices.iter().find(|d| d.id == 70).unwrap();
        assert!(!sink.is_source);
        assert!(!sink.is_monitor);
        assert!(!sink.is_default);

        let monitor = devices.iter().find(|d| d.id == 93).unwrap();
        // A `.monitor` is published as an Audio/Source here.
        assert!(monitor.is_source);
        assert!(monitor.is_monitor);
        assert!(!monitor.is_default);
    }

    #[test]
    fn build_devices_marks_no_default_when_name_absent() {
        let nodes = parse_dump(DUMP).unwrap();
        let devices = build_devices(&nodes, "some.other.node");
        assert!(devices.iter().all(|d| !d.is_default));
    }

    #[test]
    fn parse_dump_empty_array_yields_no_nodes() {
        assert!(parse_dump("[]").unwrap().is_empty());
        // Whitespace-only array too.
        assert!(parse_dump("  [ ]  ").unwrap().is_empty());
    }

    #[test]
    fn parse_dump_tolerates_unknown_top_level_and_prop_keys() {
        let json = r#"
        [
          {
            "id": 1,
            "type": "PipeWire:Interface:Node",
            "version": 3,
            "permissions": ["r", "w"],
            "info": {
              "max-input-ports": 64,
              "props": {
                "node.name": "n1",
                "media.class": "Audio/Source",
                "some.unknown.prop": "ignored",
                "factory.id": 17
              }
            }
          }
        ]
        "#;
        let nodes = parse_dump(json).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_name, "n1");
        assert_eq!(nodes[0].description, ""); // absent → empty
    }

    #[test]
    fn parse_dump_skips_node_missing_media_class() {
        let json =
            r#"[{"id":1,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"n1"}}}]"#;
        assert!(parse_dump(json).unwrap().is_empty());
    }

    #[test]
    fn parse_dump_skips_node_with_empty_name() {
        let json = r#"[{"id":1,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"","media.class":"Audio/Source"}}}]"#;
        assert!(parse_dump(json).unwrap().is_empty());
    }

    #[test]
    fn parse_dump_rejects_non_array_json() {
        for bad in ["{}", "\"a string\"", "42", "not json at all", ""] {
            let err = parse_dump(bad).unwrap_err();
            assert!(matches!(err, SetupError::Parse { .. }), "{bad:?}");
        }
    }
}
