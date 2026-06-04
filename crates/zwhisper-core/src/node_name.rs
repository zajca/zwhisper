//! Single source of truth for `PipeWire` node-name validation.
//!
//! `pipewiresrc target-object=…` and `pw-cat --target …` consume raw
//! `node.name` strings, and the `gst-launch` DSL grammar (parsed by
//! `gst::parse::launch`) treats `!`, `.`, `=`, `,`, `(`, `)` and quotes
//! as syntactically meaningful. A malicious or malformed name must not
//! be able to inject pipeline elements or shell-style arguments, and it
//! must not reach a TOML write either. We accept exactly the character
//! set seen on real `PipeWire` nodes — alphanumerics, dots, underscores,
//! hyphens, and `:` (media-class qualifiers) — and nothing else.
//!
//! Both the GStreamer device resolver (`audio/devices.rs`) and the
//! `setup` calibration / profile writer delegate here so the rule has
//! one home. The error carries a `'static` reason string that matches
//! the historical `audio/devices.rs` messages verbatim, so callers that
//! surface those strings keep their existing behaviour.

/// Maximum length of a `PipeWire` node name we will accept. Real names
/// observed on Arch are well under 100 chars; this just keeps us safe
/// from runaway inputs before they reach a command argv or a TOML write.
pub(crate) const MAX_NODE_NAME_LEN: usize = 256;

/// Why a candidate `node.name` was rejected. `Copy` because it carries
/// no owned data — the offending value stays with the caller, which has
/// richer context (which field, the original untrimmed string) for its
/// own error type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeNameError {
    /// The (trimmed) name was empty.
    Empty,
    /// The name exceeded [`MAX_NODE_NAME_LEN`].
    TooLong,
    /// The name contained characters outside `[A-Za-z0-9._:-]`.
    InvalidChars,
}

impl NodeNameError {
    /// A stable, human-readable reason. The exact strings match the
    /// historical `audio/devices.rs` messages so existing error
    /// assertions (and user-facing diagnostics) stay unchanged.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty value",
            Self::TooLong => "node name exceeds 256 characters",
            Self::InvalidChars => "node names must match [A-Za-z0-9._:-]+",
        }
    }
}

/// Allow-list validation for a `PipeWire` node name. `name` is expected
/// to be already trimmed by the caller — leading/trailing whitespace
/// counts as [`NodeNameError::InvalidChars`] here, matching the
/// allow-list (space is not in the set), so an un-trimmed name is still
/// rejected rather than silently accepted.
pub(crate) fn validate_node_name(name: &str) -> Result<(), NodeNameError> {
    if name.is_empty() {
        return Err(NodeNameError::Empty);
    }
    if name.len() > MAX_NODE_NAME_LEN {
        return Err(NodeNameError::TooLong);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
    {
        return Err(NodeNameError::InvalidChars);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_world_node_names() {
        for ok in [
            "alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo",
            "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor",
            "my.mic.node",
            "Audio:Source",
            "node-1",
            "a",
        ] {
            assert_eq!(validate_node_name(ok), Ok(()), "{ok} should be accepted");
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(validate_node_name(""), Err(NodeNameError::Empty));
        assert_eq!(NodeNameError::Empty.reason(), "empty value");
    }

    #[test]
    fn rejects_too_long() {
        let long = "a".repeat(MAX_NODE_NAME_LEN + 1);
        assert_eq!(validate_node_name(&long), Err(NodeNameError::TooLong));
        // Exactly the cap is accepted.
        let at_cap = "a".repeat(MAX_NODE_NAME_LEN);
        assert_eq!(validate_node_name(&at_cap), Ok(()));
        assert_eq!(
            NodeNameError::TooLong.reason(),
            "node name exceeds 256 characters"
        );
    }

    #[test]
    fn rejects_dsl_metacharacters_and_whitespace() {
        for bad in [
            "has space",
            "node!name",
            "a,b",
            "a=b",
            "a(b)",
            "a!b.c",
            "quote\"name",
            "new\nline",
            "tab\tname",
        ] {
            assert_eq!(
                validate_node_name(bad),
                Err(NodeNameError::InvalidChars),
                "{bad:?} should be rejected"
            );
        }
        assert_eq!(
            NodeNameError::InvalidChars.reason(),
            "node names must match [A-Za-z0-9._:-]+"
        );
    }
}
