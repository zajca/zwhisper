// Phase 6 — `zwhisper profile {list, show, clone, migrate}` handlers.
//
// All four operate on the config plane; none of them need GStreamer.
// `main.rs` skips `init_gstreamer()` for the `Profile` arm.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use color_eyre::eyre::eyre;
use tracing::info;

use super::error::ProfileError;
use super::{ProfileSource, embedded, loader, paths, resolve};

/// `zwhisper profile list` — table of name + source + version + description.
///
/// Aggregation order: user > shipped > embedded; on collision the
/// strongest wins (mirrors `resolve`).
#[allow(clippy::unnecessary_wraps)] // future I/O additions land here without rewiring callers
pub(crate) fn list() -> color_eyre::Result<()> {
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();

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
            Entry::from_body("embedded", body)
        });
    }

    if entries.is_empty() {
        println!("(no profiles found)");
        return Ok(());
    }

    println!("{:<24}  {:<10}  {:<6}  description", "name", "source", "ver");
    println!("{}", "-".repeat(72));
    for (name, entry) in entries {
        let version_label = entry
            .schema_version
            .map_or_else(|| "?".to_owned(), |v| v.to_string());
        println!(
            "{name:<24}  {:<10}  {version_label:<6}  {}",
            entry.source,
            entry.description.unwrap_or_default()
        );
    }
    Ok(())
}

/// `zwhisper profile show <name>` — print resolved source + canonical
/// TOML body (post-migration if migration ran).
pub(crate) fn show(name: &str) -> color_eyre::Result<()> {
    let source = resolve(name).map_err(eyre_from)?;
    println!("source: {} ({})", source.label(), source_path_label(&source));

    let profile = super::load(name).map_err(eyre_from)?;
    println!("---");
    println!(
        "{}",
        toml_edit::ser::to_string_pretty(&profile)
            .map_err(|e| eyre!("could not serialize profile: {e}"))?
    );
    Ok(())
}

/// `zwhisper profile clone <src> <dst>` — copy a resolved profile
/// into the user override dir, rewriting the `name` field to `<dst>`.
///
/// The "refuse to overwrite" guarantee is enforced atomically via
/// `OpenOptions::create_new`: the `target.exists()` + `fs::create`
/// approach has a TOCTOU window (the M2 review's Medium finding)
/// where a parallel writer between the check and the open would get
/// truncated rather than refused.
pub(crate) fn clone(src: &str, dst: &str) -> color_eyre::Result<()> {
    paths::validate_name(dst).map_err(eyre_from)?;

    let target = paths::user_override_path(dst).map_err(eyre_from)?;

    let mut profile = super::load(src).map_err(eyre_from)?;
    dst.clone_into(&mut profile.name);

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            eyre!(
                "could not create user profiles dir {}: {e}",
                parent.display()
            )
        })?;
    }

    let body = toml_edit::ser::to_string_pretty(&profile)
        .map_err(|e| eyre!("could not serialize cloned profile: {e}"))?;

    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(eyre_from(ProfileError::OverwriteRefused {
                path: target,
            }));
        }
        Err(e) => {
            return Err(eyre!("could not create {}: {e}", target.display()));
        }
    };
    f.write_all(body.as_bytes())
        .map_err(|e| eyre!("could not write {}: {e}", target.display()))?;
    f.sync_all()
        .map_err(|e| eyre!("could not fsync {}: {e}", target.display()))?;
    info!(
        src = src,
        dst = dst,
        path = %target.display(),
        "profile cloned"
    );
    println!("cloned {src} -> {}", target.display());
    Ok(())
}

/// `zwhisper profile migrate <name>` — force the migration chain on a
/// user override. No-op when already at `CURRENT_SCHEMA_VERSION`.
pub(crate) fn migrate(name: &str) -> color_eyre::Result<()> {
    paths::validate_name(name).map_err(eyre_from)?;
    let user_path = paths::user_override_path(name).map_err(eyre_from)?;
    if !user_path.is_file() {
        return Err(eyre!(
            "profile migrate operates on user overrides; {} not found. \
             Run `zwhisper profile clone {name} <name>` first.",
            user_path.display()
        ));
    }
    // Loading is enough — it migrates lazily.
    let profile = loader::load_from_path(&user_path).map_err(eyre_from)?;
    println!(
        "{name} at {} is now schema_version = {}",
        user_path.display(),
        profile.schema_version
    );
    Ok(())
}

fn shipped_profiles_dir() -> std::path::PathBuf {
    let root = std::env::var_os("ZWHISPER_DATA_DIR").map_or_else(
        || std::path::PathBuf::from("/usr/share/zwhisper"),
        std::path::PathBuf::from,
    );
    root.join("profiles")
}

#[derive(Debug)]
struct Entry {
    source: &'static str,
    schema_version: Option<u32>,
    description: Option<String>,
}

impl Entry {
    fn from_path(source: &'static str, path: &Path) -> Self {
        let body = fs::read_to_string(path).unwrap_or_default();
        Self::from_body(source, &body)
    }

    fn from_body(source: &'static str, body: &str) -> Self {
        let parsed = body.parse::<toml_edit::DocumentMut>().ok();
        let schema_version = parsed
            .as_ref()
            .and_then(|d| d.get("schema_version")?.as_integer())
            .and_then(|v| u32::try_from(v).ok());
        let description = parsed
            .as_ref()
            .and_then(|d| d.get("description")?.as_str())
            .map(str::to_owned);
        Self {
            source,
            schema_version,
            description,
        }
    }
}

fn scan_dir(dir: &Path, source: &'static str, entries: &mut BTreeMap<String, Entry>) {
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
            .entry(name)
            .or_insert_with(|| Entry::from_path(source, &path));
    }
}

fn source_path_label(src: &ProfileSource) -> String {
    match src {
        ProfileSource::UserOverride(p) | ProfileSource::Shipped(p) => p.display().to_string(),
        ProfileSource::Embedded(name) => format!("<embedded:{name}>"),
    }
}

#[allow(clippy::needless_pass_by_value)] // intentional point-free use in `.map_err(eyre_from)`
fn eyre_from(err: ProfileError) -> color_eyre::Report {
    eyre!("{err}")
}

/// Test-only helper: drive `clone` against a synthesized
/// `XDG_CONFIG_HOME` so unit tests do not pollute the developer's
/// real config dir.
#[cfg(test)]
pub(crate) fn clone_into_dir(src: &str, dst: &str, target: &Path) -> color_eyre::Result<super::Profile> {
    paths::validate_name(dst).map_err(eyre_from)?;
    let mut profile = super::load(src).map_err(eyre_from)?;
    dst.clone_into(&mut profile.name);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = toml_edit::ser::to_string_pretty(&profile)
        .map_err(|e| eyre!("serialize: {e}"))?;
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(eyre_from(ProfileError::OverwriteRefused {
                path: target.to_owned(),
            }));
        }
        Err(e) => return Err(eyre!("could not create {}: {e}", target.display())),
    };
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    Ok(profile)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
        assert!(err.to_string().contains("refusing to overwrite"));
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
