// Phase 2 — TOML loader + schema_version gate + validation. Phase 3
// adds the migration call between version-gate and deserialize.

use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;
use tracing::debug;

use super::error::ProfileError;
use super::migrations;
use super::schema::Profile;

/// Schema version this build supports. Bumped any time a backward-
/// incompatible change lands in `Profile`. M3 daemon must agree at
/// startup; mismatched versions are a typed startup failure, not a
/// runtime warning.
pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Load a profile by absolute path. Disk path is the only failure
/// surface that touches I/O — the in-memory `load_from_str` fork is
/// reused for embedded templates.
pub(crate) fn load_from_path(path: &Path) -> Result<Profile, ProfileError> {
    let body = fs::read_to_string(path).map_err(|source| ProfileError::Io {
        path: path.to_owned(),
        source,
    })?;

    let mut doc: DocumentMut = body
        .parse()
        .map_err(|source| ProfileError::TomlParse {
            path: path.to_owned(),
            source,
        })?;

    let found = read_schema_version(&doc, path)?;

    if found > CURRENT_SCHEMA_VERSION {
        return Err(ProfileError::UnsupportedSchemaVersion {
            path: path.to_owned(),
            found,
            current: CURRENT_SCHEMA_VERSION,
        });
    }

    if found < CURRENT_SCHEMA_VERSION {
        debug!(
            from = found,
            to = CURRENT_SCHEMA_VERSION,
            path = %path.display(),
            "migrating profile to current schema version"
        );
        migrations::run_in_place(path, &body, &mut doc, found, CURRENT_SCHEMA_VERSION)?;
    }

    deserialize_validated(&doc, path)
}

/// Same logic as `load_from_path` but for in-memory TOML (embedded
/// templates). No backup, no rewrite — embedded bodies that miss the
/// current schema version are a build-time bug surfaced as
/// `MigrationFailed` because there is nothing the runtime can do.
pub(crate) fn load_from_str(body: &str, identity: &str) -> Result<Profile, ProfileError> {
    let synthetic_path = PathBuf::from(format!("<embedded:{identity}>"));

    let doc: DocumentMut = body
        .parse()
        .map_err(|source| ProfileError::TomlParse {
            path: synthetic_path.clone(),
            source,
        })?;

    let found = read_schema_version(&doc, &synthetic_path)?;
    if found != CURRENT_SCHEMA_VERSION {
        return Err(ProfileError::MigrationFailed {
            path: synthetic_path,
            from: found,
            to: CURRENT_SCHEMA_VERSION,
            source: format!(
                "embedded template at compile time has schema_version={found} \
                 but binary supports {CURRENT_SCHEMA_VERSION}; rebuild the binary \
                 with updated profiles/"
            )
            .into(),
        });
    }

    deserialize_validated(&doc, &synthetic_path)
}

fn read_schema_version(doc: &DocumentMut, path: &Path) -> Result<u32, ProfileError> {
    let item = doc
        .get("schema_version")
        .ok_or_else(|| ProfileError::MissingSchemaVersion {
            path: path.to_owned(),
        })?;
    let raw = item
        .as_integer()
        .ok_or_else(|| ProfileError::MissingSchemaVersion {
            path: path.to_owned(),
        })?;
    if raw <= 0 {
        return Err(ProfileError::MissingSchemaVersion {
            path: path.to_owned(),
        });
    }
    u32::try_from(raw).map_err(|_| ProfileError::MissingSchemaVersion {
        path: path.to_owned(),
    })
}

fn deserialize_validated(doc: &DocumentMut, path: &Path) -> Result<Profile, ProfileError> {
    let profile: Profile =
        toml_edit::de::from_document(doc.clone()).map_err(|source| ProfileError::TomlDeserialize {
            path: path.to_owned(),
            source,
        })?;
    profile.validate()?;
    Ok(profile)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const VALID_V1: &str = r#"
schema_version = 1
name = "test"
description = "fixture"

[sources]
mic = "default"
system_output = "default"
mode = "mono_mix"

[recording]
codec = "flac"
sample_rate = 16000
max_duration_minutes = 60

[transcription]
backend = "whisper-cpp"
model = "small"
language = "auto"
auto = true

[[output]]
type = "file"
path = "~/Recordings/zwhisper/{profile}/{timestamp}.flac"
"#;

    fn tmp_with(body: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f
    }

    #[test]
    fn happy_path_v1_loads_and_validates() {
        let f = tmp_with(VALID_V1);
        let p = load_from_path(f.path()).unwrap();
        assert_eq!(p.schema_version, 1);
        assert_eq!(p.name, "test");
    }

    #[test]
    fn missing_schema_version_is_typed_error() {
        let body = VALID_V1.replace("schema_version = 1\n", "");
        let f = tmp_with(&body);
        let err = load_from_path(f.path()).unwrap_err();
        assert!(matches!(err, ProfileError::MissingSchemaVersion { .. }));
    }

    #[test]
    fn schema_version_string_rejected_as_missing() {
        let body = VALID_V1.replace("schema_version = 1", "schema_version = \"1\"");
        let f = tmp_with(&body);
        let err = load_from_path(f.path()).unwrap_err();
        assert!(matches!(err, ProfileError::MissingSchemaVersion { .. }));
    }

    #[test]
    fn schema_version_zero_rejected_as_missing() {
        let body = VALID_V1.replace("schema_version = 1", "schema_version = 0");
        let f = tmp_with(&body);
        let err = load_from_path(f.path()).unwrap_err();
        assert!(matches!(err, ProfileError::MissingSchemaVersion { .. }));
    }

    #[test]
    fn schema_version_too_high_rejected() {
        let body = VALID_V1.replace("schema_version = 1", "schema_version = 99");
        let f = tmp_with(&body);
        let err = load_from_path(f.path()).unwrap_err();
        match err {
            ProfileError::UnsupportedSchemaVersion { found, current, .. } => {
                assert_eq!(found, 99);
                assert_eq!(current, 1);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn invalid_toml_rejected_with_path() {
        let f = tmp_with("schema_version = 1 broken garbage = ");
        let err = load_from_path(f.path()).unwrap_err();
        assert!(matches!(err, ProfileError::TomlParse { .. }));
    }

    #[test]
    fn missing_required_section_is_deserialize_error() {
        let body = VALID_V1.replace("[recording]\n", "[recording_typo]\n");
        let f = tmp_with(&body);
        let err = load_from_path(f.path()).unwrap_err();
        assert!(matches!(err, ProfileError::TomlDeserialize { .. }));
    }

    #[test]
    fn validation_failures_propagate_through_loader() {
        let body = VALID_V1.replace("sample_rate = 16000", "sample_rate = 9001");
        let f = tmp_with(&body);
        let err = load_from_path(f.path()).unwrap_err();
        assert!(matches!(err, ProfileError::Validation { .. }));
    }

    #[test]
    fn stereo_split_loaded_then_unsupported() {
        let body = VALID_V1.replace("mode = \"mono_mix\"", "mode = \"stereo_split\"");
        let f = tmp_with(&body);
        let err = load_from_path(f.path()).unwrap_err();
        assert!(matches!(err, ProfileError::UnsupportedMode { .. }));
    }

    #[test]
    fn nonexistent_path_returns_io_error() {
        let err = load_from_path(Path::new("/nonexistent/profile/path.toml")).unwrap_err();
        assert!(matches!(err, ProfileError::Io { .. }));
    }

    #[test]
    fn load_from_str_happy_path() {
        let p = load_from_str(VALID_V1, "test").unwrap();
        assert_eq!(p.name, "test");
    }

    #[test]
    fn load_from_str_rejects_old_version() {
        let body = VALID_V1.replace("schema_version = 1", "schema_version = 0");
        let err = load_from_str(&body, "test").unwrap_err();
        assert!(matches!(err, ProfileError::MissingSchemaVersion { .. }));
    }

    #[test]
    fn load_from_str_rejects_mismatched_version_as_migration_failed() {
        // Embedded templates are read-only; any version skew is a
        // build-time bug, regardless of direction.
        let body = VALID_V1.replace("schema_version = 1", "schema_version = 2");
        let err = load_from_str(&body, "test").unwrap_err();
        assert!(matches!(err, ProfileError::MigrationFailed { .. }));
    }
}
