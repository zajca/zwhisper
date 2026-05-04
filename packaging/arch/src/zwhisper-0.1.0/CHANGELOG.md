# Changelog

All notable changes to **zwhisper** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-05-03

First packageable release. Closes M0–M8.

### Added

- **M0 — Walking skeleton recorder.** PipeWire → mono 16 kHz FLAC pipeline,
  GStreamer-backed encoder, M0 soak script.
- **M1 — Local transcription.** `whisper.cpp` post-process pipeline with
  cross-device-aware atomic move, tool discovery via `which`, deterministic
  output paths.
- **M2 — TOML profile system.** Schema-versioned profiles with embedded /
  shipped / user override layering, validation, migrations.
- **M3 — Daemon + CLI split with D-Bus IPC.** `zwhisperd` daemon claims
  `cz.zajca.Zwhisper1` on the session bus; `zwhisper-cli` (`zwhisper`
  binary) drives it through `Recorder1Proxy` / `Profiles1Proxy`.
  M3 wire surface frozen by `crates/zwhisper-ipc/tests/wire_freeze.rs`.
- **M4 — Tray + lifecycle pump.** StatusNotifierItem indicator,
  session-bound transcript sinks, `notify-rust` toasts, last-session
  recovery on reconnect.
- **M5 — Cloud transcription.** Deepgram batch backend with secrets
  resolver (env var → `~/.config/zwhisper/secrets.toml` mode 0600),
  retry budget, language detection.
- **M6 — Hotkey toggle.** Global hotkey via `xdg-desktop-portal`
  `GlobalShortcuts`, persistent rebind, `zwhisper hotkey` CLI surface,
  `zwhisper toggle` for WM-bound shortcuts.
- **M7 — Settings GUI.** On-demand FLTK settings binary
  (`zwhisper-settings`) with Profile / Models / Hotkey / WhisperCLI tabs,
  model downloader with SHA-256 verification against compile-time
  embedded `checksums.toml`, single-instance D-Bus gate, raise-signal
  wake-up, hotkey rebind end-to-end.
- **M8 — Packaging + protocol-version handshake.**
  - Arch `PKGBUILD` under `packaging/arch/` building all four binaries
    with `--frozen --release --workspace`.
  - System install layout (`/usr/bin`, `/usr/lib/systemd/user`,
    `/usr/share/dbus-1/services`, `/usr/share/applications`,
    `/usr/share/icons/hicolor/scalable/apps`,
    `/usr/share/zwhisper`, `/usr/share/licenses/zwhisper`).
  - `assets/icons/zwhisper.svg` (single-layer, scalable).
  - `zwhisper.install` post-hooks refreshing the icon and desktop
    caches.
  - `zwhisper_ipc::PROTOCOL_VERSION` const + `ProtocolMismatch` error
    type, exposed over D-Bus as the read-only
    `cz.zajca.Zwhisper1.Recorder1.ProtocolVersion` property.
  - CLI exits with code `4` on version mismatch (or pre-0.1.0 daemon
    that lacks the property); tray emits a single `notify-rust`
    notification and stops the reconnect loop; settings refuses to
    launch and surfaces the error through stderr + an FLTK alert.
  - `docs/RELEASE.md`, `scripts/refresh-checksums.sh`,
    `packaging/README.md`.

### Changed

- Workspace `version` bumped from `0.0.0` to `0.1.0`.
- CI extended with a `packaging-shell` job for the `packaging/**/tests/*.sh`
  smoke tests and a dedicated version-handshake test invocation.

### Out of scope (deferred post-0.1.0)

Flatpak manifest, AUR submission, Debian / RPM packages, NixOS module,
secrets editor in the settings GUI, hard RAM-cap enforcement,
auto-update mechanism, localisation, telemetry, vendored cargo
tarball. See `docs/M8-plan.md` § "Out of scope" for the full list.

[Unreleased]: https://github.com/zajca/zwhisper/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/zajca/zwhisper/releases/tag/v0.1.0
