//! M7 — `DoD` #19: validate `packaging/zwhisper-settings.desktop`
//! via the system `desktop-file-validate` tool. Skipped when the
//! tool is absent so CI sandboxes without it do not fail.

use std::process::Command;

/// Resolve the workspace root from `CARGO_MANIFEST_DIR`. We ship
/// the desktop file at `<workspace>/packaging/zwhisper-settings.desktop`,
/// not under the crate.
fn desktop_file_path() -> std::path::PathBuf {
    let crate_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("packaging").join("zwhisper-settings.desktop"))
        .expect("workspace root resolves from CARGO_MANIFEST_DIR")
}

/// `DoD` #19 — `zwhisper-settings.desktop` is a valid Desktop
/// Entry. The system `desktop-file-validate` tool exits non-zero
/// only on real errors; "hint" lines about `Categories` containing
/// multiple main categories are accepted (Settings + Audio is the
/// shipped layout per M8 packaging plan).
#[test]
fn settings_file_parses_via_validator() {
    let path = desktop_file_path();
    assert!(
        path.is_file(),
        "expected desktop file at {}",
        path.display()
    );

    let output = match Command::new("desktop-file-validate").arg(&path).output() {
        Ok(o) => o,
        Err(e) => {
            // Tool missing → skip, mirroring the `app::tests::*`
            // session-bus skips. Manual gate MV-* covers the
            // packaging path.
            eprintln!("skipping: desktop-file-validate not available ({e})");
            return;
        }
    };

    assert!(
        output.status.success(),
        "desktop-file-validate exit status {:?}; stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
