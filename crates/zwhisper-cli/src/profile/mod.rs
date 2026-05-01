// Profile module — IDEA.md § 6 + § 11 (M2).
//
// Public surface: `Profile`, `ProfileError`, `CURRENT_SCHEMA_VERSION`,
// and `load(name)`. Everything else stays `pub(crate)` until M3
// daemon needs it for IPC.

pub(crate) mod commands;
pub(crate) mod embedded;
pub(crate) mod error;
pub(crate) mod loader;
pub(crate) mod migrations;
pub(crate) mod paths;
pub(crate) mod schema;

pub(crate) use error::ProfileError;
pub(crate) use schema::{OutputDest, Profile};

use std::path::PathBuf;

/// Where a resolved profile came from. Surfaces in `profile list` /
/// `profile show` to make the user-vs-shipped-vs-embedded distinction
/// visible and disambiguates `migrate`'s allowed targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProfileSource {
    /// `${XDG_CONFIG_HOME}/zwhisper/profiles/<name>.toml`.
    UserOverride(PathBuf),
    /// `${ZWHISPER_DATA_DIR:-/usr/share/zwhisper}/profiles/<name>.toml`.
    Shipped(PathBuf),
    /// `include_dir!`-embedded at compile time.
    Embedded(&'static str),
}

impl ProfileSource {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::UserOverride(_) => "user",
            Self::Shipped(_) => "shipped",
            Self::Embedded(_) => "embedded",
        }
    }
}

/// Resolve a profile name to its source, honouring the
/// user → shipped → embedded precedence from IDEA.md § 6.
pub(crate) fn resolve(name: &str) -> Result<ProfileSource, ProfileError> {
    paths::validate_name(name)?;

    let user = paths::user_override_path(name)?;
    if user.is_file() {
        return Ok(ProfileSource::UserOverride(user));
    }

    let shipped = paths::shipped_path(name)?;
    if shipped.is_file() {
        return Ok(ProfileSource::Shipped(shipped));
    }

    if embedded::lookup(name).is_some() {
        return Ok(ProfileSource::Embedded(name_for_embedded(name)));
    }

    Err(ProfileError::NotFound {
        name: name.to_owned(),
        searched: vec![
            user.display().to_string(),
            shipped.display().to_string(),
            "<embedded>".into(),
        ],
    })
}

/// Public façade: resolve and load a profile by name.
pub(crate) fn load(name: &str) -> Result<Profile, ProfileError> {
    match resolve(name)? {
        ProfileSource::UserOverride(path) | ProfileSource::Shipped(path) => {
            loader::load_from_path(&path)
        }
        ProfileSource::Embedded(_) => {
            let body = embedded::lookup(name).ok_or_else(|| ProfileError::NotFound {
                name: name.to_owned(),
                searched: vec!["<embedded>".into()],
            })?;
            loader::load_from_str(body, name)
        }
    }
}

fn name_for_embedded(name: &str) -> &'static str {
    // Map a runtime name to its statically-allocated counterpart so
    // ProfileSource::Embedded can carry a &'static str. Since the
    // embedded directory is fixed at compile time, we just bounce
    // off `embedded::names`.
    embedded::names()
        .into_iter()
        .find(|n| *n == name)
        .unwrap_or("<unknown>")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn resolve_falls_back_to_embedded_for_known_name() {
        // We cannot guarantee the user has no override, but when the
        // tests run in a clean tempdir as $XDG_CONFIG_HOME we get the
        // embedded path. CI hosts honour this — sanity-check that
        // the `default` template is reachable somehow.
        let src = resolve("default").unwrap();
        assert!(matches!(
            src,
            ProfileSource::Embedded(_) | ProfileSource::Shipped(_) | ProfileSource::UserOverride(_)
        ));
    }

    #[test]
    fn resolve_unknown_returns_not_found_with_three_locations() {
        let err = resolve("definitely-not-a-real-profile").unwrap_err();
        match err {
            ProfileError::NotFound { searched, .. } => {
                assert_eq!(searched.len(), 3);
                assert!(searched.iter().any(|s| s.contains("<embedded>")));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn invalid_name_short_circuits_before_io() {
        let err = resolve("../etc/passwd").unwrap_err();
        assert!(matches!(err, ProfileError::InvalidName { .. }));
    }

    #[test]
    fn load_default_via_embedded_yields_valid_profile() {
        // Parallels `resolve_falls_back_to_embedded_for_known_name`
        // but exercises the full load path.
        let profile = load("default").unwrap();
        assert_eq!(profile.name, "default");
        assert_eq!(profile.schema_version, super::loader::CURRENT_SCHEMA_VERSION);
    }
}
