// Phase 3 — migration framework + atomic backup writer.
//
// A registered migration is `fn(&mut DocumentMut) -> Result<(), …>`.
// The chain runs sequentially from the file's `schema_version` up to
// `CURRENT_SCHEMA_VERSION`. Backups are written before any mutation
// reaches the disk, so a partial crash leaves the original body
// recoverable from `<file>.bak.<unix_ms>_<pid>`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use toml_edit::DocumentMut;
use tracing::{info, warn};

use super::error::ProfileError;

pub(crate) type MigrationError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type MigrationFn = fn(&mut DocumentMut) -> Result<(), MigrationError>;

/// Registered migrations. M2 ships zero real migrations — the v1
/// schema is the first locked version. The framework, backup logic,
/// and chain runner are all exercised by `cfg(test)` migrations
/// registered via `apply_chain_with`.
pub(crate) static MIGRATIONS: &[(u32, u32, MigrationFn)] = &[];

/// Write `body` to `<path>.bak.<unix_nanos>_<pid>_<seq>` using
/// `OpenOptions::create_new` so a parallel process collision returns
/// a typed `BackupFailed` instead of overwriting an earlier backup.
/// `unix_nanos + seq` keeps in-process repeats unique even when the
/// kernel clock resolution is coarser than the call cadence.
fn write_backup(path: &Path, body: &str) -> Result<PathBuf, ProfileError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let unix_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let backup = path.with_extension(match path.extension() {
        Some(ext) => format!("{}.bak.{unix_nanos}_{pid}_{seq}", ext.to_string_lossy()),
        None => format!("bak.{unix_nanos}_{pid}_{seq}"),
    });

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&backup)
        .map_err(|source| ProfileError::BackupFailed {
            path: backup.clone(),
            source,
        })?;
    file.write_all(body.as_bytes())
        .map_err(|source| ProfileError::BackupFailed {
            path: backup.clone(),
            source,
        })?;
    file.sync_all()
        .map_err(|source| ProfileError::BackupFailed {
            path: backup.clone(),
            source,
        })?;
    Ok(backup)
}

/// Run the registered chain in-place. Public wrapper used by the
/// loader; tests call `apply_chain_with` to inject custom migrations.
pub(crate) fn run_in_place(
    path: &Path,
    original_body: &str,
    doc: &mut DocumentMut,
    from: u32,
    to: u32,
) -> Result<(), ProfileError> {
    run_in_place_with(path, original_body, doc, from, to, MIGRATIONS)
}

/// Test-injectable variant: same logic as `run_in_place`, but with a
/// caller-provided migration registry. Production code calls
/// `run_in_place` (which uses `MIGRATIONS`).
pub(crate) fn run_in_place_with(
    path: &Path,
    original_body: &str,
    doc: &mut DocumentMut,
    from: u32,
    to: u32,
    registry: &[(u32, u32, MigrationFn)],
) -> Result<(), ProfileError> {
    if from >= to {
        // No-op: nothing to migrate. Skip backup + rewrite — the
        // disk file is already at or above `to`.
        return Ok(());
    }
    let backup = write_backup(path, original_body)?;
    info!(
        backup = %backup.display(),
        from,
        to,
        path = %path.display(),
        "wrote profile backup before migration"
    );

    apply_chain(doc, from, to, registry).map_err(|(step_from, step_to, source)| {
        warn!(
            backup = %backup.display(),
            "migration failed; original profile preserved at backup path"
        );
        ProfileError::MigrationFailed {
            path: path.to_owned(),
            from: step_from,
            to: step_to,
            source,
        }
    })?;

    // Pin the version to `to` regardless of how the migration chain
    // updated it — chain authors should set it themselves, but
    // doing it here is a belt-and-braces against forgetful authors.
    doc["schema_version"] = toml_edit::value(i64::from(to));

    rewrite_in_place(path, doc)?;
    info!(path = %path.display(), to, "profile migrated to current schema");
    Ok(())
}

fn apply_chain(
    doc: &mut DocumentMut,
    from: u32,
    to: u32,
    registry: &[(u32, u32, MigrationFn)],
) -> Result<(), (u32, u32, MigrationError)> {
    let mut current = from;
    while current < to {
        let next = current + 1;
        let step = registry
            .iter()
            .find(|(f, t, _)| *f == current && *t == next);
        let Some((step_from, step_to, func)) = step else {
            return Err((
                current,
                next,
                format!("no registered migration for {current} -> {next}").into(),
            ));
        };
        func(doc).map_err(|src| (*step_from, *step_to, src))?;
        current = next;
    }
    Ok(())
}

fn rewrite_in_place(path: &Path, doc: &DocumentMut) -> Result<(), ProfileError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix(".zwhisper-profile-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|source| ProfileError::Io {
            path: path.to_owned(),
            source,
        })?;
    {
        let body = doc.to_string();
        // Write through the AsFile handle then drop before persist.
        let mut handle = tmp.as_file();
        handle
            .write_all(body.as_bytes())
            .map_err(|source| ProfileError::Io {
                path: path.to_owned(),
                source,
            })?;
        handle.sync_all().map_err(|source| ProfileError::Io {
            path: path.to_owned(),
            source,
        })?;
    }
    tmp.persist(path).map_err(|persist_err| ProfileError::Io {
        path: path.to_owned(),
        source: persist_err.error,
    })?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    const V0_BODY: &str = r#"schema_version = 0
name = "test"

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
path = "~/x/{profile}/{timestamp}.flac"
"#;

    #[allow(clippy::unnecessary_wraps)] // signature must match `MigrationFn`
    fn fake_v0_to_v1(_doc: &mut DocumentMut) -> Result<(), MigrationError> {
        // No-op migration: v0 == v1 in this test fixture.
        Ok(())
    }

    fn always_fail_v0_to_v1(_doc: &mut DocumentMut) -> Result<(), MigrationError> {
        Err("intentional test failure".into())
    }

    fn write_v0_profile(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("test.toml");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(V0_BODY.as_bytes()).unwrap();
        path
    }

    #[test]
    fn backup_uses_create_new_and_unique_suffix() {
        let dir = TempDir::new().unwrap();
        let path = write_v0_profile(&dir);
        let original = fs::read_to_string(&path).unwrap();
        let backup = write_backup(&path, &original).unwrap();
        assert!(backup.exists());
        let backed_up = fs::read_to_string(&backup).unwrap();
        assert_eq!(backed_up, original);
        let name = backup.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.contains(".bak."), "{name}");
        assert!(name.contains(&format!("_{}", std::process::id())), "{name}");
    }

    #[test]
    fn run_in_place_applies_chain_and_pins_version() {
        let dir = TempDir::new().unwrap();
        let path = write_v0_profile(&dir);
        let body = fs::read_to_string(&path).unwrap();
        let mut doc: DocumentMut = body.parse().unwrap();

        let registry: &[(u32, u32, MigrationFn)] = &[(0, 1, fake_v0_to_v1)];
        run_in_place_with(&path, &body, &mut doc, 0, 1, registry).unwrap();

        let new_body = fs::read_to_string(&path).unwrap();
        assert!(new_body.contains("schema_version = 1"));

        // Backup file lives in the same dir.
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let any_backup = entries.iter().any(|n| n.contains(".bak."));
        assert!(any_backup, "no backup found among {entries:?}");
    }

    #[test]
    fn missing_migration_returns_typed_error() {
        let dir = TempDir::new().unwrap();
        let path = write_v0_profile(&dir);
        let body = fs::read_to_string(&path).unwrap();
        let mut doc: DocumentMut = body.parse().unwrap();

        // Empty registry — chain cannot be walked.
        let err = run_in_place_with(&path, &body, &mut doc, 0, 1, &[]).unwrap_err();
        assert!(matches!(
            err,
            ProfileError::MigrationFailed { from: 0, to: 1, .. }
        ));
    }

    #[test]
    fn failing_migration_propagates_with_chain_step_versions() {
        let dir = TempDir::new().unwrap();
        let path = write_v0_profile(&dir);
        let body = fs::read_to_string(&path).unwrap();
        let mut doc: DocumentMut = body.parse().unwrap();

        let registry: &[(u32, u32, MigrationFn)] = &[(0, 1, always_fail_v0_to_v1)];
        let err = run_in_place_with(&path, &body, &mut doc, 0, 1, registry).unwrap_err();
        match err {
            ProfileError::MigrationFailed {
                from, to, source, ..
            } => {
                assert_eq!(from, 0);
                assert_eq!(to, 1);
                assert!(source.to_string().contains("intentional test failure"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn idempotency_short_circuits_when_from_ge_to() {
        let dir = TempDir::new().unwrap();
        let path = write_v0_profile(&dir);
        let body = fs::read_to_string(&path).unwrap();
        let mut doc: DocumentMut = body.parse().unwrap();

        let registry: &[(u32, u32, MigrationFn)] = &[(0, 1, fake_v0_to_v1)];
        // Real upgrade: 0 -> 1 writes a backup.
        run_in_place_with(&path, &body, &mut doc, 0, 1, registry).unwrap();
        let backups_after_real_upgrade = count_backups(dir.path());
        assert_eq!(backups_after_real_upgrade, 1);

        // No-op: 1 -> 1 writes nothing (no backup, no rewrite).
        let body_now = fs::read_to_string(&path).unwrap();
        let mut doc_now: DocumentMut = body_now.parse().unwrap();
        run_in_place_with(&path, &body_now, &mut doc_now, 1, 1, registry).unwrap();
        let backups_after_noop = count_backups(dir.path());
        assert_eq!(
            backups_after_noop, backups_after_real_upgrade,
            "no-op call must not write a second backup"
        );
    }

    fn count_backups(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".bak."))
            .count()
    }
}
