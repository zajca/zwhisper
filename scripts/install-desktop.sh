#!/usr/bin/env bash
# M6 G3 — install zwhisper.desktop into the user-local applications
# directory so xdg-desktop-portal can resolve our app-id when the
# tray (or `zwhisper hotkey bind`) calls into the GlobalShortcuts
# portal. See docs/M6-plan.md DoD #19 and docs/M6-architecture.md § 5.
#
# The destination filename mirrors the tray's well-known D-Bus name
# `cz.zajca.Zwhisper1.Tray` so the portal frontend keys persisted
# bindings against the same identifier we use for the bus name and
# systemd unit. Reverse-DNS form is required by xdg-desktop-portal
# app-id conventions.
#
# Usage:
#     scripts/install-desktop.sh
#
# Re-running is idempotent (the install command overwrites the
# destination file).

set -euo pipefail

# Resolve repo root from this script's location so the install works
# regardless of the caller's current working directory.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

SOURCE="$REPO_ROOT/packaging/zwhisper.desktop"
DEST_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
DEST_NAME="cz.zajca.Zwhisper1.Tray.desktop"
DEST="$DEST_DIR/$DEST_NAME"

if [ ! -f "$SOURCE" ]; then
    echo "error: source file not found: $SOURCE" >&2
    exit 1
fi

# `install -D` creates parent dirs as needed; mode 0644 matches the
# packaging convention used by /usr/share/applications.
install -Dm0644 "$SOURCE" "$DEST"
chmod +x "$DEST" || true
echo "installed: $DEST"

# update-desktop-database refreshes the cached MIME / app-id tables
# used by xdg-desktop-portal's app-id lookup. Best-effort: the file
# install is the load-bearing step; if the cache update is missing
# we log a hint but exit 0 so callers don't fail in CI containers
# where the helper isn't packaged.
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DEST_DIR" || {
        echo "warn: update-desktop-database returned non-zero; continuing" >&2
    }
    echo "update-desktop-database: $DEST_DIR refreshed"
else
    echo "note: update-desktop-database not found; skip cache refresh" >&2
    echo "      (the desktop file is installed; portal app-id lookup may take" >&2
    echo "       a few seconds longer to pick it up on first portal call)" >&2
fi
