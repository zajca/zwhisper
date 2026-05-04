# M8 — Packaging & release prep: implementation plan

## TL;DR

> M0–M7 produced four working binaries (`zwhisperd`, `zwhisper`,
> `zwhisper-tray`, `zwhisper-settings`) wired by D-Bus and shipped as
> a `cargo build --workspace` developer install. M8 turns that into
> an installable Arch Linux package, introduces a wire-level
> protocol-version handshake so mismatched daemon/client binaries
> refuse to talk, bumps the workspace version to `0.1.0`, formalises
> the release process, and lands an idle-RSS perf gate.
>
> - PKGBUILD (Arch), systemd-user units, D-Bus service, two `.desktop`
>   files, `zwhisper.svg` icon, `secrets.toml.example`, all installed
>   under standard paths (`/usr/bin`, `/usr/lib/systemd/user`,
>   `/usr/share/...`).
> - `zwhisper_ipc::PROTOCOL_VERSION` const + `Recorder1.ProtocolVersion`
>   property; CLI exits 3, tray notifies, settings disables actions on
>   mismatch.
> - `docs/RELEASE.md` + `CHANGELOG.md` + `scripts/refresh-checksums.sh`.
> - Idle/peak RSS regression smoke (DoD #24–#26), namcap allow-list.
> - Flatpak, AUR submission, .deb/RPM, NixOS, secrets editor — OUT
>   (deferred post-M8).

---

## Status snapshot (2026-05-03)

| File / surface | Today | After M8 |
|----------------|-------|----------|
| `Cargo.toml` workspace `version` | `"0.0.0"` | `"0.1.0"` |
| `packaging/arch/PKGBUILD` | absent | present, builds + installs all four binaries |
| `packaging/arch/zwhisper.install` | absent | present, refreshes desktop + icon caches |
| `packaging/arch/namcap.expected` | absent | present, allow-listed WARNINGs only |
| `assets/icons/zwhisper.svg` | absent | present, scalable SVG |
| `crates/zwhisper-ipc/src/lib.rs` `PROTOCOL_VERSION` | absent | `pub const = env!("CARGO_PKG_VERSION")` |
| `cz.zajca.Zwhisper1.Recorder1.ProtocolVersion` D-Bus property | absent | read-only `s` property |
| Client handshake (cli, tray, settings) | absent | refuses mismatched daemon |
| `docs/RELEASE.md` | absent | present, numbered steps with verify commands |
| `CHANGELOG.md` | absent | Keep-a-Changelog with `[0.1.0] - 2026-05-03` |
| `scripts/refresh-checksums.sh` | absent | re-downloads ggml models, refreshes `checksums.toml` |
| `docs/secrets.toml.example` | tracked under `docs/` | also tracked under `packaging/share/` for install layout sanity |
| Idle/peak RSS test | absent | `crates/zwhisperd/tests/m8_perf_gate.rs` |
| CI: namcap, packaging shell tests, version-handshake matrix | absent | three new jobs in `.github/workflows/ci.yml` |

**M8 unlocks.** Anyone with `git clone + makepkg -si` gets a clean,
isolated install on Arch. The version-handshake closes the partial-
upgrade hole flagged in M5/M6 plans. Future MX (post-M8) can layer
Flatpak, AUR, .deb, and a secrets editor on top of this skeleton.

---

## Definition of done

Each item is a single testable assertion. Items 1–8 lock the Arch
PKGBUILD and install layout; 9–13 the version handshake; 14–17 the
release / docs surface; 18–22 packaging assets, perf, CI; 23 the
manual gate.

### Arch PKGBUILD + install layout

1. `packaging/arch/PKGBUILD` exists with `pkgname=zwhisper`,
   `pkgver=0.1.0`, `pkgrel=1`, `arch=('x86_64')`, `license=('MIT')`,
   `url=$workspace.homepage`, and a `source=()` entry pointing at the
   tagged GitHub archive `v0.1.0.tar.gz`. Reproducibility: `b2sums`
   are populated by `updpkgsums` at tag time, recorded in
   `docs/RELEASE.md`. Test:
   `packaging/arch/tests/pkgbuild_metadata.sh::asserts_required_fields`.
2. `makedepends=()` lists the **fltk-bundled** chain in full:
   `cargo`, `rust>=1.85`, `cmake>=3.11`, `gcc>=11`, `curl`, `tar`,
   `pkgconf`, `autoconf`, `libxft`, `libxcursor`, `libxinerama`,
   `libxfixes`, `pango`, `fontconfig`, `freetype2`, `libxkbcommon`,
   `wayland`. Sourced from `crates/zwhisper-settings/README.md` §
   "Build dependencies". Test:
   `packaging/arch/tests/pkgbuild_metadata.sh::makedepends_covers_fltk_bundled_chain`.
3. `depends=()` lists the runtime chain:
   `gstreamer`, `gst-plugins-base`, `gst-plugins-good`,
   `gst-plugin-pipewire`, `pipewire`, `wireplumber`, `dbus`,
   `xdg-desktop-portal`, `libnotify`, `gcc-libs`, `glibc`. Test:
   `packaging/arch/tests/pkgbuild_metadata.sh::runtime_depends_match_runtime_features`.
4. `prepare()` runs `cargo fetch --locked --target $(rustc -vV | sed -n 's/host: //p')`;
   `build()` runs `cargo build --frozen --release --workspace` with
   `CARGO_TARGET_DIR=target` and `RUSTFLAGS=""` (no `-C target-cpu=native`).
   `check()` runs `cargo test --frozen --release --workspace --lib` so
   the package build is gated by unit tests but skips integration
   tests that need PipeWire/D-Bus. Test:
   `packaging/arch/tests/pkgbuild_steps.sh::build_uses_frozen_release_workspace`.
5. `package()` installs **only** via `install -Dm755` / `install -Dm644`
   (no `cp -r`, no `mkdir -p`) into the canonical paths:
   - `/usr/bin/{zwhisperd,zwhisper,zwhisper-tray,zwhisper-settings}` (mode 0755)
   - `/usr/lib/systemd/user/{zwhisperd.service,zwhisper-tray.service}` (mode 0644)
   - `/usr/share/dbus-1/services/cz.zajca.Zwhisper1.service` (mode 0644)
   - `/usr/share/applications/{zwhisper.desktop,zwhisper-settings.desktop}` (mode 0644)
   - `/usr/share/icons/hicolor/scalable/apps/zwhisper.svg` (mode 0644)
   - `/usr/share/zwhisper/{secrets.toml.example,models.toml.example,checksums.toml}` (mode 0644)
   - `/usr/share/licenses/zwhisper/LICENSE` (mode 0644)
   - `/usr/share/doc/zwhisper/{README.md,CHANGELOG.md}` (mode 0644)

   Test: `packaging/arch/tests/install_paths.sh::package_installs_all_artefacts`.
6. `package()` rewrites the `ExecStart=` lines in the installed
   systemd-user units from the in-tree dev path (if any) to
   `/usr/bin/zwhisperd` / `/usr/bin/zwhisper-tray` via a single
   `sed -i` invocation. The in-tree units already use `/usr/bin/...`
   today, so the rewrite is a defensive no-op verified by string
   match. Test:
   `packaging/arch/tests/install_paths.sh::installed_units_use_usr_bin`.
7. `package()` rewrites the `Exec=` line in the installed D-Bus
   service file from any source path to `/usr/bin/zwhisperd`. Test:
   `packaging/arch/tests/install_paths.sh::dbus_service_points_at_usr_bin`.
8. `namcap PKGBUILD` and `namcap zwhisper-0.1.0-1-x86_64.pkg.tar.zst`
   produce zero ERROR-level findings. Any WARNING is captured in
   `packaging/arch/namcap.expected` (one per line, format
   `<level> <message-prefix>`); a CI diff fails if a new WARNING
   appears. Test (skipped when namcap absent):
   `packaging/arch/tests/namcap_clean.sh::no_unexpected_findings`.

### Version handshake (pre-flight refusal of mismatched binaries)

9. `zwhisper-ipc` exports
   `pub const PROTOCOL_VERSION: &str = env!("CARGO_PKG_VERSION");`
   in `crates/zwhisper-ipc/src/lib.rs`, doc-comment names it the
   wire-level daemon-client contract, single source of truth for
   every binary in the workspace. Test:
   `crates/zwhisper-ipc/tests/m8_protocol_version.rs::const_matches_workspace_version`
   (test asserts equality with `env!("CARGO_PKG_VERSION")` from the
   test crate — catches workspace-version drift across crates).
10. `zwhisper-ipc` exports
    `pub struct ProtocolMismatch { pub expected: String, pub got: String }`
    with `thiserror::Error` + a clear `Display` impl
    (`"daemon protocol mismatch: expected {expected}, got {got}"`).
    Test: `crates/zwhisper-ipc/tests/m8_protocol_version.rs::mismatch_error_displays_expected_got`.
11. `Recorder1` exposes a read-only `ProtocolVersion` D-Bus property
    of type `s` returning `zwhisper_ipc::PROTOCOL_VERSION`. The
    property is on the *existing* `Recorder1` interface (not a new
    interface) so the M3 surface freeze diff is one additive line.
    Test:
    `crates/zwhisperd/tests/m8_dbus_protocol_version.rs::property_returns_workspace_version`
    (in-process zbus connection, no real bus).
12. **Client handshake — CLI.** Every `zwhisper` subcommand reads
    `ProtocolVersion` before any other RPC. On mismatch: stderr
    `"daemon protocol mismatch: expected X, got Y"`, exit code 3.
    On `MethodNotFound` (i.e. pre-0.1.0 daemon without the property):
    stderr `"daemon does not implement ProtocolVersion (pre-0.1.0?). Reinstall the daemon."`, exit code 3.
    Test:
    `crates/zwhisper-cli/tests/m8_version_handshake.rs::cli_refuses_mismatched_daemon_version`,
    `cli_refuses_legacy_daemon_without_property`.
13. **Client handshake — tray + settings.** The tray performs the
    handshake on its first connect; on mismatch it surfaces a single
    `notify-rust` notification `"zwhisper daemon version mismatch — please reinstall"`
    and enters a sticky "mismatch" state where left-click reopens the
    same notification (no infinite reconnect loop). Settings does the
    handshake on app load and disables every action button across all
    four tabs with a banner `"Daemon version X does not match settings version Y."`.
    Tests:
    `crates/zwhisper-tray/tests/m8_version_handshake.rs::tray_notifies_on_mismatch_once`,
    `crates/zwhisper-settings/tests/m8_version_handshake.rs::settings_disables_actions_on_mismatch`.

### Release process + documentation

14. `Cargo.toml` workspace `version = "0.0.0"` is bumped to `"0.1.0"`;
    `Cargo.lock` regenerated. The bump lands in the same commit as
    the PKGBUILD so a single tag covers code + package. Test:
    `crates/zwhisper-ipc/tests/m8_protocol_version.rs::const_matches_workspace_version`
    (DoD #9 transitively guards this).
15. `docs/RELEASE.md` exists with numbered steps:
    1. Update `CHANGELOG.md` `[Unreleased]` → `[X.Y.Z] - YYYY-MM-DD`.
    2. Bump `workspace.package.version` in root `Cargo.toml`.
    3. Run `cargo build --workspace --release --locked` (regenerates `Cargo.lock`).
    4. Run `scripts/refresh-checksums.sh` (verifies ggml SHA-256s).
    5. Commit `release: vX.Y.Z`, tag `vX.Y.Z`, push tag.
    6. GitHub Actions creates the source tarball at the tag URL.
    7. Run `updpkgsums packaging/arch/PKGBUILD` (locally), commit `packaging: refresh b2sums for vX.Y.Z`.
    8. Run `cd packaging/arch && makepkg -si` to dry-run the package install.

    Each step has a single verify command on the same line. Test:
    `docs/tests/release_doc.sh::release_doc_lists_required_steps`
    (grep-based smoke).
16. `CHANGELOG.md` exists at repo root in
    [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format
    with sections `[Unreleased]`, `[0.1.0] - 2026-05-03`. The 0.1.0
    section enumerates M0–M8 deliverables. Test:
    `docs/tests/changelog_format.sh::changelog_has_unreleased_and_first_release`.
17. `scripts/refresh-checksums.sh` re-downloads each ggml model
    listed in `crates/zwhisper-settings/checksums.toml`, recomputes
    SHA-256, and exits non-zero if any entry drifts. Idempotent on
    unchanged inputs. Uses `set -euo pipefail`, `mktemp -d`,
    `trap rm -rf` cleanup, `curl --fail --location --silent --show-error`.
    Test:
    `crates/zwhisper-settings/tests/m8_refresh_checksums_script.rs::refresh_script_idempotent_on_unchanged_inputs`
    (drives the script via `assert_cmd` against a `wiremock` HTTP
    fixture — no network in test).

### Packaging assets + performance + CI

18. `assets/icons/zwhisper.svg` is a hand-authored single-layer SVG
    (microphone glyph + waveform, brand neutral). No embedded raster,
    no `<script>`, declares `viewBox`, validates with `xmllint --noout`.
    The tray `.desktop` file's `Icon=zwhisper` line resolves to it
    after install. Test:
    `crates/zwhisper-tray/tests/m8_icon_asset.rs::icon_is_clean_svg`.
19. `packaging/arch/zwhisper.install` runs
    `gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor &>/dev/null || :`
    and `update-desktop-database -q &>/dev/null || :` in
    `post_install`, `post_upgrade`, and `post_remove`. The `|| :`
    swallow is intentional — the cache helpers are best-effort and
    must not fail the package transaction. Test:
    `packaging/arch/tests/install_hooks.sh::install_hooks_refresh_caches`.
20. **Idle RSS gate.** `crates/zwhisperd/tests/m8_perf_gate.rs::idle_rss_under_threshold`
    spawns `zwhisperd` as a subprocess with no active session, waits
    5 seconds, samples `/proc/<pid>/status:VmRSS`, and asserts
    `<= idle_rss_mib_max` from the fixture
    `crates/zwhisperd/tests/fixtures/m8_perf_thresholds.toml`
    (`idle_rss_mib_max = 60`). The fixture is loaded via `toml`, not
    hardcoded in the test body — see CLAUDE.md "Configuration — Zero
    Hardcoded Values". Test is `#[ignore]` by default; CI runs it
    with `--include-ignored` on a Linux runner that has PipeWire
    available; the local-dev contract is "run on demand before tag".
21. **Peak RSS gate.** `m8_perf_gate.rs::peak_rss_under_threshold`
    runs a 10-second mock recording (writing silence to /tmp) and
    asserts `VmHWM <= peak_rss_mib_max` (`peak_rss_mib_max = 350` from
    the same fixture). Same `#[ignore]` policy as DoD #20.
22. **CI additions** in `.github/workflows/ci.yml`:
    a. `packaging-shell` job runs the `packaging/**/tests/*.sh` shell
       smoke tests with `desktop-file-utils`, `xmllint` installed.
    b. `version-handshake` step runs
       `cargo test --workspace --tests m8_version_handshake m8_protocol_version`
       on every push (default profile).
    c. `perf-smoke` job runs `cargo test -p zwhisperd --release --test m8_perf_gate -- --include-ignored`
       on a Linux runner; `continue-on-error: true` for the first
       merge so a thresholds calibration is possible, then flipped
       to `false` in a follow-up commit on the same PR.
    d. `namcap` step runs only when `which namcap` succeeds (i.e. on
       the optional `arch-linux:latest` container matrix entry);
       otherwise the step is skipped with a `notice::`.
    Test: green CI on the M8 PR.

### Manual verification gate

23. `docs/M8-verification.md` exists with the MV-1..MV-10 matrix
    (see § "Manual verification gate" below); each MV-N has a single
    observable pass/fail criterion that the user runs by hand on a
    clean Arch box. The doc is updated as the gate is exercised.

> Total: 23 DoD items. Item count is bounded by what is independently
> testable; collapsing further weakens the wire-surface guarantees.

---

## Architectural decisions

### Where does PROTOCOL_VERSION live?

`zwhisper-ipc`. That crate is the documented "single source of truth
for the wire format" (see its module doc-comment). It already exports
`BUS_NAME`, `OBJECT_PATH`, `RECORDER_INTERFACE`, `PROFILES_INTERFACE`,
`ERROR_NAME_PREFIX`. `PROTOCOL_VERSION` is the same kind of constant —
a wire-level invariant — and shares the same lifetime guarantees.

`zwhisper-core` is application logic (audio, profile, transcribe). It
must not know about D-Bus. Putting `PROTOCOL_VERSION` there would
either (a) duplicate the const in `zwhisper-ipc` for the D-Bus
property impl or (b) make `zwhisper-ipc` depend on `zwhisper-core`,
which inverts the layering.

### Why `Recorder1.ProtocolVersion` and not a new `Versioned1` interface?

Properties are the cheapest D-Bus surface — no method body, just a
getter. Adding a sibling interface widens the introspection XML
(harder to freeze, harder to test). The M3 surface-freeze allow-list
gets one additive line. The property is on the recorder because every
client already opens a proxy on `Recorder1`; no client has to take a
new bus name or path.

### Compatibility with pre-0.1.0 daemons

A pre-0.1.0 daemon does not implement the property. zbus surfaces
this as `MethodCallNotImplemented` / `UnknownProperty`. Clients catch
the specific error variant and report
`"daemon does not implement ProtocolVersion (pre-0.1.0?). Reinstall the daemon."`
with exit code 3. They do **not** silently fall back to "assume
compatible" — silent fallback in a wire-protocol layer is exactly the
class of bug the handshake is meant to catch (CLAUDE.md "No Silent
Defaults").

### Reproducible builds

`packaging/arch/PKGBUILD`'s `build()` exports `SOURCE_DATE_EPOCH`
(from the git tag commit time, captured at release time and recorded
in `docs/RELEASE.md`), `RUSTFLAGS="-C strip=symbols"`, and runs
`cargo build --frozen` (rejects any `Cargo.lock` drift). No
`-C target-cpu=native`. No vendoring in M8 (deferred — cargo's
default network fetch is acceptable for the first packaged release;
vendor tarball generation can land in M8.1 if Arch maintainers
require it for AUR).

### The fltk-bundled trade-off

`fltk = { version = "1.5", features = ["fltk-bundled", "use-wayland"] }`
already bakes FLTK into the binary. `makedepends` covers the build,
not the run-time. Run-time `depends` only need the X11/Wayland
display libraries + fontconfig, because FLTK itself is statically
linked. This keeps `depends` tight (verified by namcap) and avoids
shipping a system FLTK fork that might version-skew with our
expectations.

### Stripped binaries vs `panic = "abort"`

`profile.release` in `Cargo.toml` already sets `strip = "symbols"`
and `panic = "abort"`. The PKGBUILD does not re-strip. Stripped +
abort means crash dumps lose stack traces. Mitigation: the daemon's
`tracing` subscriber writes to `~/.cache/zwhisper/logs/*.log` (M0)
which is not stripped, plus `RUST_BACKTRACE=1` is set in the
`zwhisperd.service` unit so the kernel-level abort trace lands in
journalctl.

### Single-instance vs system upgrade

`pacman -U` replacing `/usr/bin/zwhisper-settings` while the previous
instance is running is benign: the kernel pins the running ELF in
memory, the new file is at a new inode, the next launch picks up the
new binary. The D-Bus single-instance gate (M7 #15) keys on the bus
name `cz.zajca.Zwhisper1.Settings`, which the old process still
holds; the new process raises the existing window via the documented
M7 raise-signal path. After the user closes settings, `pacman` files
are fully active.

For the daemon, replacing `/usr/bin/zwhisperd` mid-session is also
fine: D-Bus auto-activation re-spawns the new binary after the
current session ends. `Type=dbus` means systemd waits for the bus
name handover. The version handshake (DoD #11–#13) is exactly the
cross-check that catches the case where a partially upgraded host
ends up with a new daemon and an old client (or vice versa).

---

## Wire-surface contract

Additive only. Existing wire surface unchanged.

- `zwhisper_ipc::PROTOCOL_VERSION: &'static str` — public const.
  Equal to `env!("CARGO_PKG_VERSION")`, which is the workspace
  version. Bumped in lockstep with `Cargo.toml`.
- `zwhisper_ipc::ProtocolMismatch { expected: String, got: String }` —
  public struct, derives `thiserror::Error`. `Display` impl produces
  the canonical user-facing error string.
- `cz.zajca.Zwhisper1.Recorder1.ProtocolVersion` — read-only D-Bus
  property, signature `s`, returns `PROTOCOL_VERSION`. No method, no
  signal.

The M3 surface-freeze test (`crates/zwhisper-core/tests/m7_surface_freeze.rs`
and the M3 introspection golden) gets a single additive entry. The
diff is one line — the test that would normally fail on widening is
adjusted to allow the new property explicitly.

No new dependency. No new IPC channel. No new env var. No new config
key.

---

## Risks

| ID | Likelihood | Impact | Risk | Mitigation |
|----|------------|--------|------|------------|
| R1 | M | H | `fltk-bundled` build fails on minimal Arch CI runner (cmake/gcc drift) | `makedepends` pins per DoD #2; CI matrix runs `cargo build --release` on every push so drift surfaces before tag time. Documented fallback: `fltk` system feature flag (deferred). |
| R2 | H | M | M3 surface-freeze test blocks on the new `ProtocolVersion` property | Batch B updates the freeze allow-list in the same PR as the property addition; `product-engineer` verifies the diff is a single additive line. |
| R3 | M | M | Idle-RSS perf gate flakes on CI (no PipeWire, no audio device) | Threshold loaded from TOML fixture, not hardcoded; tests are `#[ignore]` by default; CI's `perf-smoke` job is `continue-on-error: true` for the calibration commit and flipped to `false` in a follow-up. |
| R4 | M | H | Pre-0.1.0 daemons crash newer clients on missing property | C-batch handshake catches `MethodCallNotImplemented` / `UnknownProperty` zbus errors and reports the canonical mismatch error. Covered by `cli_refuses_legacy_daemon_without_property`. |
| R5 | L | M | `desktop-file-validate` rejects either `.desktop` post-install | M7 already validated both files at the source path. Batch D re-runs the validator on the installed file. The `Categories=Settings;AudioVideo;Audio;` line in `zwhisper-settings.desktop` is already known to emit a hint (multiple main categories) — captured in `namcap.expected`. |
| R6 | M | M | namcap ERROR finding on first PKGBUILD | `namcap.expected` allow-lists known WARNINGs only. Any new ERROR is a CI failure. Dry-run gate via `makepkg --printsrcinfo` and `namcap` runs locally before tag push (`docs/RELEASE.md` step 8). |
| R7 | L | M | `Exec=zwhisper-tray` without absolute path breaks under restricted PATH (e.g., `xdg-desktop-portal` sandbox in Flatpak) | Out of scope for M8 (Flatpak deferred). On Arch, `/usr/bin` is on the default PATH. M6's portal app-id resolution lookup keys on the `.desktop` basename, not the binary path. |
| R8 | L | H | `gst-plugin-pipewire` package name drift on Arch (rename in 2025) | Verified via `pacman -Si gst-plugin-pipewire` at PKGBUILD authoring time. If renamed by tag time, RELEASE.md step 8 (`makepkg -si` dry-run) catches it. |

---

## Implementation tasks

Six parallel batches A–F. No two batches edit the same file. All
batches converge before the `product-engineer` quality gate.

### Batch A — `zwhisper-ipc` PROTOCOL_VERSION + handshake error type

Owns:
- `crates/zwhisper-ipc/src/lib.rs` (extend with `PROTOCOL_VERSION` const and `ProtocolMismatch` struct)
- `crates/zwhisper-ipc/tests/m8_protocol_version.rs` (new)

Tasks:
- A1. Add `pub const PROTOCOL_VERSION: &str = env!("CARGO_PKG_VERSION");` with module-level doc-comment.
- A2. Add `pub struct ProtocolMismatch { pub expected: String, pub got: String }` with `#[derive(Debug, thiserror::Error)]` and the canonical `Display`.
- A3. Write `m8_protocol_version.rs::const_matches_workspace_version` and `mismatch_error_displays_expected_got`.
- A4. Re-verify `pub use` exports in `lib.rs` so downstream crates see both symbols without a sub-path import.
- A5. Run `cargo doc -p zwhisper-ipc` clean.

Success criteria: DoD #9, #10. Owns: A's three files.

### Batch B — Daemon `ProtocolVersion` property + surface-freeze update

Owns:
- `crates/zwhisperd/src/recorder_service.rs` (add property method)
- `crates/zwhisperd/tests/m8_dbus_protocol_version.rs` (new)
- `crates/zwhisper-core/tests/m7_surface_freeze.rs` (additive allow-list update)
- `crates/zwhisperd/Cargo.toml` (no change unless `zwhisper-ipc` is not already a dep — already is)

Depends on: A merged.

Tasks:
- B1. Add `#[zbus(property)] async fn protocol_version(&self) -> &str` returning `zwhisper_ipc::PROTOCOL_VERSION`.
- B2. Write `m8_dbus_protocol_version.rs::property_returns_workspace_version` and `property_is_readable_without_active_session` (both use `zbus::connection::Builder::session().p2p()` for in-process testing — no real bus).
- B3. Update `m7_surface_freeze.rs` allow-list: add the property fn-pointer to the frozen surface so future re-privatisation fails to compile.
- B4. Confirm no signal emission added; the property is pull-only.

Success criteria: DoD #11.

### Batch C — Client handshake (cli, tray, settings)

Owns:
- `crates/zwhisper-cli/src/main.rs` (or its top-level command dispatch — wherever `Recorder1Proxy::new` is first constructed)
- `crates/zwhisper-cli/tests/m8_version_handshake.rs` (new)
- `crates/zwhisper-tray/src/connect.rs` (or equivalent — first-connect path)
- `crates/zwhisper-tray/tests/m8_version_handshake.rs` (new)
- `crates/zwhisper-settings/src/app.rs` (or wherever the runtime bridge boots)
- `crates/zwhisper-settings/tests/m8_version_handshake.rs` (new)

Depends on: A + B merged.

Tasks:
- C1. CLI: extract a small `verify_protocol(proxy: &Recorder1Proxy<'_>) -> Result<(), ProtocolMismatch>` helper used by every subcommand before any other RPC. On `MethodCallNotImplemented` / `UnknownProperty`, treat as mismatch with `got = "pre-0.1.0"`.
- C2. CLI tests:
  - `cli_refuses_mismatched_daemon_version` — fake daemon returns `"99.0.0"`, assert exit code 3 + stderr contains the canonical message.
  - `cli_refuses_legacy_daemon_without_property` — fake daemon does not implement the property, assert exit code 3 + stderr contains `pre-0.1.0`.
- C3. Tray: handshake on first connect; on mismatch, send a single `notify-rust` notification, then enter a sticky "mismatch" state. Left-click while in this state reopens the same notification. No reconnect loop. Test: state machine + single notification.
- C4. Tray test: `tray_notifies_on_mismatch_once` — three reconnect attempts, expect exactly one notification.
- C5. Settings: handshake on app load; on mismatch disable every action button across all four tabs and show a banner. The banner copy is exact.
- C6. Settings test: `settings_disables_actions_on_mismatch` — mock daemon, assert all buttons disabled, banner text matches.

Success criteria: DoD #12, #13.

### Batch D — Arch PKGBUILD + install hooks + icon asset + perf gate

Owns:
- `packaging/arch/PKGBUILD` (new)
- `packaging/arch/zwhisper.install` (new)
- `packaging/arch/namcap.expected` (new)
- `packaging/arch/tests/{pkgbuild_metadata,pkgbuild_steps,install_paths,install_hooks,namcap_clean}.sh` (new)
- `assets/icons/zwhisper.svg` (new)
- `crates/zwhisper-tray/tests/m8_icon_asset.rs` (new)
- `crates/zwhisperd/tests/m8_perf_gate.rs` (new)
- `crates/zwhisperd/tests/fixtures/m8_perf_thresholds.toml` (new)

Tasks:
- D1. Author PKGBUILD per DoD #1–#7. `b2sums=('SKIP')` initial; release step regenerates real sums.
- D2. Author `zwhisper.install` per DoD #19.
- D3. Author shell tests under `packaging/arch/tests/` — bash `set -euo pipefail`, each test is one function, exits non-zero on first failure.
- D4. Author `assets/icons/zwhisper.svg` (single layer, microphone glyph + small waveform; viewBox `0 0 64 64`; one solid colour; no embedded raster). Reference Inkscape source.
- D5. Author `m8_icon_asset.rs::icon_is_clean_svg` driving `xmllint --noout` via `assert_cmd`.
- D6. Author `m8_perf_gate.rs::idle_rss_under_threshold` and `peak_rss_under_threshold` per DoD #20–#21. Threshold fixture lives at `crates/zwhisperd/tests/fixtures/m8_perf_thresholds.toml`. Tests are `#[ignore]` so default `cargo test` doesn't try to spawn a daemon under PipeWire-less CI.

Success criteria: DoD #1–#8, #18–#21.

### Batch E — Release docs + scripts + CI

Owns:
- `docs/RELEASE.md` (new)
- `CHANGELOG.md` (new)
- `scripts/refresh-checksums.sh` (new)
- `crates/zwhisper-settings/tests/m8_refresh_checksums_script.rs` (new)
- `.github/workflows/ci.yml` (extend with packaging-shell, version-handshake, perf-smoke, namcap jobs)
- `README.md` (Install section: new "Arch Linux" subsection)
- `packaging/README.md` (new)
- `docs/tests/{release_doc,changelog_format,readme_install}.sh` (new)
- `Cargo.toml` workspace `version` bump

Tasks:
- E1. Bump `Cargo.toml` `workspace.package.version` from `"0.0.0"` to `"0.1.0"`. Run `cargo build --release --locked` to refresh `Cargo.lock`. Both files committed.
- E2. Write `docs/RELEASE.md` per DoD #15.
- E3. Write `CHANGELOG.md` per DoD #16.
- E4. Write `scripts/refresh-checksums.sh` per DoD #17. Bash, `set -euo pipefail`, `mktemp -d`, `trap`, `curl --fail --location --silent --show-error`. No silent defaults.
- E5. Write `m8_refresh_checksums_script.rs::refresh_script_idempotent_on_unchanged_inputs` driving the script via `assert_cmd` against a `wiremock` fixture.
- E6. Extend `.github/workflows/ci.yml` per DoD #22.
- E7. Write `packaging/README.md` (layout, namcap allow-list policy, dry-run procedure).
- E8. Update `README.md` Install section with the Arch subsection.
- E9. Write the three grep-based smoke tests under `docs/tests/`.

Success criteria: DoD #14–#17, #22, plus the `README.md` / `packaging/README.md` doc updates.

### Batch F — Pre-merge multi-agent review

Owns: read-only.

Depends on: A–E.

- `security-reviewer` — focus on the install-script attack surface (gtk-update-icon-cache PATH hijack, install-hook `|| :` swallow, secrets-example file mode), the version-handshake wire surface, and any new `unsafe`/`expect`/`unwrap`.
- `performance-reviewer` — validate `m8_perf_gate.rs` thresholds against the M7-verification baseline. Confirm release profile unchanged.
- `silent-failure-hunter` — handshake error paths in CLI/tray/settings; the install-hook swallow; refresh-checksums script error paths.
- `devils-advocate` — partial-upgrade matrix, `pacman -Rns` orphan units, `Exec=zwhisper-tray` portal lookup, fltk-bundled drift, secrets-file UX.
- `product-engineer` — final READY/NEEDS-WORK/BLOCKED gate against the 23 DoD items + MV-1..MV-10.

Success criteria: zero ≥80-confidence blocking findings, or all blocking findings re-delegated and resolved.

---

## Crate dependency graph (M8 changes)

```
zwhisper-ipc (PROTOCOL_VERSION + ProtocolMismatch)
    │
    ├── zwhisperd (Recorder1.ProtocolVersion property)
    ├── zwhisper-cli (verify_protocol on every subcommand)
    ├── zwhisper-tray (handshake on first connect)
    └── zwhisper-settings (handshake on app load)

zwhisper-core (no change)
    └── m7_surface_freeze.rs (additive allow-list update)
```

No new edges. No layering inversion.

---

## Test matrix

| File | Test | Asserts |
|------|------|---------|
| `crates/zwhisper-ipc/tests/m8_protocol_version.rs` | `const_matches_workspace_version` | `PROTOCOL_VERSION == env!("CARGO_PKG_VERSION")` |
| `crates/zwhisper-ipc/tests/m8_protocol_version.rs` | `mismatch_error_displays_expected_got` | DoD #10 — Display impl correct |
| `crates/zwhisperd/tests/m8_dbus_protocol_version.rs` | `property_returns_workspace_version` | DoD #11 — property reads back the const |
| `crates/zwhisperd/tests/m8_dbus_protocol_version.rs` | `property_is_readable_without_active_session` | property doesn't require state |
| `crates/zwhisperd/tests/m8_perf_gate.rs` | `idle_rss_under_threshold` | DoD #20 — idle ≤ 60 MiB |
| `crates/zwhisperd/tests/m8_perf_gate.rs` | `peak_rss_under_threshold` | DoD #21 — peak ≤ 350 MiB |
| `crates/zwhisper-cli/tests/m8_version_handshake.rs` | `cli_refuses_mismatched_daemon_version` | DoD #12a |
| `crates/zwhisper-cli/tests/m8_version_handshake.rs` | `cli_refuses_legacy_daemon_without_property` | DoD #12b — pre-0.1.0 daemon |
| `crates/zwhisper-tray/tests/m8_version_handshake.rs` | `tray_notifies_on_mismatch_once` | DoD #13 — single notification |
| `crates/zwhisper-tray/tests/m8_icon_asset.rs` | `icon_is_clean_svg` | DoD #18 — xmllint --noout pass |
| `crates/zwhisper-settings/tests/m8_version_handshake.rs` | `settings_disables_actions_on_mismatch` | DoD #13 — banner + disabled buttons |
| `crates/zwhisper-settings/tests/m8_refresh_checksums_script.rs` | `refresh_script_idempotent_on_unchanged_inputs` | DoD #17 |
| `crates/zwhisper-core/tests/m7_surface_freeze.rs` | (extend) | additive entry for `ProtocolVersion` |
| `packaging/arch/tests/pkgbuild_metadata.sh` | `asserts_required_fields` | DoD #1 |
| `packaging/arch/tests/pkgbuild_metadata.sh` | `makedepends_covers_fltk_bundled_chain` | DoD #2 |
| `packaging/arch/tests/pkgbuild_metadata.sh` | `runtime_depends_match_runtime_features` | DoD #3 |
| `packaging/arch/tests/pkgbuild_steps.sh` | `build_uses_frozen_release_workspace` | DoD #4 |
| `packaging/arch/tests/install_paths.sh` | `package_installs_all_artefacts` | DoD #5 |
| `packaging/arch/tests/install_paths.sh` | `installed_units_use_usr_bin` | DoD #6 |
| `packaging/arch/tests/install_paths.sh` | `dbus_service_points_at_usr_bin` | DoD #7 |
| `packaging/arch/tests/install_hooks.sh` | `install_hooks_refresh_caches` | DoD #19 |
| `packaging/arch/tests/namcap_clean.sh` | `no_unexpected_findings` | DoD #8 |
| `docs/tests/release_doc.sh` | `release_doc_lists_required_steps` | DoD #15 |
| `docs/tests/changelog_format.sh` | `changelog_has_unreleased_and_first_release` | DoD #16 |
| `docs/tests/readme_install.sh` | `readme_documents_arch_install` | DoD #22 doc anchor |

---

## Manual verification gate

The user runs these on a clean Arch box. Each step has a single
observable pass condition.

| ID | Title | Pass condition |
|----|-------|----------------|
| MV-1 | Build & install | `cd packaging/arch && makepkg -si` succeeds; `pacman -Q zwhisper` reports `0.1.0-1`. |
| MV-2 | File ownership | `pacman -Qlq zwhisper \| xargs -I{} test -e {}` exits 0 (every listed file exists). |
| MV-3 | Daemon protocol property | `systemctl --user start zwhisperd` then `busctl --user introspect cz.zajca.Zwhisper1 /cz/zajca/Zwhisper1/Recorder1` lists `ProtocolVersion`; `busctl --user get-property cz.zajca.Zwhisper1 /cz/zajca/Zwhisper1/Recorder1 cz.zajca.Zwhisper1.Recorder1 ProtocolVersion` returns `s "0.1.0"`. |
| MV-4 | Tray autostart | `systemctl --user enable --now zwhisper-tray`; tray icon appears in StatusNotifier host within 5 seconds. |
| MV-5 | Settings launch | From application menu, click "zwhisper Settings"; window opens within 2 seconds; Status / Profile tab renders without panic. |
| MV-6 | Hotkey end-to-end | Default hotkey toggles a recording session; transcript reaches clipboard. M6 regression check inside the packaged bits. |
| MV-7 | Uninstall is clean | `pacman -Rns zwhisper`; `find /usr -path '*/zwhisper*' -o -name 'cz.zajca*' 2>/dev/null` returns nothing; journalctl shows no `gtk-update-icon-cache` errors during the transaction. |
| MV-8 | Mismatch UX (daemon ahead) | Manually replace `/usr/bin/zwhisperd` with a `0.0.99`-tagged build, restart user session; `zwhisper status` exits 3 with the canonical mismatch stderr; tray shows the mismatch notification once; settings disables actions and shows the banner. |
| MV-9 | Mismatch UX (daemon behind) | Manually replace `/usr/bin/zwhisperd` with a `0.0.0` build (no `ProtocolVersion`); `zwhisper status` exits 3 with `pre-0.1.0` stderr message. |
| MV-10 | Idle RSS budget | `systemctl --user start zwhisperd`, idle 60 s, `cat /proc/$(pidof zwhisperd)/status \| grep VmRSS` reports ≤ 60 MiB. |

---

## Out of scope (deferred post-M8)

1. **Flatpak manifest + Flathub submission** — also fixes M6's deferred double-instance bypass; defer together.
2. **AUR `zwhisper` and `zwhisper-git`** — M8 lands the PKGBUILD; the user uploads when they choose.
3. **`.deb` (Debian/Ubuntu) and RPM (Fedora) packages** — Arch first, others later.
4. **NixOS module / flake output**.
5. **`secrets.toml` editor in `zwhisper-settings`** — file-only editing today; M9 candidate (already deferred from M7).
6. **Hard RAM-cap enforcement (cgroup `MemoryMax=`)** — M8 ships an idle/peak smoke gate only.
7. **Code signing / `.pkg.tar.zst` PGP signing keys**.
8. **Auto-update mechanism** — no in-app "check for updates".
9. **Localisation / `.po` files for the settings GUI**.
10. **Telemetry / opt-in usage metrics**.
11. **Vendored cargo tarball** — first AUR upload may need this; not required for the local `makepkg -si` path.
12. **Bash completions / man pages** — covered by `cargo zigbuild`-style helpers in a follow-up; out of M8 scope to keep the PKGBUILD lean.

---

## Coordination notes

- Batches A and B are sequential (B depends on A's const + error
  type). Once A is merged, B and the M3 surface-freeze update can
  run in parallel with all D and E work.
- Batch C cannot start until B is merged (clients need the property
  to handshake against). C's three sub-batches (cli, tray, settings)
  can run in parallel since they edit disjoint files.
- Batch D (PKGBUILD + perf + icon) is fully independent of A/B/C and
  can start day one. It only blocks on E for the workspace version
  bump (the PKGBUILD pkgver references `0.1.0`, which only lands
  when E1 ships).
- Batch E1 (version bump) is a one-line change but lands first in
  the merge order so all subsequent commits have a coherent version
  string.
- Batch F runs after A–E land and before the final commit.
- The release commit (`release: v0.1.0`) is the M8 close commit.
  `feat(m8): packaging + release prep + protocol-version handshake`.

---

## Plan headlines (10-bullet summary)

1. Workspace bumped to `0.1.0`; first packageable release.
2. Hand-maintained Arch PKGBUILD under `packaging/arch/` with
   `fltk-bundled` makedepends, `--frozen --release --workspace`
   build, install-only `package()`.
3. Standard install layout: `/usr/bin`, `/usr/lib/systemd/user`,
   `/usr/share/dbus-1/services`, `/usr/share/applications`,
   `/usr/share/icons/hicolor/scalable/apps`, `/usr/share/zwhisper`,
   `/usr/share/licenses/zwhisper`.
4. `zwhisper.install` post-hook refreshes desktop + icon caches.
5. namcap allow-list + CI namcap step (skipped when absent).
6. `zwhisper_ipc::PROTOCOL_VERSION` const + `ProtocolMismatch` error
   type, exposed over D-Bus as `Recorder1.ProtocolVersion` property.
7. CLI exits 3, tray notifies (once), settings disables actions on
   protocol mismatch — including legacy pre-0.1.0 daemons that lack
   the property.
8. `docs/RELEASE.md` + `CHANGELOG.md` + `scripts/refresh-checksums.sh`
   formalise the release.
9. `m8_perf_gate.rs` idle/peak RSS smoke (TOML-fixture thresholds, no
   hardcoded numbers); CI runs it with `--include-ignored`.
10. Manual gate MV-1..MV-10 documented in `docs/M8-verification.md`;
    user runs them on a clean Arch box before tagging `v0.1.0`.

---

*End of M8 plan.*
