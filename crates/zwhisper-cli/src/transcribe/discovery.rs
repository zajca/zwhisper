//! `whisper-cli` binary discovery — implements the 5-step lookup
//! from IDEA.md § 4 (steps 1–4 are runtime, step 5 is M7 settings UI).
//!
//! The lookup is wrapped behind a [`Locator`] trait so unit tests
//! exercise every branch without mutating process-global env vars
//! or relying on the host's `$PATH`. Production wires up
//! [`RealLocator`].

// The discovery surface is consumed by the runner that lands in M1
// phase 3. Until then nothing calls `locate_whisper_cli` from main.rs
// — the unit tests already exercise every branch via `locate_with`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use tracing::debug;

use super::error::TranscribeError;

/// Indirection over env / `$PATH` / filesystem lookups so the
/// resolver can be unit tested without touching process state.
pub(crate) trait Locator {
    /// Look up an env var by name. Returns `None` if unset or empty.
    fn env_var(&self, name: &str) -> Option<String>;

    /// Look up a binary on `$PATH` (analogous to `which::which`).
    fn which(&self, binary: &str) -> Option<PathBuf>;

    /// Return the user's home dir (or `None` if unresolvable).
    fn home_dir(&self) -> Option<PathBuf>;

    /// Return `true` if `path` exists and is an executable file.
    fn is_executable(&self, path: &Path) -> bool;
}

/// Production [`Locator`] backed by `std::env`, `which::which`,
/// `dirs::home_dir`, and a `metadata` + Unix-mode executable check.
#[derive(Debug, Default)]
pub(crate) struct RealLocator;

impl Locator for RealLocator {
    fn env_var(&self, name: &str) -> Option<String> {
        match std::env::var(name) {
            Ok(v) if !v.is_empty() => Some(v),
            _ => None,
        }
    }

    fn which(&self, binary: &str) -> Option<PathBuf> {
        which::which(binary).ok()
    }

    fn home_dir(&self) -> Option<PathBuf> {
        dirs::home_dir()
    }

    fn is_executable(&self, path: &Path) -> bool {
        is_executable_file(path)
    }
}

/// Cross-platform "is this an executable file?" check. On Unix we
/// require any of the `x` bits to be set; on other platforms we
/// settle for "is a file" because Windows executability is encoded
/// in the extension, not in the inode.
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// Marker `PathBuf` displayed in `searched:` for `$PATH` lookups so
/// the user sees where we actually probed instead of an empty list.
fn path_marker(binary: &str) -> PathBuf {
    PathBuf::from(format!("{binary} (PATH)"))
}

/// 5-step lookup from IDEA.md § 4 (steps 1–4 — step 5 is M7).
///
/// Order:
/// 1. `ZWHISPER_WHISPER_CLI` — must be an existing executable when
///    set; an explicit env var is a contract, so we do **not**
///    silently fall through if the path is broken.
/// 2. `which("whisper-cli")`
/// 3. `which("whisper-cpp")`
/// 4. `~/.local/bin/whisper-cli` (executable)
/// 5. → [`TranscribeError::BackendUnavailable`] enumerating every
///    location we attempted.
pub(crate) fn locate_with<L: Locator>(l: &L) -> Result<PathBuf, TranscribeError> {
    let mut searched: Vec<PathBuf> = Vec::new();

    // Step 1: explicit override.
    if let Some(raw) = l.env_var("ZWHISPER_WHISPER_CLI") {
        let candidate = PathBuf::from(&raw);
        if l.is_executable(&candidate) {
            debug!(path = %candidate.display(), "whisper-cli located via ZWHISPER_WHISPER_CLI");
            return Ok(candidate);
        }
        // An explicit env var that doesn't resolve is a hard error —
        // surface it with only the offending path so the user fixes
        // the var instead of ignoring the failed override.
        return Err(TranscribeError::BackendUnavailable {
            searched: vec![candidate],
        });
    }

    // Step 2: whisper-cli on PATH.
    if let Some(path) = l.which("whisper-cli") {
        debug!(path = %path.display(), "whisper-cli located on PATH");
        return Ok(path);
    }
    searched.push(path_marker("whisper-cli"));

    // Step 3: whisper-cpp alias on PATH (some distros ship the
    // binary under the older name).
    if let Some(path) = l.which("whisper-cpp") {
        debug!(path = %path.display(), "whisper-cli located on PATH (whisper-cpp alias)");
        return Ok(path);
    }
    searched.push(path_marker("whisper-cpp"));

    // Step 4: ~/.local/bin/whisper-cli — common manual-install spot.
    if let Some(home) = l.home_dir() {
        let candidate = home.join(".local").join("bin").join("whisper-cli");
        if l.is_executable(&candidate) {
            debug!(path = %candidate.display(), "whisper-cli located in ~/.local/bin");
            return Ok(candidate);
        }
        searched.push(candidate);
    }

    Err(TranscribeError::BackendUnavailable { searched })
}

/// Production entry point — wires up [`RealLocator`].
pub(crate) fn locate_whisper_cli() -> Result<PathBuf, TranscribeError> {
    locate_with(&RealLocator)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[derive(Default)]
    struct MockLocator {
        env: HashMap<String, String>,
        path: HashMap<String, PathBuf>,
        home: Option<PathBuf>,
        executables: HashSet<PathBuf>,
    }

    impl MockLocator {
        fn with_env(mut self, name: &str, value: &str) -> Self {
            self.env.insert(name.to_owned(), value.to_owned());
            self
        }

        fn with_on_path(mut self, binary: &str, resolved: PathBuf) -> Self {
            self.path.insert(binary.to_owned(), resolved);
            self
        }

        fn with_home(mut self, home: PathBuf) -> Self {
            self.home = Some(home);
            self
        }

        fn with_executable(mut self, path: PathBuf) -> Self {
            self.executables.insert(path);
            self
        }
    }

    impl Locator for MockLocator {
        fn env_var(&self, name: &str) -> Option<String> {
            self.env.get(name).filter(|v| !v.is_empty()).cloned()
        }

        fn which(&self, binary: &str) -> Option<PathBuf> {
            self.path.get(binary).cloned()
        }

        fn home_dir(&self) -> Option<PathBuf> {
            self.home.clone()
        }

        fn is_executable(&self, path: &Path) -> bool {
            self.executables.contains(path)
        }
    }

    #[test]
    fn env_var_set_and_exists_returns_path() {
        let target = PathBuf::from("/opt/whisper/whisper-cli");
        let l = MockLocator::default()
            .with_env("ZWHISPER_WHISPER_CLI", "/opt/whisper/whisper-cli")
            .with_executable(target.clone());

        let resolved = locate_with(&l).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn env_var_set_but_missing_returns_unavailable() {
        let l = MockLocator::default()
            .with_env("ZWHISPER_WHISPER_CLI", "/does/not/exist/whisper-cli");

        let err = locate_with(&l).unwrap_err();
        let TranscribeError::BackendUnavailable { searched } = &err else {
            panic!("expected BackendUnavailable, got {err:?}");
        };
        assert_eq!(searched, &vec![PathBuf::from("/does/not/exist/whisper-cli")]);
    }

    #[test]
    fn whisper_cli_on_path_wins() {
        let target = PathBuf::from("/usr/bin/whisper-cli");
        let l = MockLocator::default().with_on_path("whisper-cli", target.clone());

        let resolved = locate_with(&l).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn whisper_cpp_alias_used_when_cli_missing() {
        let target = PathBuf::from("/usr/local/bin/whisper-cpp");
        let l = MockLocator::default().with_on_path("whisper-cpp", target.clone());

        let resolved = locate_with(&l).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn home_local_bin_fallback() {
        let home = PathBuf::from("/home/zwhisper-test");
        let target = home.join(".local").join("bin").join("whisper-cli");
        let l = MockLocator::default()
            .with_home(home)
            .with_executable(target.clone());

        let resolved = locate_with(&l).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn nothing_found_returns_unavailable() {
        let home = PathBuf::from("/home/zwhisper-test");
        let l = MockLocator::default().with_home(home.clone());

        let err = locate_with(&l).unwrap_err();
        let TranscribeError::BackendUnavailable { searched } = &err else {
            panic!("expected BackendUnavailable, got {err:?}");
        };
        assert_eq!(
            searched,
            &vec![
                PathBuf::from("whisper-cli (PATH)"),
                PathBuf::from("whisper-cpp (PATH)"),
                home.join(".local").join("bin").join("whisper-cli"),
            ]
        );
    }

    #[test]
    fn nothing_found_without_home_omits_home_entry() {
        let l = MockLocator::default();

        let err = locate_with(&l).unwrap_err();
        let TranscribeError::BackendUnavailable { searched } = &err else {
            panic!("expected BackendUnavailable, got {err:?}");
        };
        assert_eq!(
            searched,
            &vec![
                PathBuf::from("whisper-cli (PATH)"),
                PathBuf::from("whisper-cpp (PATH)"),
            ]
        );
    }

    #[test]
    fn empty_env_var_treated_as_unset() {
        // Ensure an explicitly empty env var does not bypass PATH.
        let target = PathBuf::from("/usr/bin/whisper-cli");
        let l = MockLocator::default()
            .with_env("ZWHISPER_WHISPER_CLI", "")
            .with_on_path("whisper-cli", target.clone());

        let resolved = locate_with(&l).unwrap();
        assert_eq!(resolved, target);
    }
}
