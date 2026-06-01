//! Persistent active-profile selection for CLI-only operation.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use zwhisper_core::profile;

const ACTIVE_PROFILE_FILE: &str = "active-profile";

pub(crate) fn load() -> Option<String> {
    let path = path()?;
    load_from(&path).ok().flatten()
}

pub(crate) fn store(name: &str) -> io::Result<PathBuf> {
    let path = path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not resolve XDG config directory for active profile",
        )
    })?;
    store_at(name, &path)?;
    Ok(path)
}

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|base| base.join("zwhisper").join(ACTIVE_PROFILE_FILE))
}

fn load_from(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let name = raw.trim();
            if name.is_empty() || profile::validate_name(name).is_err() {
                return Ok(None);
            }
            if profile::load(name).is_err() {
                return Ok(None);
            }
            Ok(Some(name.to_owned()))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn store_at(name: &str, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        let temp_path = parent.join(format!(
            ".{ACTIVE_PROFILE_FILE}.{}.tmp",
            uuid::Uuid::new_v4()
        ));
        {
            let mut temp = fs::File::create_new(&temp_path)?;
            use std::io::Write as _;
            writeln!(temp, "{name}")?;
            temp.sync_all()?;
        }
        fs::rename(temp_path, path)?;
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("active profile path has no parent: {}", path.display()),
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(&dir.path().join("active-profile")).unwrap(), None);
    }

    #[test]
    fn load_rejects_invalid_profile_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active-profile");
        fs::write(&path, "../nope\n").unwrap();
        assert_eq!(load_from(&path).unwrap(), None);
    }

    #[test]
    fn store_writes_trimmed_readable_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("active-profile");
        store_at("meeting", &path).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "meeting\n");
    }
}
