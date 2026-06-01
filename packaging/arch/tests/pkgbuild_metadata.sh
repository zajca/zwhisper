#!/usr/bin/env bash
#
# M8 DoD #1, #2, #3 — PKGBUILD metadata smoke tests.
#
# Each `_test_*` function asserts a single property; the runner at
# the bottom invokes them all and exits non-zero on the first
# failure. The test exists so a bad packaging edit (missing
# `license`, missing `arch`, drift in `makedepends`) is caught at
# CI time before a release tag is cut.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PKGBUILD="$SCRIPT_DIR/../PKGBUILD"

if [[ ! -f "$PKGBUILD" ]]; then
    echo "FAIL: PKGBUILD not found at $PKGBUILD" >&2
    exit 1
fi

asserts_required_fields() {
    local f="$PKGBUILD"
    grep -q '^pkgname=zwhisper$'      "$f" || { echo "FAIL: pkgname missing"  >&2; return 1; }
    grep -q '^pkgver=[0-9]'           "$f" || { echo "FAIL: pkgver missing"   >&2; return 1; }
    grep -q '^pkgrel=[0-9]'           "$f" || { echo "FAIL: pkgrel missing"   >&2; return 1; }
    grep -q "^arch=('x86_64')$"       "$f" || { echo "FAIL: arch missing"     >&2; return 1; }
    grep -q "^license=('MIT')$"       "$f" || { echo "FAIL: license missing"  >&2; return 1; }
    grep -q '^url='                   "$f" || { echo "FAIL: url missing"      >&2; return 1; }
    grep -q '^source='                "$f" || { echo "FAIL: source missing"   >&2; return 1; }
    grep -q '^b2sums='                "$f" || { echo "FAIL: b2sums missing"   >&2; return 1; }
    echo "ok: asserts_required_fields"
}

makedepends_covers_fltk_source_chain() {
    local f="$PKGBUILD"
    # The FLTK source-build chain must contain cmake, gcc, pkgconf,
    # fontconfig, freetype2, plus the Wayland link surface (pango,
    # libxkbcommon, wayland, wayland-protocols, dbus). X11 headers
    # are intentionally forbidden.
    local required=(
        "rust"
        "cargo"
        "cmake"
        "gcc"
        "pkgconf"
        "pango"
        "fontconfig"
        "freetype2"
        "libxkbcommon"
        "wayland"
        "wayland-protocols"
        "dbus"
    )
    for dep in "${required[@]}"; do
        # The file uses single-quoted entries; some carry version
        # constraints (`'rust>=1.85'`). Match the bare name as a
        # prefix inside any single-quoted token.
        if ! grep -qE "'${dep}(>=[0-9.]+)?'" "$f"; then
            echo "FAIL: makedepends missing '${dep}'" >&2
            return 1
        fi
    done
    local forbidden=(
        "libxft"
        "libxcursor"
        "libxinerama"
        "libxfixes"
    )
    for dep in "${forbidden[@]}"; do
        if grep -qE "'${dep}(>=[0-9.]+)?'" "$f"; then
            echo "FAIL: makedepends must not include X11 dependency '${dep}'" >&2
            return 1
        fi
    done
    echo "ok: makedepends_covers_fltk_source_chain"
}

runtime_depends_match_runtime_features() {
    local f="$PKGBUILD"
    local required=(
        "gstreamer"
        "gst-plugins-base"
        "gst-plugins-good"
        "gst-plugin-pipewire"
        "pipewire"
        "wireplumber"
        "dbus"
        "xdg-desktop-portal"
        "libnotify"
    )
    for dep in "${required[@]}"; do
        if ! grep -q "'${dep}'" "$f"; then
            echo "FAIL: depends missing '${dep}'" >&2
            return 1
        fi
    done
    echo "ok: runtime_depends_match_runtime_features"
}

build_uses_frozen_release_workspace() {
    local f="$PKGBUILD"
    grep -q 'cargo build --frozen --release --workspace' "$f" \
        || { echo "FAIL: build() does not use --frozen --release --workspace" >&2; return 1; }
    grep -q 'cargo fetch --locked' "$f" \
        || { echo "FAIL: prepare() does not run cargo fetch --locked" >&2; return 1; }
    grep -q 'CFLTK_WAYLAND_ONLY=1' "$f" \
        || { echo "FAIL: build() does not force FLTK Wayland-only mode" >&2; return 1; }
    echo "ok: build_uses_frozen_release_workspace"
}

asserts_required_fields
makedepends_covers_fltk_source_chain
runtime_depends_match_runtime_features
build_uses_frozen_release_workspace
echo "all PKGBUILD metadata checks passed"
