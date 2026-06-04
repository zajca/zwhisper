# RFC: `zwhisper audio` — Microphone Setup & Calibration Tooling

## Status

Proposed.

This RFC describes a target design for a guided microphone setup feature. It
turns today's manual, error-prone audio setup (hand-editing a profile to name a
PipeWire node, then hand-tuning input gain in `pavucontrol`) into a single
guided command that picks the right device, measures the signal, sets a safe
level, and persists the choice everywhere it matters.

Scope decisions already made (2026-06-03):

- **Reach:** full interactive wizard **plus** mic-only profile support.
- **Where gain lives:** *both* — PipeWire-native volume (`wpctl`) for an
  immediate global fix **and** a zwhisper-owned `input_gain_db` in the profile
  for reproducibility.
- **Level metering:** CLI-side via `pw-cat` raw PCM (no GStreamer in the CLI, no
  running daemon required).

## Summary

zwhisper today gives the user no help configuring capture. The microphone is
selected by writing a raw PipeWire `node.name` into a profile's `[sources]`
table (or leaving `"default"`), and input level is entirely external: the
`README` documents `wpctl set-volume <id> 0.25` and `pavucontrol`, but nothing
measures, recommends, or applies a level. On the developer's own box this has
already bitten twice — an ALC1220 mic that saturates with broadband noise above
~30 % PipeWire volume (usable window ~25–28 %), and confusion over which node is
the real mic versus a sink monitor.

This RFC adds a `zwhisper audio` command group:

```text
zwhisper audio devices    # enumerate inputs/outputs (id, name, description, default, volume)
zwhisper audio meter      # live VU meter for manual fine-tuning
zwhisper audio calibrate  # measure speech level, recommend/apply a safe volume, persist to a profile
zwhisper audio setup      # interactive wizard tying the above together
```

The load-bearing observation is that everything needed — enumerate devices,
read raw PCM to measure loudness, read/set volume, set the default source — is a
**shell-out plus parsing** problem (`pw-dump`, `pw-cat`, `wpctl`). None of it
needs GStreamer. That keeps the CLI a thin client (it deliberately dropped the
`audio`/`gstreamer` feature — see `crates/zwhisper-cli/Cargo.toml:19`) and lets
the analysis logic live in `zwhisper-core` behind a mockable trait, fully
unit-testable without a running PipeWire daemon.

## Goals

- One command (`zwhisper audio setup`) takes a non-expert from "nothing
  configured" to "the right mic, at a safe level, working everywhere" in well
  under a minute.
- Pick the right input by **human description**, not by memorizing a
  `node.name`; clearly mark the current default and flag sink-monitor sources.
- **Measure** the signal (peak/RMS dBFS, plus a noise-floor sample) and
  **auto-set** a safe input level, with explicit protection against
  saturation-prone hardware (ALC1220).
- Persist the choice so it works across **all** capture paths: the daemon
  (profiles with `mic = "default"`), `zwhisper-dictate` (which reads
  `pactl get-default-source`), and other desktop apps.
- Keep the durable, reproducible state zwhisper-owned: the selected node and the
  calibrated gain land in the profile (`sources.mic`, `sources.input_gain_db`),
  independent of what other apps later do to PipeWire.
- Work **without GStreamer in the CLI** and **without a running daemon** — setup
  must be usable even when the daemon is wedged.
- Keep all new logic unit-testable behind a mockable PipeWire-tooling trait; no
  real hardware required for the core test suite.
- Add a clean **mic-only** capture path (no system audio mixed in) for the
  dictation use case, lifting today's hard rejection of mic-only profiles.

## Non-Goals

- Not a replacement for `pavucontrol`/`wpctl` — it automates the common case,
  not every routing scenario.
- Not a GUI; this is a terminal command (the `zwhisper-settings`/`zwhisper-tray`
  crates may surface it later, out of scope here).
- Does not add GStreamer to the CLI or a live-level D-Bus RPC to the daemon.
- Does not change the transcription backends, model registry, or the FLAC
  artifact contract.
- Does not attempt per-application volume routing; PipeWire-native volume is a
  device-level setting and is intentionally global.

## Current Architecture (what exists today)

**CLI surface.** `crates/zwhisper-cli/src/main.rs:51` defines the command enum
(`record`, `transcribe`, `profile`, `model`, `backend`, `status`,
`instructions`, `toggle`, `hotkey`). Adding a new `audio` subcommand group is a
one-variant change here plus a clap `Subcommand` in
`crates/zwhisper-cli/src/cli.rs`. The CLI is a thin D-Bus client; it depends on
`zwhisper-core` with `default-features = false, features = ["profile",
"transcribe"]` — **no `audio`/`gstreamer`** (`crates/zwhisper-cli/Cargo.toml`).
It already pulls `serde_json`, `toml_edit`, `tempfile`, `tokio`, `reqwest`.

**Device handling.** `crates/zwhisper-core/src/audio/devices.rs` has a
`WpctlRunner` trait (`inspect(alias)` → `wpctl inspect`,
`list_node_names()` → `pw-cli ls Node`), a `resolve(mic, monitor)` that turns
`"default"` into concrete node names (`devices.rs:138`), and
`validate_node_name` — an allow-list `[A-Za-z0-9._:-]+`, max 256 chars
(`devices.rs:269`). It does **not** enumerate with description/`media.class`,
and has **no** volume read/set. Crucially, this module sits under the `audio`
feature (GStreamer), so the CLI cannot use it as-is.

**Profile schema.** `crates/zwhisper-core/src/profile/schema.rs`:

- `Sources { mic: String, system_output: String, mode: Mode }` (`schema.rs:74`).
- `Mode { MonoMix, StereoSplit }` (`schema.rs:18`); only `MonoMix` is honored.
- `Recording { codec, sample_rate, max_duration_minutes }`;
  `SUPPORTED_SAMPLE_RATES = [16_000, 44_100, 48_000]` (`schema.rs:10`).
- **No gain/volume field anywhere.**
- `Profile::validate` (`schema.rs:378`) **hard-rejects** an empty
  `system_output` as "mic-only mode not supported" (`schema.rs:418`) and rejects
  `StereoSplit` (`schema.rs:435`).

**Profile writer.** `crates/zwhisper-core/src/profile/listing.rs:75`
(`clone_to_user`) serializes a whole `Profile` via
`toml_edit::ser::to_string_pretty` and writes atomically (`create_new` +
`sync_all`). `paths.rs:13` has `validate_name`; `paths.rs:31`
`user_override_path`. There is no read-modify-write-one-field helper yet (a
full reserialize loses comments).

**Level metering.** None. The GStreamer pipeline
(`crates/zwhisper-core/src/audio/pipeline.rs:163`,
`pipeline_description`) has **no `level` element** and emits no dB. The recorder
(`recorder.rs`) always writes a FLAC file; `RecordingReport` carries
`pcm: Option<Arc<[f32]>>` (mono f32 at ASR rate when `capture_pcm: true`) but no
loudness measurement and no monitor-only mode. The mono-mix downmix-before-mixer
fix (`audio/x-raw,channels=1 ! mix.` per source pad) is present at
`pipeline.rs:179`.

**Volume.** 100 % external. No `wpctl set-volume` in the codebase; no `volume`
element in the pipeline.

**Daemon D-Bus.** `Recorder1` (`crates/zwhisper-ipc/src/recorder.rs`):
`start_recording(profile) -> session_id`, `stop_recording`, `get_status`, plus
signals. `Profiles1`: `list`, `list_v2`, `get_active`, `set_active`, `reload`.
**No device enumeration over D-Bus** — resolution is client-side.

**Dictation path.** `contrib/bin/zwhisper-dictate` bypasses the daemon: it reads
`ZWHISPER_DICTATE_SOURCE` or `pactl get-default-source`, then
`pw-record --target "$src"` (mic-only). This is why setting the PipeWire
**default source** is the single highest-leverage action: it fixes dictation,
the daemon (`mic = "default"`), and every other app at once.

## Proposed Architecture

### Module layout

| Layer | Module | Responsibility | Feature gate |
|---|---|---|---|
| core | `zwhisper-core/src/setup/mod.rs` (new) | `PipewireControl` trait, shared types, `SetupError` | new **`setup`** (`dep:serde_json` only — no GStreamer) |
| core | `setup/devices.rs` | parse `pw-dump` JSON → `Vec<AudioDevice>`; reuse `validate_node_name` | `setup` |
| core | `setup/volume.rs` | parse/format `wpctl` volume; `Volume` type | `setup` |
| core | `setup/level.rs` | `LevelStats`, `analyze(&[f32])`, `recommend_volume(...)` (pure fns) | `setup` |
| core | `setup/config.rs` | tunables (target window, thresholds, timeouts, caps) — no hardcoded values | `setup` |
| CLI | `zwhisper-cli/src/commands/audio.rs` (new) | spawn `pw-cat`, read stdout, VU render, prompts, dispatch | CLI `setup` (default-on) |
| schema | `profile/schema.rs` | add `sources.input_gain_db`; allow mic-only | — |
| daemon | `audio/pipeline.rs`, `recorder.rs` | `volume` element for `input_gain_db`; mic-only branch | `audio` |

The CLI gains a `setup` feature (default-on; it only needs `serde_json`, already
present). `zwhisper-core` gains a `setup` feature that pulls **no** GStreamer, so
the CLI can depend on it without re-acquiring the `audio` graph.

Why a new `setup` module instead of extending `audio/devices.rs`: the existing
module is gated behind the `audio` (GStreamer) feature the CLI deliberately
dropped. `validate_node_name` is reused (extracted to a shared location or
re-exported) so node-name validation has a single source of truth.

### Tooling indirection (mockable)

```rust
// core/src/setup/mod.rs
pub trait PipewireControl: Send + Sync {
    /// `pw-dump` parsed to the audio nodes we care about.
    fn dump_nodes(&self) -> Result<Vec<RawNode>, SetupError>;
    /// `wpctl inspect @DEFAULT_AUDIO_SOURCE@` → node.name (for is_default).
    fn default_source_name(&self) -> Result<String, SetupError>;
    fn get_volume(&self, id: u32) -> Result<Volume, SetupError>;
    fn set_volume(&self, id: u32, linear: f32) -> Result<(), SetupError>;
    fn set_default(&self, id: u32) -> Result<(), SetupError>;
}

pub struct AudioDevice {
    pub id: u32,                 // object.id — required by wpctl + pw-cat --target
    pub node_name: String,       // node.name — required by pipewiresrc target-object
    pub description: String,     // node.description — what the user reads
    pub is_source: bool,         // media.class == "Audio/Source"
    pub is_monitor: bool,        // node.name ends with ".monitor"
    pub is_default: bool,        // cross-referenced from default_source_name()
    pub volume: Option<Volume>,  // current linear volume + muted
}

pub struct Volume { pub linear: f32, pub muted: bool }
```

A production `SystemPipewire` implements the trait via `std::process::Command`
(no shell). A `MockPipewire` backs unit tests with canned `pw-dump` JSON and
volume strings.

Level measurement (`pw-cat`) runs as a CLI-owned child process; its raw `f32`
stdout is decoded into `&[f32]` and fed to the pure core functions
`analyze()` / `recommend_volume()`. The child has a timeout, is killed on
completion, and its stdout read is size-bounded.

### Command behavior

```text
zwhisper audio devices [--json]
    Enumerate. Human table by default: id, description, [DEFAULT], [monitor],
    volume%. `--json` for scripts.

zwhisper audio meter [--source default|<node.name>|<id>]
    Live VU meter from pw-cat raw PCM. ASCII bar refreshed on \r with peak/RMS
    dBFS and a clip indicator. Ctrl+C to stop. Read-only.

zwhisper audio calibrate [--source <sel>] [--profile <name>]
                         [--target-peak-db <f>] [--seconds <n>]
                         [--apply] [--set-default] [--max-volume <f>]
    1. Record a short noise-floor sample (silence), then prompt the user to
       speak for `--seconds`.
    2. Compute peak/RMS dBFS for both; report.
    3. Recommend a volume. With --apply, set it via wpctl and re-measure
       (iterate 2-3x). With --set-default, also make this the default source.
    4. With --profile, write `sources.mic` (concrete node) and
       `sources.input_gain_db` into the named user-override profile.
    Without --apply it is a dry run (recommendation only).

zwhisper audio setup
    Interactive wizard: enumerate -> pick mic -> calibrate (with live meter) ->
    choose preset (dictation = mic-only / meeting = mono_mix) -> apply PipeWire
    volume + set-default -> write profile(s) -> print a plain-language summary.
```

`<sel>` resolution: `default` → `default_source_name()`; a `node.name` →
look up its id in the dump; a bare integer → used directly as id (validated
numeric).

## Calibration Algorithm

This is the core "auto-set the level" logic, kept as pure, tested functions in
`setup/level.rs`.

- **dB computation.** For mono `f32` in `[-1.0, 1.0]` (0 dBFS = full scale):
  `peak_db = 20·log10(max|s|)`, `rms_db = 20·log10(sqrt(mean(s^2)))`. Silence
  guards: an all-zero buffer reports `-inf`/a floor sentinel, never `NaN`.
- **Two windows.** Measure a **noise-floor** sample (first ~0.5 s, before the
  speak prompt) and a **speech** sample. The gap between them is the usable
  headroom; a high floor that stays high after lowering volume is the ALC1220
  broadband-noise signature.
- **Volume recommendation.** `wpctl` volume is **linear amplitude** (0.5 ≈
  −6 dB). To move the measured speech peak toward the target:
  `new = clamp(current · 10^((target_peak_db − measured_peak_db)/20),
  min_volume, max_volume)`.
- **Iterate.** Hardware gain stages (ALC1220) are not perfectly linear, so apply
  → re-measure → adjust, up to a configured iteration cap, until the peak is
  within tolerance of the target.
- **Saturation protection.** Never raise above `--max-volume` (default 1.0;
  the wizard suggests a lower cap when the noise floor is high). If the mic is
  too quiet even at the cap, report it rather than looping. All volumes are
  clamped to `[0.0, max]` and checked `is_finite()`.
- **No magic numbers.** Target window (default speech peak ≈ −9…−6 dBFS, idle
  floor < −45 dBFS), tolerance, iteration cap, sample length, `pw-cat` timeout,
  and the default volume cap live in `setup/config.rs`, not inline (CLAUDE.md:
  zero hardcoded values, no silent defaults).

## Profile Changes (the "both" gain decision)

```toml
[sources]
mic = "alsa_input.pci-0000_00_1f.3.analog-stereo"  # wizard writes a concrete node
system_output = "default"                           # or "" for mic-only (see below)
mode = "mono_mix"
input_gain_db = -2.0                                # NEW: optional SW trim, default 0
```

- New `Sources.input_gain_db: Option<f32>`, expressed in dB (human-readable);
  `#[serde(default, skip_serializing_if = "Option::is_none")]` so existing
  profiles round-trip byte-for-byte. Validated `is_finite()` and within a sane
  range (e.g. −30…+30 dB) in `Profile::validate`.
- The daemon pipeline applies it as a GStreamer `volume` element (converted from
  dB to a linear factor) on the mic branch, **after** `pipewiresrc`. This is the
  zwhisper-owned trim that other apps cannot disturb.
- **"Both" semantics:** `calibrate --apply` sets the PipeWire device volume
  (global hardware-gain fix for every path) **and** records the selected node +
  resulting trim in the profile (reproducible zwhisper state). PipeWire gets the
  device into a healthy range; `input_gain_db` is a fine SW trim layered on top.
- **Writer.** Add a `set_sources_fields(name, mic, input_gain_db)` helper using
  `toml_edit::DocumentMut` (read-modify-write a single table) so **comments and
  formatting survive** — unlike `clone_to_user`'s full reserialize. Same atomic
  temp+`rename`+`sync_all` discipline. Only user-override profiles are mutable
  (`paths::user_override_path`); shipped/embedded profiles prompt a clone first.
- **Migration.** The field is additive and optional, so a `schema_version` bump
  is not strictly required; if bumped, the migration step is a no-op fill.

## Mic-Only Mode

The dictation use case wants the mic with **no** system audio. Today this is
hard-rejected. Lifting it touches four places:

- `schema.rs`: allow `system_output == ""` to mean mic-only (relax the
  `schema.rs:418` rejection); keep the `StereoSplit` rejection. Decide between
  reusing empty `system_output` versus a dedicated `Mode` variant — empty
  `system_output` matches the existing field shape and the RFC-audio-source
  framing, so prefer that.
- `audio/devices.rs::resolve`: return a mic-only `DeviceSelection` for an empty
  monitor instead of `DeviceError::InvalidArgument` (`devices.rs:154`).
- `audio/pipeline.rs`: a mic-only branch (`pipewiresrc → audioconvert →
  audioresample → mono → encode`, no `audiomixer`). Keep the existing
  downmix-before-mixer regression test; add a mic-only pipeline test.
- `recorder.rs` + daemon: thread an `Option` monitor through `RecordOptions`.

The wizard then offers a **"dictation (mic only)"** preset versus a
**"meeting (mic + system)"** preset. This also gives the daemon a clean
dictation path that today only `zwhisper-dictate` (out-of-process `pw-record`)
provides.

## External Tools & Security

| Tool | Use | Notes (verified 2026-06-03; confirm on hardware) |
|---|---|---|
| `pw-dump` | enumerate `{id, node.name, node.description, media.class}` (parsed with `serde_json`) | default flag not present in dump → cross-reference `wpctl inspect` |
| `pw-cat --record --raw --format=f32 --channels=1 -` | raw headerless LE f32 PCM on stdout for metering | `--target` takes a **numeric id** (not node.name) → map from `pw-dump`; `--latency` lowers buffer for responsive metering |
| `wpctl get-volume <id>` | read level | stdout `Volume: 0.45` (+ optional ` [MUTED]`) |
| `wpctl set-volume <id> <v>` | set level | linear `0.0–1.0` (0.5 ≈ −6 dB); also accepts `45%`, `5%+` |
| `wpctl set-default <id>` | make default source | global; gated behind explicit `--set-default` / wizard confirm |

Security invariants (priority #1):

- Exclusively `Command::new().arg(...)` — never a shell string → no command
  injection. `id` validated as purely numeric; `node.name` via the existing
  `[A-Za-z0-9._:-]+` allow-list before it ever reaches `pipewiresrc` or a TOML
  write.
- `pw-dump` output read with a size cap (reject absurdly large dumps); parse is
  tolerant of unknown fields (serde ignores), strict on the fields we use.
- `pw-cat` child: hard timeout, killed on completion/early-exit, stdout read
  bounded (no unbounded buffering); EOF/spawn errors are typed, not panics.
- Volume: always clamped to `[0.0, max_volume]` and `is_finite()`-checked before
  `set-volume`; never NaN/inf/negative; never above the configured cap.
- `set-default` mutates global state → only with explicit `--set-default` in
  non-interactive use, and with confirmation in the wizard.
- Profile writes: atomic (temp + `rename` + `sync_all`), `DocumentMut` to
  preserve comments, re-validated after write; only user-override profiles are
  mutated.

## Migration Strategy (phased; each phase independently valuable)

| Phase | Deliverable | Risk | Hardware? |
|---|---|---|---|
| **0** | core `setup` module: types, `pw-dump` parsing, volume parsing, dB computation, calibration algorithm, `PipewireControl` trait + `MockPipewire`; full unit tests | low | no (mock) |
| **1** | CLI `audio devices` + `audio meter` (read-only) | low | verify |
| **2** | CLI `audio calibrate` (measure + `--apply` volume + `--set-default`); dry-run default | medium | verify |
| **3** | schema `input_gain_db` + `toml_edit` profile writer + daemon pipeline `volume` element | medium | verify |
| **4** | `audio setup` interactive wizard (composes 1–3) | low | yes (UX) |
| **5** | mic-only profiles (schema + devices + pipeline + daemon) + wizard preset | **higher** (pipeline rewrite) | yes |

Phases 0–2 ship usable value (enumeration + auto-calibration with
PipeWire-native volume) without touching the daemon. Phase 5 (mic-only) is the
riskiest (capture-pipeline change) and lands last.

## Testing Strategy

### Unit tests (no hardware, `MockPipewire`)

- `pw-dump` JSON fixtures: real mic, sink, `.monitor` source, missing fields,
  empty dump. Assert `AudioDevice` mapping and `is_monitor`/`is_source`.
- Volume parsing: `Volume: 0.45`, `Volume: 0.45 [MUTED]`, malformed/empty.
- dB computation: silence (no NaN), full-scale (≈0 dBFS), clipping, known
  sine/constant vectors with hand-computed peak/RMS.
- Calibration: convergence within tolerance, clamp at cap, too-quiet mic
  terminates without looping, non-linear hardware modeled by a mock that
  under-responds.
- Node id/name validation (numeric id; allow-list name).

### CLI tests

- clap parser truth-table for the new subcommands (mirroring the style in
  `crates/zwhisper-cli/src/cli.rs` `mod tests`).
- Dispatcher exit codes; `--json` output shape; dry-run vs `--apply`.

### Daemon tests (phases 3 & 5)

- `pipeline_description` includes the `volume` element when `input_gain_db` is
  set; the linear factor matches the dB conversion.
- Mic-only pipeline string (no `audiomixer`); existing mono-mix regression test
  stays green.
- Profile round-trip with `input_gain_db` present/absent; comment preservation
  through the new writer.

### Hardware verification (developer runtime box; not possible in sandbox)

Real `pw-cat`/`wpctl`/`pw-dump`; ALC1220 calibration converges into the target
window; mic-only daemon recording transcribes correctly; `set-default` makes
dictation pick the calibrated mic.

## Risks

- `pw-cat --target` likely accepts only a numeric id while `zwhisper-dictate`
  uses a node.name — mitigated by always resolving an id from `pw-dump`. Verify
  on hardware.
- Linear-volume → dB mapping is approximate across hardware gain stages
  (ALC1220), so calibration iterates rather than computing once.
- `set-default` is global (affects all apps) — gated behind an explicit flag /
  wizard confirmation.
- WirePlumber version differences: `@DEFAULT_AUDIO_SOURCE@` vs
  `@DEFAULT_SOURCE@`; the `wpctl` volume output format. Detect/verify at runtime.
- Phase 5 rewrites part of the capture pipeline; the FLAC and ASR branches must
  stay independent (per RFC-audio-source-model), and the mono-mix regression
  test must not break.
- Mic-only schema relaxation must not silently re-enable the old
  empty-`system_output` → "default" coercion that the M2 review flagged as a
  high-severity surprise.

## Open Decisions

- **Interactive prompt mechanism.** Hand-rolled stdin + `\r`-refreshed ASCII VU
  meter (no new dependency) vs adding `dialoguer` for arrow-key selection.
  Default: hand-rolled, to keep the dependency graph lean.
- **Command name.** `zwhisper audio` (extensible) with a `mic` alias, vs a
  top-level `zwhisper mic`. Default: `audio`.
- **`input_gain_db` vs linear in the profile.** dB is human-readable; the
  pipeline converts to a linear `volume` factor. Default: dB in the profile.
- **Schema version bump.** Additive optional field may not need one; decide when
  Phase 3 lands whether to bump for explicitness.
- **Dictation env vs profile.** Should the wizard also write
  `ZWHISPER_DICTATE_SOURCE` for the helper, or rely solely on `set-default`?
  Default: rely on `set-default` (zero extra config), mention the env override.

## Recommended Direction

Build the `setup` module in `zwhisper-core` first (Phase 0) so the analysis and
calibration logic is testable behind `MockPipewire` with no hardware. Layer the
read-only CLI commands (Phase 1), then calibration with PipeWire-native volume
(Phase 2) — at which point the feature already solves the original pain
(picking the right mic and setting a safe level globally). Then persist into
profiles with a comment-preserving writer and the daemon `volume` element
(Phase 3), wrap it in the wizard (Phase 4), and finally add the mic-only capture
path (Phase 5). This keeps the CLI a thin client, puts all testable logic in
core, applies the gain both globally (PipeWire) and reproducibly (profile), and
defers the only high-risk change (pipeline rewrite) to the end.
