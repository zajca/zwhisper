//! M7 public-surface freeze (DoD #18).
//!
//! These `use` lines pin every item that `zwhisper-settings` (built
//! by Group A in M7) imports from `zwhisper-core`. If any item below
//! becomes private again (or its arity / argument types change), this
//! integration test fails to compile and CI blocks the regression
//! the moment a contributor reverts the M7 D-prelim promotion.
//!
//! The surface contract is intentionally narrow: four `profile::*`
//! validators, two `transcribe::models::*` resolvers, and one
//! `transcribe::discovery::*` detector. Internal helpers
//! (`Locator`, `ModelDirProvider`, `resolve_with`, `locate_with`)
//! stay `pub(crate)` so the trait-injection test surface does not
//! leak across the crate boundary.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use zwhisper_core::profile::ProfileError;
use zwhisper_core::profile::{shipped_path, user_override_path, user_profiles_dir, validate_name};
use zwhisper_core::transcribe::TranscribeError;
use zwhisper_core::transcribe::discovery::detect_whisper_cli;
use zwhisper_core::transcribe::models::{models_dir, resolve_model};

/// Compile-time witness: the four `profile::*` items are reachable
/// from the `profile` namespace at the canonical signatures the
/// settings UI relies on. Coercing to a `fn` pointer with an
/// explicit type rejects any signature drift (different argument
/// types, different return types, different arity).
#[test]
fn m7_pub_surface_profile_paths_pinned() {
    let _: fn(&str) -> Result<(), ProfileError> = validate_name;
    let _: fn(&str) -> Result<PathBuf, ProfileError> = user_override_path;
    let _: fn(&str) -> Result<PathBuf, ProfileError> = shipped_path;
    let _: fn() -> Result<PathBuf, ProfileError> = user_profiles_dir;
}

/// Compile-time witness: the three `transcribe::*` items reachable
/// at the canonical signatures. `models_dir` and `detect_whisper_cli`
/// are M7-introduced thin wrappers; `resolve_model` was promoted from
/// `pub(crate)` to `pub` without a signature change.
#[test]
fn m7_pub_surface_transcribe_pinned() {
    let _: fn(&str) -> Result<PathBuf, TranscribeError> = resolve_model;
    let _: fn() -> Result<PathBuf, TranscribeError> = models_dir;
    let _: fn() -> Result<PathBuf, TranscribeError> = detect_whisper_cli;
}

/// Behavioural smoke: `validate_name` rejects path traversal before
/// any I/O. This is one of the security-critical guarantees the
/// settings profile editor relies on (Risk G2 in M7-plan), so we
/// pin the rejection here rather than only inside `paths.rs` where
/// future refactors might bypass the public path.
#[test]
fn validate_name_rejects_path_traversal_through_pub_surface() {
    for bad in ["../etc/passwd", "meeting/extra", "..", ".", ""] {
        let err = validate_name(bad).unwrap_err();
        assert!(
            matches!(err, ProfileError::InvalidName { .. }),
            "expected InvalidName for {bad:?}, got {err:?}"
        );
    }
}

/// Behavioural smoke: `models_dir` returns the same parent
/// directory the runtime resolver writes ggml files into. We can
/// only assert structural invariants here (the actual XDG dir is
/// host-specific), but that is enough to catch a future refactor
/// that would point the wrapper at a different subtree.
#[test]
fn models_dir_returns_zwhisper_models_subpath() {
    // `models_dir()` only fails when `dirs::data_local_dir()`
    // returns `None`. CI hosts always resolve it; gracefully skip
    // the assertion otherwise so this test does not become flaky
    // under exotic sandbox configurations.
    let Ok(dir) = models_dir() else {
        return;
    };
    let s = dir.to_string_lossy();
    assert!(
        s.ends_with("zwhisper/models"),
        "models_dir must terminate at '<data>/zwhisper/models', got {s}"
    );
}
