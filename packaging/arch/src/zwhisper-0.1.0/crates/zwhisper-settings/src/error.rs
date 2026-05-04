//! M7 — top-level error type for the settings binary.
//!
//! Each tab area has its own variant so error context survives the
//! `?` boundary at module edges. Variants are intentionally minimal
//! at A-stage: Group B/C/D will add `From` impls (and may split
//! variants further) when their concrete error types stabilise. The
//! `String` payloads here let the tabs surface error messages
//! verbatim to the UI without committing to a wire shape that later
//! parties cannot change.
//!
//! See M7-plan § 7.1 ("Error model").

use std::io;

/// All non-`io::Error` errors crossing module boundaries inside
/// `zwhisper-settings` go through this enum. The variants line up
/// with the four tabs plus the in-process model downloader and the
/// shared `models.toml` parser.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // Variants populated by Group B/C/D.
pub(crate) enum SettingsError {
    /// Profile editor errors — wraps validation, atomic-write, and
    /// `Profiles1.reload` failures.
    #[error("profile editor: {0}")]
    Profile(String),

    /// Model downloader errors — HTTP, SHA mismatch, rename failure.
    #[error("model download: {0}")]
    Download(String),

    /// Hotkey tab errors — portal bind / unbind failure or timeout.
    #[error("hotkey portal: {0}")]
    Hotkey(String),

    /// Whisper-cli detector errors — no binary found, or several
    /// candidates with no policy to pick one.
    #[error("whisper-cli discovery: {0}")]
    Discovery(String),

    /// `~/.config/zwhisper/models.toml` parse / IO errors.
    #[error("models config: {0}")]
    Config(String),

    /// Filesystem errors that bubble through `?` from
    /// `tokio::fs` / `std::fs` / `tempfile`. Promoted via `From`
    /// so call sites can stay terse.
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn error_chain_via_from() {
        // `?` on a `std::io::Error` should produce a `SettingsError`
        // without losing the source — the From impl is the only
        // contract the rest of the crate relies on.
        let raw = io::Error::new(io::ErrorKind::NotFound, "nope");
        let wrapped: SettingsError = raw.into();
        match wrapped {
            SettingsError::Io(inner) => {
                assert_eq!(inner.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn display_includes_variant_payload() {
        let e = SettingsError::Profile("validation failed: empty name".into());
        let rendered = format!("{e}");
        assert!(rendered.contains("profile editor"), "{rendered}");
        assert!(rendered.contains("empty name"), "{rendered}");
    }
}
