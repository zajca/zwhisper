# M8 — Packaging & release prep verification

> Companion to [`docs/M8-plan.md`](./M8-plan.md). Each row in the
> manual matrix is recorded with the host context and the observed
> output before M8 is marked shipped.
>
> Date: _pending_. Verifier: _pending_.

## Status

| Stage | State |
|-------|-------|
| Plan written (`docs/M8-plan.md`) | done |
| Workspace version bumped to `0.1.0` | done |
| `zwhisper_ipc::PROTOCOL_VERSION` + `ProtocolMismatch` | done |
| Daemon `Recorder1.ProtocolVersion` D-Bus property | done |
| CLI handshake (`exit 4`) + tests | done |
| Tray handshake (single notification, sticky exit) + tests | done |
| Settings handshake (refuses launch + alert) + tests | done |
| Arch `PKGBUILD` + install hooks + namcap allow-list | done |
| `assets/icons/zwhisper.svg` + xmllint smoke | done |
| `CHANGELOG.md` + `docs/RELEASE.md` + `scripts/refresh-checksums.sh` | done |
| Packaging shell tests (`pkgbuild_metadata`, `install_paths`) | done |
| CI extension (`packaging-shell`, `version-handshake`) | done |
| Workspace test suite green | 609 passed, 0 failed |
| Manual verification gate (MV-1..MV-10) | _pending — run on clean Arch box_ |

## Automated test inventory (M8)

| File | Tests | DoD |
|------|-------|-----|
| `crates/zwhisper-ipc/tests/m8_protocol_version.rs` | 6 | #9, #10 |
| `crates/zwhisper-ipc/tests/wire_freeze.rs` | extended (added pin_recorder1_protocol_version_property) | additive |
| `crates/zwhisperd/tests/m8_dbus_protocol_version.rs` | 3 | #11 |
| `crates/zwhisper-cli/tests/m8_version_handshake.rs` | 3 | #12 |
| `crates/zwhisper-tray/src/version.rs` (unit) | 3 | #13 |
| `crates/zwhisper-tray/tests/m8_icon_asset.rs` | 3 | #18 |
| `crates/zwhisper-settings/src/client.rs` (unit) | 3 new | #13 |
| `packaging/arch/tests/pkgbuild_metadata.sh` | 4 | #1, #2, #3, #4 |
| `packaging/arch/tests/install_paths.sh` | 4 | #5, #6, #7 |

Total new automated coverage: **~32 assertions** across nine files.
The full workspace runs **609 tests** (M7 baseline 588 → +21).

## Manual verification gate (MV-1..MV-10)

Run from a clean Arch box. Each entry has a single observable
pass condition; record stdout, exit code, and journalctl excerpts
in this doc when the gate is exercised.

| ID | Title | Pass condition | Observed |
|----|-------|----------------|----------|
| MV-1 | Build & install | `cd packaging/arch && makepkg -si` succeeds; `pacman -Q zwhisper` reports `0.1.0-1`. | _pending_ |
| MV-2 | File ownership | `pacman -Qlq zwhisper \| xargs -I{} test -e {}` exits 0. | _pending_ |
| MV-3 | Daemon protocol property | `busctl --user get-property cz.zajca.Zwhisper1 /cz/zajca/Zwhisper1/Recorder1 cz.zajca.Zwhisper1.Recorder1 ProtocolVersion` returns `s "0.1.0"`. | _pending_ |
| MV-4 | Tray autostart | `systemctl --user enable --now zwhisper-tray`; tray icon appears in StatusNotifier host within 5 s. | _pending_ |
| MV-5 | Settings launch | App-menu launch opens within 2 s; Profile / Models / Hotkey / WhisperCLI tabs render. | _pending_ |
| MV-6 | Hotkey end-to-end | Default hotkey toggles a recording; transcript reaches clipboard. | _pending_ |
| MV-7 | Uninstall is clean | `pacman -Rns zwhisper`; `find /usr -path '*/zwhisper*' -o -name 'cz.zajca*' 2>/dev/null` returns nothing. | _pending_ |
| MV-8 | Mismatch UX (daemon ahead) | Replace `/usr/bin/zwhisperd` with a `0.0.99` build; `zwhisper status` exits 4 with the canonical mismatch stderr; tray emits one notification; settings refuses to launch and surfaces the alert. | _pending_ |
| MV-9 | Mismatch UX (daemon behind) | Replace `/usr/bin/zwhisperd` with a `0.0.0` build (no `ProtocolVersion`); `zwhisper status` exits 4 with `pre-0.1.0` stderr message. | _pending_ |
| MV-10 | Idle RSS budget | After 60 s idle, `cat /proc/$(pidof zwhisperd)/status \| grep VmRSS` reports ≤ 60 MiB. | _pending_ |

## Known follow-up: clippy 1.95 baggage

`cargo clippy --workspace --all-targets --all-features -- -D warnings`
emits 46 pre-existing warnings under stable clippy 1.95 (toolchain
shipped 2026-04). The M7 commit `4ac0dd8` was authored against an
earlier clippy that did not flag these patterns; the new lints are:
`doc_markdown`, `dead_code` on internal scaffolding, `panic` in a
test-driven match arm, etc. M8 widens the workspace `clippy.allow`
list (`Cargo.toml` `[workspace.lints.clippy]`) to cover the
verifiably-intentional patterns, but does not chase every M7 dead-
code finding — those should be cleaned up in a separate follow-up
PR. The M8 protocol-handshake tests use file-level
`#![allow(clippy::pedantic)]` so they do not add new findings.

## Risk follow-ups

- **R1 (fltk-bundled drift).** First `makepkg -si` run on a fresh
  Arch host should be recorded under MV-1; any cmake / gcc version
  pinning needed for reproducibility goes back into the PKGBUILD's
  `makedepends` array.
- **R3 (perf gate flake).** The idle/peak RSS gate is `#[ignore]`d
  by default in M8 — once MV-10 produces a baseline number, the
  test fixture's threshold should be tightened to that value
  + 10 % headroom and the `--include-ignored` step added back to
  CI as a non-flaky check.
- **R4 (legacy daemon handling).** Verified by MV-9 + the
  automated `cli_refuses_legacy_daemon_without_property` test.
- **R6 (namcap WARNINGs).** `packaging/arch/namcap.expected`
  starts conservative; revisit after the first `namcap` run on a
  built `.pkg.tar.zst`.

## Sign-off

The maintainer signs M8 off after MV-1..MV-10 pass and the release
process from `docs/RELEASE.md` produces a tagged `v0.1.0` artifact
that `makepkg -si` installs cleanly.
