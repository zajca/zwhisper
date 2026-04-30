# M0 — Walking skeleton: implementation plan

> Target milestone from [IDEA.md § 11](../IDEA.md). This plan turns the
> current CLI scaffolding into a working single-process recorder that
> captures mic + sink monitor and writes a valid FLAC file.

## Status snapshot (2026-04-30, post-Phase 7D)

| Area | State | Evidence |
|---|---|---|
| Cargo workspace + tooling | done | `Cargo.toml`, `rust-toolchain.toml`, CI workflows |
| `zwhisper-cli` skeleton | done | `crates/zwhisper-cli/` parses `record/transcribe/status` |
| `zwhisper record …` actually records | done | commit `4418d52`; `crates/zwhisper-cli/src/audio/{recorder,pipeline,watchdog,devices}.rs`; `cli::run_record` calls `record_blocking` |
| GStreamer / PipeWire deps wired | done | `Cargo.toml` workspace.dependencies (`gstreamer`, `tokio`, `uuid`, `dirs`); `gst::init` gated to the `Record` subcommand in `main.rs` |
| Device discovery | done | `audio/devices.rs` (`wpctl inspect` + `pw-cli ls Node` allow-list, 13 unit tests) |
| FLAC validity / soak verification | done | `scripts/m0-soak.sh`; 3600 s soak slope 0.0147 KiB/s, 57 600 008 samples (`docs/M0-verification.md`); `Recorder::stop` enforces STREAMINFO length 34 + samples-vs-wall-clock ± 16 000 gate |
| Live capture covered by default tests | done | `tests/cli.rs::record_writes_valid_flac` is no longer behind `audio-it`; runs on every `cargo test`, runtime-skips with `[SKIP]` when no `$XDG_RUNTIME_DIR/pipewire-0` socket is present |
| Hot-swap detection (DoD #5) | pending — manual test | unit tests cover `DeviceLost` branches; physical USB unplug not yet exercised by maintainer (`docs/M0-verification.md` § 5) |

**Verdict: M0 walking skeleton is implemented; 4/5 DoD items signed
off, only the manual hot-swap unplug remains.** Detailed evidence
lives in `docs/M0-verification.md`.

## Definition of done (verbatim from IDEA.md)

1. `zwhisper record --mic default --monitor default --output x.flac --duration 60`
   produces a valid FLAC.
2. Pipeline survives **60 minutes** of continuous recording with no
   memory growth (RSS sampled every minute, slope ≈ 0).
3. After stop, FLAC is valid: header parses, declared length matches
   wall-clock duration ± one buffer, no truncation.
4. No dropped samples — `pipewiresrc` underrun / `audiomixer` discont
   events are zero on the happy path; if any occur they are surfaced as
   warnings, not swallowed.
5. Default device hot-swap during recording is **detected and reported**
   (graceful stop + non-zero exit), never a silent partial recording.

Anything not on this list is explicitly out of scope for M0 (no daemon,
no D-Bus, no profiles, no transcription — those are M1+).

## Non-goals for M0

- Daemonization, D-Bus, tray, settings GUI (M3+)
- Profiles, schema versioning (M2)
- Whisper.cpp integration (M1)
- Stereo split capture mode (M2 with profiles)
- Hardened systemd unit (M8)
- Hotkeys (M6)

## Architecture for M0

Single binary `zwhisper`. New module layout inside `zwhisper-cli`:

```
crates/zwhisper-cli/src/
├── main.rs            # entrypoint, tracing init
├── cli.rs             # clap args (already exists)
└── audio/
    ├── mod.rs         # public façade: `Recorder` handle + `record_blocking`
    ├── devices.rs     # default-device discovery (wpctl)
    ├── pipeline.rs    # GStreamer pipeline construction (mono-mix)
    └── watchdog.rs    # underrun / device-removed monitoring
```

Rationale: keep everything in `zwhisper-cli` until M3 splits the daemon
out. Module boundaries match what will later move to `zwhisper-core`,
so the future split is mechanical — but only if we honour the API rules
below.

### Public API rules (M3 lock-ins)

These are non-negotiable for M0 because reversing them later means
rewriting the M3 D-Bus surface (IDEA.md § 2):

1. **Handle pattern, not a one-shot fn.** The façade exposes a
   `Recorder` type:

   ```rust
   pub struct Recorder { /* owns pipeline thread, watchdog, stop tx */ }
   impl Recorder {
       pub fn start(opts: RecordOptions) -> Result<Self, RecordingError>;
       pub fn stop(self) -> Result<RecordingReport, RecordingError>;
       pub fn state(&self) -> RecorderState;
       pub fn session_id(&self) -> SessionId;
   }
   ```

   The CLI wraps it with `pub fn record_blocking(opts, stop: StopSignal)
   -> Result<RecordingReport, RecordingError>` that races
   `tokio::signal::ctrl_c` against `tokio::time::sleep(duration)` and
   then calls `recorder.stop()`. M3 calls `start`/`stop` directly from
   the D-Bus handler.

2. **No `gst::*` or `glib::*` in any `pub` signature.** They live
   `pub(crate)` only.
   - `RecordOptions` is plain Rust (`PathBuf`, `String`, `Duration`).
   - `RecordingReport { samples_written: u64, duration: Duration,
     underruns: u32, warnings: Vec<String>, audio_path: PathBuf }` —
     `audio_path` is needed for the M3 `RecordingComplete(s, s)` signal.
   - `RecordingError` is a `thiserror` enum: `DeviceDisappeared { node:
     String }`, `PipelineFailed { stage: &'static str, source:
     Box<dyn Error + Send + Sync> }`, `EncoderFailed`, `Timeout`,
     `WpctlFailed { stderr: String }`. `glib::Error` and
     `gst::StateChangeError` are wrapped, never re-exported.
   - The watchdog surfaces a domain `BusEvent` enum, not raw
     `MessageView`.

3. **`RecorderState` enum is canonical now.** `Idle | Starting |
   Recording | Stopping | Failed`. The string mapping
   (`"recording"`, …) lives in `Display` on this enum, in one place.
   M3's `GetStatus` D-Bus method returns these strings — if M0 invents
   a different name (e.g. `Running`) we either break the wire format
   in M3 or rewrite M0.

4. **`SessionId(Uuid)` is generated inside `Recorder::start`.** M0
   logs it through `tracing`; M3 returns it from `StartRecording` and
   echoes it in `StateChanged`. Putting this in the caller now means
   M3 plumbs it through retroactively.

### Stop signalling

The façade owns a `tokio::sync::watch::Sender<StopReason>` shared
between the watchdog and the duration/Ctrl+C race:

```rust
enum StopReason { Running, DurationElapsed, UserRequested, DeviceLost { node: String }, BusError { stage: &'static str } }
```

Multiple producers (watchdog, timer, ctrl_c) write to it. Only
`Recorder::stop` consumes the latest value, runs the
`send_event(Eos) → bus wait → set_state(Null)` sequence (the fragile
finalisation path lives in exactly one place), and translates the
reason into `RecordingReport` / `RecordingError`. This is the reason
`shutdown.rs` is **not** a separate module — racing two futures is ten
lines and folding them into the façade keeps the EOS owner singular.

## Phased plan

Each phase lands as a single PR-sized commit set. Phases are executed
sequentially — each one builds on the previous one's verification.

### Phase 0 — Host prerequisites (doc only, ~30 min)

Add a `docs/M0-host-setup.md` snippet listing the Arch packages required
to build & run M0:

- `pipewire`, `pipewire-alsa`, `wireplumber`
- `gstreamer`, `gst-plugins-base`, `gst-plugins-good`, `gst-plugin-pipewire`
- `flac` (for verification tooling: `flac -t`, `metaflac --show-total-samples`)
- Build chain: `pkgconf`, `clang` (for `gstreamer-rs` bindgen)

**Done when**: `docs/M0-host-setup.md` exists and the maintainer's box
boots `gst-launch-1.0 pipewiresrc ! fakesink num-buffers=10` cleanly.

### Phase 1 — Wire GStreamer dependencies (~1 h)

- Add to `workspace.dependencies`:
  - `gstreamer = "0.23"`
  - `tokio = { version = "1", features = ["signal", "rt", "macros", "time", "sync"] }` — needed for Ctrl+C, duration timer, and the `watch` channel that carries `StopReason`; small surface, no full async refactor
  - `uuid = { version = "1", features = ["v4"] }` — for `SessionId`
- Add to `zwhisper-cli/Cargo.toml`: `gstreamer`, `tokio`, `uuid`.
- We do **not** depend on `glib` directly. Bus iteration uses
  `bus.iter_timed` from a dedicated `std::thread`, not `glib::MainLoop`
  (validated against gstreamer-rs 0.23 docs — `MainLoop` is GUI-shaped
  overhead we don't need). `glib` stays a transitive dep through
  `gstreamer`.
- Use `tracing-appender` (already declared) from `main.rs` for a daily
  file appender at `${XDG_STATE_HOME:-~/.local/state}/zwhisper/zwhisper.log`.
  Per IDEA § 7, never log transcript text — N/A here, but enforce no
  payload logging convention from day one.

**Done when**: `cargo build --workspace` and `cargo test --workspace`
still pass; `gst::init()` is called in `main` behind a `OnceLock` so it
runs exactly once.

### Phase 2 — Default-device discovery (~3 h)

`audio/devices.rs` exposes:

```rust
pub struct DeviceSelection {
    pub mic_node:    String,  // resolved PipeWire node name
    pub monitor_node: String,
}

pub fn resolve(mic_arg: &str, monitor_arg: &str) -> Result<DeviceSelection>;
```

Resolution rules:

- If `mic_arg != "default"`: take it verbatim, validate it appears in
  `wpctl status` (or fail with a list of available nodes).
- If `mic_arg == "default"`: shell out to `wpctl inspect @DEFAULT_AUDIO_SOURCE@`
  and parse `node.name`. Same pattern for `@DEFAULT_AUDIO_SINK@` plus
  `.monitor` suffix for the monitor source.
- No `pw-metadata` parsing in M0 — `wpctl` is the simpler, stable path
  and IDEA.md explicitly allows it as the fallback. Promote to primary
  for M0; revisit `pw-metadata` if `wpctl` proves flaky in M3.

**Done when**:
- Unit test with a fake `WpctlRunner` trait covers: default mic, default
  monitor, explicit override, missing device → typed error.
- Manual smoke: `zwhisper record --mic default --monitor default --output /tmp/x.flac --duration 1` prints the resolved node names through `tracing` at `info`.

### Phase 3 — GStreamer mono-mix pipeline (~5 h)

`audio/pipeline.rs` builds the pipeline described in IDEA.md § 3
verbatim, parameterised by the resolved nodes and output path:

```text
pipewiresrc target-object=<mic>     ! audioconvert ! audioresample ! mix.
pipewiresrc target-object=<monitor> ! audioconvert ! audioresample ! mix.
audiomixer name=mix                 ! audioconvert ! audioresample
                                    ! audio/x-raw,rate=16000,channels=1
                                    ! flacenc ! filesink location=...
```

Use `gst::parse::launch` for the first cut. It returns
`Result<gst::Element, glib::Error>`; downcast via
`.downcast::<gst::Pipeline>()`. Upgrade to programmatic element
construction only if we need property access mid-stream.

Lifecycle (owned entirely by `Recorder`):

1. **File pre-create.** `filesink` has no `mode` property that maps to
   POSIX permissions (`file-mode` is `truncate`/`append` only). Open
   the output via `OpenOptions::new().mode(0o600).create_new(true)`,
   close the handle, then point `filesink location=` at the path. This
   guarantees `0600` regardless of umask.
2. Construct pipeline, transition to `Playing`.
3. Spawn a dedicated `std::thread` that loops on
   `bus.iter_timed(Some(100 * gst::ClockTime::MSECOND))`, classifies
   each message into a domain `BusEvent`, and forwards stop-worthy
   events into the `watch::Sender<StopReason>`. The main task awaits
   the `watch::Receiver` along with `tokio::signal::ctrl_c` and
   `tokio::time::sleep(duration)` (the timer is skipped when
   `--duration == 0`).
4. **Stop sequence (the *only* place this runs):**
   `pipeline.send_event(gst::event::Eos::new())` →
   `bus.timed_pop_filtered(Some(5 * gst::ClockTime::SECOND),
   &[gst::MessageType::Eos, gst::MessageType::Error])` →
   `pipeline.set_state(gst::State::Null)`. Joining the bus thread
   afterwards is mandatory — leaks otherwise. **Never just drop the
   pipeline** — that leaves the FLAC header without a sample count and
   `flac -t` will reject the file.
5. Return `RecordingReport { samples_written, duration, underruns,
   warnings, audio_path }` (samples taken from the pipeline's running
   time at EOS).

**Done when**:
- `zwhisper record --output /tmp/x.flac --duration 3` produces a file
  that `flac -t /tmp/x.flac` accepts and `metaflac --show-total-samples`
  reports `~48000` samples (3 s × 16 kHz, ± one buffer).
- Output file permissions are `0600` (verified by `stat -c %a`).
- `cargo clippy --all-targets -- -D warnings` clean.

### Phase 4 — Watchdog: underruns + device removal (~3 h)

`audio/watchdog.rs` is the bus-thread classifier. It does **not** send
EOS itself — it writes `StopReason` into the `watch::Sender` and lets
`Recorder::stop` run the EOS sequence. Single owner of EOS is the
whole point.

Classification (the exact `MessageView` variants for #1 and #3 are
flagged "runtime verify" in the gstreamer-rs report; we lock them in
during this phase via `GST_DEBUG=4` traces against a real
`pipewiresrc`):

- `MessageView::Warning` whose source is `pipewiresrc` → underrun
  counter increment, log at `warn` with the source's element name.
- `MessageView::Error` from any element → `StopReason::BusError {
  stage: <element name> }`, log at `error` with the gst error string
  preserved.
- Device removal: candidate signals are (a) `MessageView::Element` with
  a PipeWire `node-removed` structure, (b) `MessageView::Error` from a
  `pipewiresrc`, (c) `StateChanged` to `Null` on an element we own
  while we are still in the `Recording` state. All three are wired and
  whichever fires first writes `StopReason::DeviceLost { node }`.
  Recorder::stop translates this into `RecordingError::DeviceDisappeared`
  on exit, surfaced as a non-zero CLI status.

**Done when**:
- Manual test: start recording, unplug the USB mic (or
  `pactl unload-module module-…`) → process exits within ~2 s with
  non-zero status and a clear message naming the lost device; the
  partial FLAC up to the cut is still valid.
- Unit test for the bus-message classifier with synthetic
  `MessageRef`s constructed via `gst::message::*` builders. Tests do
  not require a running PipeWire daemon.

### Phase 5 — Wire it into `cli::run_record` + cleanup `bail!` (~1 h)

Replace the `bail!` in `crates/zwhisper-cli/src/cli.rs:52` with a call
into `audio::record_blocking` (the convenience wrapper that owns a
`Recorder` and races `ctrl_c`/`sleep` internally). Map errors via
`color_eyre`. Keep the test `record_is_not_implemented_yet` — but flip
it: rename to `record_writes_valid_flac` and exercise a 1-second
capture against the real PipeWire daemon, gated behind a
`cfg(feature = "audio-it")` so CI without audio hardware can skip it
(CI runners have PipeWire-less containers; integration tests only run
on the maintainer's box and on opt-in self-hosted runners).

**Done when**:
- `cargo test --workspace` passes (without the audio-it feature).
- `cargo test --workspace --features audio-it` passes locally.
- The old "not implemented" test message no longer appears anywhere.

### Phase 6 — 60-minute soak verification (~1 h human + 60 min wall)

Add `scripts/m0-soak.sh`:

- Starts `zwhisper record --output /tmp/zwhisper-soak.flac --duration 3600`.
- Concurrently samples RSS via `ps -o rss= -p $PID` every 60 s into
  `logs/m0-soak-<timestamp>.csv`.
- After completion: `flac -t`, `metaflac --show-total-samples` (expect
  ≈ `3600 × 16000`), and a small awk one-liner over the CSV asserting
  the linear-regression slope of RSS over time is < 1 KiB/s.

**Done when**:
- A successful run is recorded in `docs/M0-verification.md` with the
  CSV path, regression slope, and `flac -t` output. This file is the
  M0 sign-off artefact.

### Phase 7 — Cross-check DoD checklist (~30 min)

Walk DoD items 1–5 against evidence and tick them in
`docs/M0-verification.md`. If any item lacks evidence, loop back to
the relevant phase rather than declaring done.

## Risks (what could push us back)

- **`pipewiresrc` device-removed semantics** vary between PipeWire
  versions. If the bus doesn't emit a clear signal on hot-swap, we
  fall back to a periodic `wpctl status` poll inside the watchdog —
  worse but acceptable for M0.
- **`flacenc` with mono 16 kHz** at very small buffers can produce
  oddly-sized frames; if `flac -t` rejects the file we add an explicit
  `audiobuffersplit` element. Locked in during Phase 3 verification.
- **CI cannot run audio integration tests.** Mitigated by the
  `audio-it` feature flag; soak test is a local artefact, not CI.
- **`tokio` adoption creep.** Limited to `signal::ctrl_c`,
  `time::sleep`, and `sync::watch` for M0; don't async-ify the
  pipeline. Push back in review if creep shows up.
- **Smoke-test gotcha (verified 2026-04-30).** `gst-launch-1.0
  pipewiresrc num-buffers=10 ! fakesink` fails with `target not
  found` on a healthy host because `pipewiresrc` cannot negotiate caps
  without a converter. Always smoke-test with `… ! audioconvert !
  fakesink`. This matches our real pipeline shape.
- **Unverified gstreamer-rs API surfaces (Phase 3 / 4 lock in):**
  - `pipewiresrc` underrun bus message (`Warning` vs `Element`) —
    needs runtime verification with `GST_DEBUG=4`.
  - Hot-swap detection variant (`Element` with `node-removed` vs
    `Error` vs `StateChanged → Null`) — same.
  - `flacenc` minimum input buffer size — observed during Phase 3
    integration test; if `flac -t` rejects, add `audiobuffersplit`.

## Out of scope, on purpose (re-statement)

These will be tempting during M0 but must not land here, per
IDEA.md § 13:

- D-Bus interface, even a stub
- Tray indicator, even hidden
- Profile loader, even with one hardcoded profile
- Whisper.cpp invocation, even behind a `--transcribe` flag
- Stereo-split mode (deferred until profiles in M2)

## Definition-of-done sign-off

M0 is closed only when `docs/M0-verification.md` is committed with all
five DoD items ticked and links to artefacts (CSV, FLAC test output,
clippy/test logs). Until then, M0 stays open.
