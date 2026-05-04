//! M8 — packaging icon asset (DoD #18).
//!
//! These tests pin the contract that the in-tree
//! `assets/icons/zwhisper.svg` is:
//!
//! - present
//! - parseable as XML (`xmllint --noout`)
//! - free of embedded `<script>` (a security-sensitive constraint:
//!   the icon is rendered by every desktop-environment icon cache
//!   and SVG `<script>` would execute under whatever sandbox
//!   policy the renderer enforces)
//! - declares an explicit `viewBox` so cache helpers can scale
//!   without rasterising at 16 px and producing a blurry icon
//! - referenced by the tray's `.desktop` file via `Icon=zwhisper`
//!
//! `xmllint` is a member of `libxml2` (Arch) / `libxml2-utils`
//! (Debian-likes); the test skips when it is unavailable.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::pedantic
)]

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at this crate; go up two levels
    // (crates/zwhisper-tray -> workspace root).
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn icon_path() -> PathBuf {
    workspace_root().join("assets/icons/zwhisper.svg")
}

#[test]
fn icon_file_exists_at_canonical_path() {
    let path = icon_path();
    assert!(
        path.exists(),
        "zwhisper.svg must exist at {} (M8 DoD #18)",
        path.display()
    );
}

#[test]
fn icon_is_clean_svg() {
    let path = icon_path();
    if !path.exists() {
        panic!("missing icon at {}", path.display());
    }

    if which::which("xmllint").is_err() {
        eprintln!("[SKIP] icon_is_clean_svg: xmllint not on PATH");
        return;
    }

    let out = Command::new("xmllint")
        .arg("--noout")
        .arg(&path)
        .output()
        .expect("invoke xmllint");
    assert!(
        out.status.success(),
        "xmllint --noout failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let body = std::fs::read_to_string(&path).expect("read svg");
    assert!(
        body.contains("viewBox"),
        "icon must declare a viewBox attribute"
    );
    assert!(
        !body.contains("<script"),
        "icon must not contain <script> tags (security)"
    );
    assert!(
        !body.contains("data:image"),
        "icon must not embed raster data: URIs"
    );
}

/// The tray `.desktop` file's `Icon=zwhisper` line resolves at run
/// time to the installed
/// `/usr/share/icons/hicolor/scalable/apps/zwhisper.svg`. Pin the
/// in-tree desktop file uses the bare name `zwhisper` (no path,
/// no extension) so the XDG icon-theme spec resolution stays
/// portable across distributions.
#[test]
fn tray_desktop_file_references_zwhisper_icon() {
    let desktop = workspace_root().join("packaging/zwhisper.desktop");
    let body = std::fs::read_to_string(&desktop).expect("read tray desktop");
    assert!(
        body.lines().any(|l| l.trim() == "Icon=zwhisper"),
        "tray .desktop must use Icon=zwhisper (bare name); body was:\n{body}"
    );
}
