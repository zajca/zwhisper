#!/usr/bin/env bash
#
# M8 DoD #5, #6, #7 — package() install-path contract.
#
# The PKGBUILD's package() function is plain bash that runs under
# `makepkg`'s fakeroot. We do not stand up makepkg here; instead we
# parse the file and assert the install commands cover every path
# the M8 plan declares. This catches a missing-install regression
# before the package transaction touches /usr.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PKGBUILD="$SCRIPT_DIR/../PKGBUILD"

if [[ ! -f "$PKGBUILD" ]]; then
    echo "FAIL: PKGBUILD not found at $PKGBUILD" >&2
    exit 1
fi

assert_install_target() {
    local target="$1"
    if ! grep -q "$target" "$PKGBUILD"; then
        echo "FAIL: install line missing target $target" >&2
        return 1
    fi
}

package_installs_all_artefacts() {
    # Binaries.
    assert_install_target '/usr/bin/zwhisperd'
    assert_install_target '/usr/bin/zwhisper'
    # systemd user unit.
    assert_install_target '/usr/lib/systemd/user/zwhisperd.service'
    # D-Bus session-bus auto-activation.
    assert_install_target '/usr/share/dbus-1/services/cz.zajca.Zwhisper1.service'
    # Shared data templates.
    assert_install_target '/usr/share/zwhisper/secrets.toml.example'
    # License + docs.
    assert_install_target '/usr/share/licenses/'
    assert_install_target '/usr/share/doc/'
    echo "ok: package_installs_all_artefacts"
}

package_excludes_retired_gui_artefacts() {
    local forbidden=(
        '/usr/bin/zwhisper-tray'
        '/usr/bin/zwhisper-settings'
        '/usr/lib/systemd/user/zwhisper-tray.service'
        '/usr/share/applications/zwhisper.desktop'
        '/usr/share/applications/zwhisper-settings.desktop'
        '/usr/share/icons/hicolor/scalable/apps/zwhisper.svg'
    )
    for target in "${forbidden[@]}"; do
        if grep -q "$target" "$PKGBUILD"; then
            echo "FAIL: retired GUI/tray target still installed: $target" >&2
            return 1
        fi
    done
    echo "ok: package_excludes_retired_gui_artefacts"
}

package_uses_install_only() {
    # Every package() body line that touches the destination uses
    # `install -D` — no plain `cp`, no `mkdir -p`, no `tar`. This
    # keeps the install set declaratively visible to namcap and
    # the static analysis above.
    if grep -E '^\s*(cp|mkdir|tar)\b' "$PKGBUILD"; then
        echo "FAIL: PKGBUILD uses non-install commands in package()" >&2
        return 1
    fi
    echo "ok: package_uses_install_only"
}

dbus_service_points_at_usr_bin() {
    # The in-tree D-Bus service file already encodes the canonical
    # path. PKGBUILD installs it verbatim. Pin the in-tree contents
    # so a future edit cannot regress the install path silently.
    local svc="$SCRIPT_DIR/../../../dbus/cz.zajca.Zwhisper1.service"
    if [[ ! -f "$svc" ]]; then
        echo "FAIL: dbus service file missing at $svc" >&2
        return 1
    fi
    if ! grep -q '^Exec=/usr/bin/zwhisperd$' "$svc"; then
        echo "FAIL: D-Bus service Exec= must be /usr/bin/zwhisperd" >&2
        return 1
    fi
    echo "ok: dbus_service_points_at_usr_bin"
}

installed_unit_uses_usr_bin() {
    local zwhisperd_unit="$SCRIPT_DIR/../../../systemd/zwhisperd.service"
    grep -q '^ExecStart=/usr/bin/zwhisperd$' "$zwhisperd_unit" \
        || { echo "FAIL: zwhisperd.service ExecStart must be /usr/bin/zwhisperd" >&2; return 1; }
    echo "ok: installed_unit_uses_usr_bin"
}

package_installs_all_artefacts
package_excludes_retired_gui_artefacts
package_uses_install_only
dbus_service_points_at_usr_bin
installed_unit_uses_usr_bin
echo "all install-path checks passed"
