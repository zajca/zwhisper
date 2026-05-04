//! Data-producing helpers for `zwhisper profile {list, clone, migrate}`.
//!
//! Phase 1 of M3 split the original `commands` module into two halves:
//! pure data operations (this file) and CLI pretty-printers (which
//! live in `zwhisper-cli`'s `profile_commands` module). The daemon
//! consumes the data half via D-Bus once Phase 2 lands; the CLI
//! consumes the same data and renders a human-readable table.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use tracing::info;

use super::error::ProfileError;
use super::{Profile, ProfileSource, embedded, loader, paths, resolve};

/// One row in the `profile list` table. `source` is the precedence
/// label (`"user"`, `"shipped"`, `"embedded"`); `schema_version` is
/// `None` when the TOML did not carry an integer at the
/// `schema_version` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    pub name: String,
    pub source: String,
    pub schema_version: Option<u32>,
    pub description: Option<String>,
    /// `[transcription].backend`, parsed from the TOML when present.
    /// `None` when the file is malformed or the backend field is
    /// absent; callers (`Profiles1.list_v2` wire emit) substitute
    /// `"whisper-cpp"` as the legacy default in that case so the
    /// tray gets a deterministic value.
    pub backend: Option<String>,
}

/// Aggregate every visible profile, honouring the
/// user > shipped > embedded precedence from IDEA.md § 6.
///
/// I/O failures on individual files are silently treated as missing
/// entries — a corrupted user override should not hide the shipped
/// fallback from `profile list`. Caller-facing errors here would
/// only confuse users who can already see the broken file in `ls`.
pub fn list_entries() -> Result<Vec<ProfileEntry>, ProfileError> {
    let mut entries: BTreeMap<String, ProfileEntry> = BTreeMap::new();

    if let Ok(dir) = paths::user_profiles_dir() {
        if dir.is_dir() {
            scan_dir(&dir, "user", &mut entries);
        }
    }

    let shipped_dir = shipped_profiles_dir();
    if shipped_dir.is_dir() {
        scan_dir(&shipped_dir, "shipped", &mut entries);
    }

    for name in embedded::names() {
        entries.entry(name.to_owned()).or_insert_with(|| {
            let body = embedded::lookup(name).unwrap_or_default();
            entry_from_body(name, "embedded", body)
        });
    }

    Ok(entries.into_values().collect())
}

/// Clone a profile from any source into a user override. The
/// destination filename is `${XDG_CONFIG_HOME}/zwhisper/profiles/<dst>.toml`
/// and the file is opened with `create_new` to refuse silent
/// overwrites — the M2 review's TOCTOU-safe pattern.
///
/// Returns the resolved destination path so callers can include it
/// in a "cloned `<src>` -> `<path>`" message without re-deriving it.
pub fn clone_to_user(src: &str, dst: &str) -> Result<PathBuf, ProfileError> {
    paths::validate_name(dst)?;
    let target = paths::user_override_path(dst)?;

    let mut profile = super::load(src)?;
    dst.clone_into(&mut profile.name);

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|source| ProfileError::Io {
            path: parent.to_owned(),
            source,
        })?;
    }

    let body =
        toml_edit::ser::to_string_pretty(&profile).map_err(|e| ProfileError::Validation {
            profile: dst.to_owned(),
            message: format!("could not serialize cloned profile: {e}"),
        })?;

    let mut f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ProfileError::OverwriteRefused { path: target });
        }
        Err(source) => {
            return Err(ProfileError::Io {
                path: target,
                source,
            });
        }
    };
    f.write_all(body.as_bytes())
        .map_err(|source| ProfileError::Io {
            path: target.clone(),
            source,
        })?;
    f.sync_all().map_err(|source| ProfileError::Io {
        path: target.clone(),
        source,
    })?;
    info!(
        src = src,
        dst = dst,
        path = %target.display(),
        "profile cloned"
    );
    Ok(target)
}

/// Force-load a user-override profile through the migration chain.
/// No-op when the file is already at `CURRENT_SCHEMA_VERSION`.
/// Errors out with a typed `ProfileError` when the named profile is
/// not a user override (the only mutable source).
pub fn migrate_user(name: &str) -> Result<Profile, ProfileError> {
    paths::validate_name(name)?;
    let user_path = paths::user_override_path(name)?;
    if !user_path.is_file() {
        return Err(ProfileError::NotFound {
            name: name.to_owned(),
            searched: vec![user_path.display().to_string()],
        });
    }
    loader::load_from_path(&user_path)
}

/// Re-resolution helper used by CLI `profile show` to print the
/// concrete file path / `<embedded>` marker before dumping the body.
/// Public because the CLI's `profile_commands` module needs it after
/// the carve-out; the daemon does not call this directly.
pub fn resolved_source(name: &str) -> Result<ProfileSource, ProfileError> {
    resolve(name)
}

fn shipped_profiles_dir() -> PathBuf {
    let root = std::env::var_os("ZWHISPER_DATA_DIR")
        .map_or_else(|| PathBuf::from("/usr/share/zwhisper"), PathBuf::from);
    root.join("profiles")
}

fn entry_from_path(name: &str, source: &str, path: &Path) -> ProfileEntry {
    let body = fs::read_to_string(path).unwrap_or_default();
    entry_from_body(name, source, &body)
}

fn entry_from_body(name: &str, source: &str, body: &str) -> ProfileEntry {
    let parsed = body.parse::<toml_edit::DocumentMut>().ok();
    let schema_version = parsed
        .as_ref()
        .and_then(|d| d.get("schema_version")?.as_integer())
        .and_then(|v| u32::try_from(v).ok());
    let description = parsed
        .as_ref()
        .and_then(|d| d.get("description")?.as_str())
        .map(str::to_owned);
    let backend = parsed
        .as_ref()
        .and_then(|d| d.get("transcription")?.as_table_like())
        .and_then(|t| t.get("backend")?.as_str())
        .map(str::to_owned);
    ProfileEntry {
        name: name.to_owned(),
        source: source.to_owned(),
        schema_version,
        description,
        backend,
    }
}

fn scan_dir(dir: &Path, source: &str, entries: &mut BTreeMap<String, ProfileEntry>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for ent in read.flatten() {
        let path = ent.path();
        let Some(file) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Filter `.bak.<ts>_<pid>` and other suffixes — we list real
        // profiles, not migration backups.
        let lower_ext_toml = path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"));
        if !lower_ext_toml {
            continue;
        }
        if file.contains(".toml.bak.") || file.starts_with('.') {
            continue;
        }
        let name = file.trim_end_matches(".toml").to_owned();
        // Stronger source already wins; only insert if not present.
        entries
            .entry(name.clone())
            .or_insert_with(|| entry_from_path(&name, source, &path));
    }
}

/// Test-only helper: drive the clone op against a synthesized
/// destination path so unit tests do not pollute the developer's
/// real config dir.
#[cfg(test)]
pub(crate) fn clone_into_dir(src: &str, dst: &str, target: &Path) -> Result<Profile, ProfileError> {
    paths::validate_name(dst)?;
    let mut profile = super::load(src)?;
    dst.clone_into(&mut profile.name);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|source| ProfileError::Io {
            path: parent.to_owned(),
            source,
        })?;
    }
    let body =
        toml_edit::ser::to_string_pretty(&profile).map_err(|e| ProfileError::Validation {
            profile: dst.to_owned(),
            message: format!("serialize: {e}"),
        })?;
    let mut f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ProfileError::OverwriteRefused {
                path: target.to_owned(),
            });
        }
        Err(source) => {
            return Err(ProfileError::Io {
                path: target.to_owned(),
                source,
            });
        }
    };
    f.write_all(body.as_bytes())
        .map_err(|source| ProfileError::Io {
            path: target.to_owned(),
            source,
        })?;
    f.sync_all().map_err(|source| ProfileError::Io {
        path: target.to_owned(),
        source,
    })?;
    Ok(profile)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn list_entries_contains_default_meeting_voicememo() {
        let entries = list_entries().unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        for required in ["default", "meeting", "voicememo"] {
            assert!(
                names.contains(&required),
                "list_entries missing {required}: {names:?}"
            );
        }
    }

    #[test]
    fn list_entries_reports_schema_version_for_embedded() {
        let entries = list_entries().unwrap();
        let default_entry = entries
            .iter()
            .find(|e| e.name == "default")
            .expect("default profile present");
        // The shipped/embedded `default.toml` always declares
        // `schema_version = 1`; if the embedded body parsed at all
        // the integer should round-trip.
        assert!(default_entry.schema_version.is_some());
    }

    #[test]
    fn clone_into_dir_writes_user_profile_with_renamed_field() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("custom.toml");
        let profile = clone_into_dir("default", "custom", &target).unwrap();
        assert_eq!(profile.name, "custom");
        let body = fs::read_to_string(&target).unwrap();
        assert!(body.contains("name = \"custom\""), "{body}");
    }

    #[test]
    fn clone_into_dir_refuses_existing_target() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("custom.toml");
        clone_into_dir("default", "custom", &target).unwrap();
        let err = clone_into_dir("default", "custom", &target).unwrap_err();
        assert!(matches!(err, ProfileError::OverwriteRefused { .. }));
    }

    #[test]
    fn scan_dir_filters_backup_suffix() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("a.toml"),
            "schema_version = 1\nname = \"a\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("a.toml.bak.1700000000000_999"),
            "doesn't matter",
        )
        .unwrap();
        let mut entries = BTreeMap::new();
        scan_dir(dir.path(), "user", &mut entries);
        assert_eq!(entries.len(), 1);
        assert!(entries.contains_key("a"));
    }
}
