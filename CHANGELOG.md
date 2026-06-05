# Changelog

All notable changes to **zwhisper** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2026-06-05

Backend availability is now explicit, and recordings orphaned by a killed
daemon recover themselves on the next start. Two silent-failure fixes,
prompted by a dictation profile that transcribed nothing on a build
without the Parakeet feature.

### Added

- **`Backend::is_compiled_in()` + `zwhisper backend list`.** A single
  source of truth for which transcription backends the running build can
  actually use, plus a command to inspect it without attempting a
  transcribe. A feature-gated backend (`parakeet`) reports the missing
  `--features` flag instead of failing only when first used.
- **Backend guard in `zwhisper audio setup`.** The wizard now refuses,
  with a rebuild hint and before any PipeWire mutation, to configure a
  profile whose transcription backend is not compiled into this build —
  instead of silently writing a profile that records but never
  transcribes.
- **Orphaned-recording recovery.** `zwhisperd` reaps a stale
  `active-session.json` at startup: a recording cannot survive a daemon
  restart, so a leftover marker is definitionally orphaned. The daemon
  preserves the audio (`last-session.json`), enqueues a tracked recovery
  transcribe job (so it lands in history and delivers normally), and
  clears the marker.
- **`Jobs1.JobFailed` delivery.** `zwhisper deliver` now raises a desktop
  notification when a transcription job fails (e.g. a not-compiled
  backend); previously a failed daemon auto-transcribe surfaced only in
  `StateChanged "failed"` + history.
- **Stale-session reporting in `zwhisper status`.** A defensive
  human + JSON note when an `active-session.json` is present while the
  daemon is not mid-session.

### Fixed

- **Packaging: Parakeet feature.** `packaging/arch/PKGBUILD` now builds
  with `--features parakeet`, so the shipped `dictation` profile can
  transcribe. A package built without it failed every dictation transcribe
  with `backend ``parakeet`` is not compiled in`, surfaced only on stderr.

## [0.5.0] - 2026-06-04

Type-at-cursor delivery: transcripts can now be typed directly into the
focused window on wlroots compositors, with a safe clipboard/notification
fallback everywhere else.

### Added

- **`type_at_cursor` output (`docs/RFC-type-at-cursor.md`).** A fourth
  `[[output]]` destination that types the transcript at the cursor via
  `wtype` (the Wayland `virtual-keyboard-v1` protocol). Supported on
  **wlroots only** (Sway/Hyprland); GNOME/Mutter and KDE/KWin lack the
  protocol for these clients and degrade to clipboard + notification.
  Keyboard-layout independent (`wtype` uploads its own keymap).
- **Stricter intent guard for typing.** Auto-typing runs only for a
  foreground job and only below an 8 KB ceiling (`TYPE_MAX_BYTES`); larger
  or detached/auto transcripts notify-with-action instead of spraying
  keystrokes into whatever window is focused.
- **Safe fallback chain.** A missing `wtype`, an unsupported compositor, or
  a `wtype` failure never loses the transcript: it is copied to the
  clipboard and announced via a notification (the transcript is also always
  on disk).
- **`zwhisper output last --to type`.** One-shot manual replay of the last
  transcript at the cursor, mirroring `--to clipboard` (size ceiling
  applies; clipboard fallback on failure).
- **`zwhisper audio setup` delivery choice.** The wizard now offers an
  interactive transcript destination (file only / file + type-at-cursor /
  file + clipboard) and writes the matching `[[output]]`; the dictation
  preset defaults to type-at-cursor.
- **`set_outputs` profile writer** that rewrites a profile's `[[output]]`
  array-of-tables in place, preserving the rest of the document.
- **Manual verification guide** `docs/RFC-type-at-cursor-verification.md`.

### Changed

- **`zwhisper instructions --agent`** now documents the delivery surface:
  `output last --to clipboard|type|notify`, `deliver --listen`, and the
  `[[output]]` destination types.
- **Packaging:** `wtype` added to `optdepends` (soft dependency; absent →
  clean clipboard/notification fallback).

## [0.4.1] - 2026-06-04

Dependency and toolchain maintenance release. No user-facing behaviour
changes; the FLAC decode/resample path was rewritten against the new
`symphonia` 0.6 / `rubato` 3.0 APIs with output kept bit-for-bit
deterministic.

### Changed

- **Workspace dependencies refreshed to latest.** Major bumps:
  `gstreamer`/`gstreamer-app` 0.23 → 0.25, `reqwest` 0.12 → 0.13
  (the `rustls-tls` feature became `rustls`: rustls + aws-lc-rs provider
  + `rustls-platform-verifier`, replacing the dropped bundled
  `webpki-roots`), `symphonia` 0.5 → 0.6, `rubato` 0.15 → 3.0,
  `zip` 2.2 → 8, `toml` 0.8 → 1, `toml_edit` 0.22 → 0.25, `sha2`
  0.10 → 0.11, `which` 6 → 8, `dirs` 5 → 6, `signal-hook(-tokio)`
  0.3 → 0.4, `clap` 4.5 → 4.6, plus `zbus`/`zvariant` and the usual
  semver-compatible lockfile updates.
- **MSRV raised 1.88 → 1.92**, required by `gstreamer` 0.25 and
  `ashpd` 0.13.
- **CI actions updated**: `actions/checkout` v4 → v6,
  `actions/upload-artifact` v4 → v7, `actions/download-artifact`
  v4 → v8, `softprops/action-gh-release` v2 → v3.

### Removed

- Dropped the `RUSTSEC-2024-0436` (`paste`) advisory ignore from
  `deny.toml` / `audit.yml` — `gstreamer` 0.25 no longer pulls `paste`.

## [0.4.0] - 2026-06-04

Thick-daemon role: transcription jobs, durable history, and session-bound
transcript delivery (RFC: daemon-role, Phases 1–3).

### Added

- **`Jobs1` D-Bus interface.** A transcription job queue running as a
  sibling of the recording slot (never the same lane): `TranscribeFile`,
  `Cancel`, `ListJobs`, plus `JobCompleted` / `JobFailed` / `JobProgress`
  signals. Configurable serialized concurrency (default 1, env
  `ZWHISPER_JOB_CONCURRENCY`). Distinct from `Recorder1.TranscriptComplete`.
- **`History1` D-Bus interface.** Durable session history at
  `$XDG_STATE_HOME/zwhisper/history.json`, owned by a single serialized
  writer task (no lost-update race). `ListSessions`, `GetSession`, `Forget`
  (audio kept unless `--delete-files`). Startup recovery marks interrupted
  sessions without auto-retry and reaps orphaned subprocesses. `Retry` is
  registered but returns a typed `RetryUnavailable` until the audio-model
  RFC lands (Phase 4).
- **`zwhisper deliver --listen`.** Session-bound consumer (auto-enabled
  systemd user unit, bound to `graphical-session.target`) that restores
  Clipboard/Notification delivery from the resolved `profile.outputs`
  carried in `JobCompleted`. Intent-based stale-clipboard guard (foreground
  jobs inject; detached/background jobs notify-with-action). Best-effort:
  a missed signal means the transcript is on disk only.
- **New CLI commands.** `transcribe --queue/--detach` (the local path stays
  the default — zero daemon dependency, preserving the headless guarantee),
  `jobs [cancel]`, `history [forget]`, `retry`, and `output last --to
  clipboard|notify` (the manual fallback for a missed delivery).
- **Per-interface protocol versioning.** `Jobs1` / `History1` each expose
  their own `ProtocolVersion`; clients degrade gracefully against an older
  daemon that lacks the interfaces.

### Changed

- Post-record auto-transcribe now runs as a job on the queue (so it is
  recorded in history and emits `Jobs1` signals), while the lifecycle still
  emits the frozen `Recorder1` terminal signals — the wire contract is
  unchanged. `whisper-cli` is spawned in its own process group with
  kill-on-drop so cancel/shutdown tears it down cleanly.

## [0.3.0] - 2026-06-04

Guided microphone setup & calibration (RFC: mic-setup).

### Added

- **`zwhisper audio` command group.** `devices` (enumerate inputs/outputs,
  `--json`), `meter` (live VU meter from `pw-cat` raw PCM with peak/RMS
  dBFS and a clip indicator), `calibrate` (measure noise floor + speech,
  recommend/apply a safe PipeWire volume with saturation protection,
  optional `--set-default` and profile write; dry-run by default), and
  `setup` (interactive wizard tying it together with dictation/meeting
  presets). CLI-side and daemon-free.
- **Core `setup` module + Cargo feature.** A GStreamer-free `setup` feature
  exposing a mockable `PipewireControl` trait (`pw-dump` / `wpctl` parsing,
  dBFS analysis, the iterative calibration algorithm) — fully unit-tested
  with no hardware. Shared `node_name` validation and `gain` (dB↔linear)
  helpers as single sources of truth.
- **Software input gain.** New optional `sources.input_gain_db` profile
  field applied as a GStreamer `volume` element on the mic branch (layered
  on top of the PipeWire-native device volume), with a comment-preserving
  single-table profile writer.
- **Mic-only capture mode.** Profiles may set `system_output = ""` for a
  mic-only (no system audio) capture — the dictation use case — via a
  no-`audiomixer` single-source pipeline branch.

### Changed

- `Profile::validate` now accepts an empty `sources.system_output` as
  mic-only instead of rejecting it; the empty value is preserved verbatim
  and never silently coerced to the sink monitor.
- Workspace `version` bumped from `0.2.1` to `0.3.0`.

## [0.2.1] - 2026-06-03

### Fixed

- **mono_mix capture level.** The capture pipeline fed `audiomixer` two
  multi-channel (stereo) sources and downmixed to mono only at the mixer
  output. On a single quiet microphone this attenuated the mic by ~22 dB,
  pushing speech below the noise floor so recordings transcribed as empty
  (or as whisper.cpp filler like `[ Thank you.]`). Each source branch is
  now downmixed to mono (`audio/x-raw,channels=1`) before the
  `audiomixer`, restoring the mic to its captured level.

### Added

- **Desktop integration (`contrib/`).** Ready-to-use Wayland helpers:
  `zwhisper-dictate` (push-to-dictate, mic-only, transcript → clipboard),
  `zwhisper-cycle-profile`, Sway key bindings, a Waybar module + optional
  CSS, example Parakeet/whisper.cpp profiles, and a one-shot `install.sh`.
- **README quickstart + desktop integration guide**, plus a
  microphone-level troubleshooting section for empty transcripts.

## [0.2.0] - 2026-06-03

Backend-agnostic audio + model boundaries (RFC: audio-source-model) and a
new in-process Parakeet backend.

### Added

- **AudioSource boundary.** A backend-agnostic audio representation
  (`AudioSource` + `AudioArtifact` + `PcmAvailability` + `AudioMetadata`)
  with a pull-based `PcmChunkSource` trait. Decouples how audio is captured
  and persisted from how a backend consumes it (encoded file vs. in-memory
  PCM vs. decode-from-artifact).
- **ModelSpec registry.** A unified model boundary (`ModelKind` /
  `ModelArtifact` / `ModelSource` / `ModelSpec` / `ModelStatus` /
  `ModelRegistry`) covering single-file, directory-bundle, and remote
  models. Registry-load validation enforces a name allow-list (CWE-22) and
  HTTPS-only sources before any resolution or install can act on a spec.
- **Parakeet backend (opt-in).** In-process `parakeet-tdt-0.6b-v3`
  transcription via `transcribe-rs` + ONNX Runtime, behind the default-OFF
  `parakeet` Cargo feature in `zwhisper-cli` and `zwhisperd`. Release ships
  both a lean default build and a separate `-parakeet` build.
- **FLAC decode + resample.** `symphonia` FLAC decode → mono downmix →
  `rubato` resample to the backend's ASR sample rate, exposed as an
  `ArtifactDecodeSource` PCM source.
- **Hardened model bundle installer.** Multi-file and archive
  (`zip` / `tar.gz`) model installs with lexical zip-slip protection,
  symlink/hardlink rejection, decompression-bomb caps, HTTPS-only client
  with non-HTTPS-redirect rejection, verify-before-extract, and atomic
  same-filesystem install.
- **Live PCM fan-out.** Native-rate capture with a GStreamer `tee` +
  `appsink` ASR branch producing 16 kHz mono `f32` PCM in parallel with the
  FLAC recording branch, with safe fallback to decode-from-artifact.
- **Release workflow.** Tag-triggered `release.yml` builds the default and
  `parakeet` flavours, packages tarballs with SHA-256 sidecars, and
  publishes the matching `CHANGELOG.md` section as the release notes.

### Changed

- Backend configuration converged from a `BackendConfig` enum to a
  `BackendSettings` side-map keyed by `profile::schema::Backend`; added
  `Backend::Parakeet` and `Backend::from_id`.
- `Transcriber` trait reworked around
  `transcribe(&AudioSource, &ModelArtifact, &TranscribeOpts)`; a
  coordinator resolves audio + model and reconciles the ASR sample rate
  across both axes.
- `Profile::validate` relaxed from 16 kHz-only to `{16k, 44.1k, 48k}`
  capture rates.
- Workspace `version` bumped from `0.1.0` to `0.2.0`.

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

[Unreleased]: https://github.com/zajca/zwhisper/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/zajca/zwhisper/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/zajca/zwhisper/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/zajca/zwhisper/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/zajca/zwhisper/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/zajca/zwhisper/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/zajca/zwhisper/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/zajca/zwhisper/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/zajca/zwhisper/releases/tag/v0.1.0
