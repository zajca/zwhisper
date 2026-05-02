use std::path::PathBuf;

use super::error::ProfileError;

const SHIPPED_FALLBACK: &str = "/usr/share/zwhisper/profiles";

/// `[A-Za-z0-9._-]+` — matches IDEA.md § 6 spec and rejects path
/// traversal (`/`, `..` would either contain `/` or be dot-only),
/// shell metacharacters, and whitespace before any I/O.
pub(crate) fn validate_name(name: &str) -> Result<(), ProfileError> {
    if name.is_empty() {
        return Err(ProfileError::InvalidName { name: name.into() });
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    let only_dots = name.chars().all(|c| c == '.');
    if !ok || only_dots {
        return Err(ProfileError::InvalidName { name: name.into() });
    }
    Ok(())
}

/// `${XDG_CONFIG_HOME:-~/.config}/zwhisper/profiles/<name>.toml`.
pub(crate) fn user_override_path(name: &str) -> Result<PathBuf, ProfileError> {
    validate_name(name)?;
    let base = dirs::config_dir().ok_or_else(|| ProfileError::Validation {
        profile: name.into(),
        message: "no XDG config dir resolvable for the current user".into(),
    })?;
    Ok(base.join("zwhisper/profiles").join(format!("{name}.toml")))
}

/// `${ZWHISPER_DATA_DIR:-/usr/share/zwhisper}/profiles/<name>.toml`.
///
/// Resolved at runtime so test isolation (`env_remove`) works and so
/// distro packagers can override the data dir at install time without
/// rebuilding. The compile-time `option_env!` flavour was the M2
/// review's Low finding: it made the integration tests
/// host-dependent.
pub(crate) fn shipped_path(name: &str) -> Result<PathBuf, ProfileError> {
    validate_name(name)?;
    let root = std::env::var_os("ZWHISPER_DATA_DIR")
        .map_or_else(|| PathBuf::from(SHIPPED_FALLBACK), PathBuf::from);
    Ok(root.join("profiles").join(format!("{name}.toml")))
}

/// Directory that hosts user-override profiles (without the filename).
pub(crate) fn user_profiles_dir() -> Result<PathBuf, ProfileError> {
    let base = dirs::config_dir().ok_or_else(|| ProfileError::Validation {
        profile: String::new(),
        message: "no XDG config dir resolvable for the current user".into(),
    })?;
    Ok(base.join("zwhisper/profiles"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_accepts_normal_identifiers() {
        for ok in [
            "meeting",
            "voicememo",
            "my-meeting",
            "team_call",
            "v1.0",
            "default",
        ] {
            validate_name(ok).expect(ok);
        }
    }

    #[test]
    fn validate_name_rejects_traversal_and_separators() {
        for bad in [
            "../etc/passwd",
            "meeting/extra",
            "meeting with spaces",
            "name!",
            "..",
            ".",
            "...",
            "",
        ] {
            assert!(
                validate_name(bad).is_err(),
                "expected reject for {bad:?}"
            );
        }
    }

    #[test]
    fn user_override_path_lands_under_xdg_config() {
        let p = user_override_path("meeting").unwrap();
        let s = p.to_string_lossy();
        assert!(s.contains("zwhisper/profiles/meeting.toml"), "{s}");
    }

    #[test]
    fn shipped_path_uses_compile_time_root_with_fallback() {
        let p = shipped_path("meeting").unwrap();
        let s = p.to_string_lossy();
        assert!(s.ends_with("/profiles/meeting.toml"), "{s}");
    }
}
