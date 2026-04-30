# M0 — Definition-of-done verification (in progress)

> Sign-off artefact for the M0 walking skeleton. This file is closed
> only when **all five DoD bullets from IDEA.md § 11** have evidence.

## DoD checklist

| # | DoD bullet | Evidence | State |
|---|---|---|---|
| 1 | `zwhisper record --mic default --monitor default --output x.flac --duration 60` produces a valid FLAC. | 60-min soak (below) wrote `/tmp/zwhisper-soak-20260430-150903.flac`, accepted by `flac -t`. | **READY** |
| 2 | 60-min continuous recording, RSS slope ≈ 0. | `logs/m0-soak-20260430-150903.csv` — slope **0.0147 KiB/s** (warmup_skipped=300s, threshold ±4 KiB/s). | **READY** |
| 3 | After stop, FLAC valid: header parses, declared length matches wall-clock duration ± one buffer. | soak FLAC samples = **57 600 008** vs expected **57 600 000** → drift **8 samples**, well under one mono buffer (1024 samples). `flac -t` accepts. | **READY** |
| 4 | No dropped samples — `pipewiresrc` underrun / `audiomixer` discont events surfaced as warnings, not swallowed. | `watchdog::classify` (case-insensitive `UNDERRUN_NEEDLES`); `recorder.rs` increments `underruns` and stores warning strings (capped at 100) — both fields land in `RecordingReport`. Soak run reported `underruns=0`, `warnings=0`. | **READY** |
| 5 | Default device hot-swap during recording is detected and reported (graceful stop + non-zero exit), never a silent partial recording. | Watchdog `DeviceLost` paths cover (a) case-insensitive `pipewiresrc` Error w/ device-lost needles, (b) `Element` `node-removed`. Manual unplug test pending. | _pending manual test_ |

## Quick sanity (dev box, after Phase 7 fixes)

| Check | Command | Result |
|---|---|---|
| Build clean | `cargo build --workspace` | OK |
| Unit + CLI tests | `cargo test --workspace` | 25 unit + 5 cli passed |
| `audio-it` integration | `cargo test --workspace --features audio-it` | 26 unit + 6 cli passed (incl. `record_writes_valid_flac`) |
| Clippy strict | `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| Post-fix smoke recording | `zwhisper record --output /tmp/zwhisper-postfix.flac --duration 2` | wall-clock 2.49 s, file 0600, `flac -t ` accepts, samples = 32 043 (= 2 × 16 000 ± 1 buffer ≈ +43 samples), session_id logged, underruns=0, warnings=0 |
| EOS-race fix | same run, log timeline | `draining` → `complete` = 6 ms (was ~109 ms before fix) |

## Phase 7 review team — applied vs. deferred

### Applied in this milestone

1. **Critical (perf)** — unbounded `warnings` vec → cap at `MAX_WARNINGS = 100` (`audio/recorder.rs:33-37`).
2. **High (perf)** — EOS race between bus thread and `wait_for_eos` → `audio/recorder.rs:`stop` now reads `StopReason` via watch channel, joins bus thread, *then* transitions to Null. New helper `wait_for_stop_signal`.
3. **High (silent-failure)** — `tokio::signal::ctrl_c()` Err was swallowed → bubbles up as `RecordingError::PipelineFailed { stage: "install_ctrl_c_handler", … }` (`audio/recorder.rs::race_stop`).
4. **High (security)** — gst-launch DSL injection via node names → strict allow-list `[A-Za-z0-9._:-]+`, max 256 chars, applied to both `--mic`/`--monitor` and wpctl-resolved names (`audio/devices.rs::validate_node_name`).
5. **High (silent-failure / devils-advocate)** — every `pipewiresrc` Warning was tagged as Underrun → require substring match against `UNDERRUN_NEEDLES` (`audio/watchdog.rs`).
6. **Medium (security)** — TOCTOU empty file on pipeline build failure → `audio/pipeline.rs::build` removes the file on inner failure.
7. **Medium (silent-failure)** — `RecordingError::OutputPath` lost the `io::Error` → variant carries `{ path, source }` (`audio/error.rs`).
8. **Low (security)** — soak script numeric/path injection → strict regex validation on `$1`/`$2` before any arithmetic or path usage.

### Phase 7B (post-PE review, second pass)

A second user-supplied review surfaced six more issues; all were
fixed before the soak finished:

9. **Critical** — `record_blocking::race_stop` returned
   `StopRequest::UserRequested` even when the bus watchdog had won
   the race. `Recorder::request_stop` then `send_replace`-d the real
   `StopReason::DeviceLost { … }` / `BusError { … }` with
   `UserRequested`, turning a hardware failure into a clean exit
   code 0. Fix: introduced `RaceOutcome { Caller(StopRequest),
   BusInitiated }`; `record_blocking` only calls `request_stop` for
   the `Caller` variant. (`audio/recorder.rs::race_stop`,
   `record_blocking`).
10. **Critical** — bus thread leaked on a failed `Recorder::start`
    because it was spawned *before* `set_state(Playing)`. If Playing
    failed, the join handle was dropped without ever flipping
    `bus_shutdown`. Fix: spawn the bus thread *after* the Playing
    transition; the bus queues pre-roll messages until iteration
    starts, so no signal is lost. (`audio/recorder.rs::start_inner`).
11. **High** — empty output file lingered after a failed
    `set_state(Playing)`, blocking retries with `EEXIST`. The
    pipeline-build cleanup did not cover later failures. Fix: wrap
    `start_inner` in `Recorder::start` and remove the precreated
    file on every error path. (`audio/recorder.rs::start`).
12. **High** — `init_gstreamer()` ran for every subcommand (`status`,
    `transcribe`), making the CLI fail without GStreamer even when
    those paths do not need it; the old `status` line ("not running")
    was also misleading. Fix: call `init_gstreamer()` only for
    `Record`; rephrase `status` to reflect M0 reality.
    (`main.rs::main`, integration test
    `status_runs_without_daemon`).
13. **High** — `--duration 0` was unbounded, in conflict with the
    spec's `max_duration_minutes` runaway-recording safeguard
    (IDEA.md § 1, lines 33-36). Fix: new `--max-duration-minutes`
    flag (default 240 min); `cli::resolve_duration` enforces the cap
    and emits a clear error when an explicit duration exceeds it.
    Pass `--max-duration-minutes 0` to opt out. Unit-tested in
    `cli::tests`.
14. **Medium** — `cargo clippy --all-targets --all-features -D warnings`
    failed because of a missing backtick in the `audio-it` integration
    test's doc comment, so CI never actually exercised the
    `record_writes_valid_flac` path even with all features enabled.
    Fix: corrected the doc comment; `--all-features` clippy is now
    clean.

### Phase 7C (third-pass review, post-fix delta)

A third user-supplied review caught four more issues missed by the
first two passes; all addressed:

15. **Critical (data loss)** — the failure cleanup added in fix #11
    blindly deleted the user's pre-existing `--output` file. If
    `precreate_output` returned `EEXIST`, `Recorder::start` would
    `remove_file` the user's data on its way out. Fix:
    `pipeline::build` now returns a `(Pipeline, BuiltOutput)`
    tuple; the `BuiltOutput` token tracks ownership and only the
    file *we* created via `OpenOptions::create_new` is removed on
    a later failure (`audio/pipeline.rs::BuiltOutput`,
    `audio/recorder.rs::start`).
16. **High** — `race_stop` could miss a stop reason that the bus
    thread had already written. The watch-loop called
    `borrow_and_update()` and then awaited `.changed()`, so a
    pre-poll non-`Running` value triggered an indefinite wait
    (with `--duration 0`) or got silently overwritten by the
    duration timer (with explicit duration). Fix: invert the loop
    to check the *current* value before awaiting the next change
    (`audio/recorder.rs::race_stop`).
17. **Medium (M3 leak)** — `RecordingError::PipelineFailed.stage`
    was `&'static str`, forcing `Box::leak` for dynamic stage
    labels coming off the bus. Tolerable for a one-shot CLI but a
    permanent allocation per error in M3's long-running daemon.
    Fix: switched the field to `Cow<'static, str>`; static labels
    remain `Cow::Borrowed`, dynamic strings become
    `Cow::Owned(String)` and free with the error
    (`audio/error.rs:18-21`, all `PipelineFailed` constructors).
18. **Medium (CI baseline)** — `cargo clippy --all-targets --all-features
    -- -D warnings` failed on `clippy::items_after_test_module`
    because `run_transcribe` lived after the `mod tests` block in
    `cli.rs`. Fix: moved the test module to the end of the file
    (`crates/zwhisper-cli/src/cli.rs`).

### Phase 7D (fourth-pass review)

A fourth user-supplied review surfaced four more issues; all addressed:

19. **High** — `DEVICE_LOST_NEEDLES` were case-sensitive while the
    Error branch did not lowercase the combined message, so a
    capitalisation drift in `gst-plugin-pipewire` would route
    hot-swaps to generic `BusError` instead of `DeviceDisappeared`.
    Fix: lowercase the needles, lowercase the combined payload+debug
    in `watchdog::classify`'s Error branch (matching the existing
    Warning-branch convention). Added a unit test
    (`device_lost_needle_match_is_case_insensitive`).
20. **Medium** — explicit `--mic` / `--monitor` were only validated
    syntactically; the M0-plan promise to "validate it appears in
    `wpctl status`, missing device → typed error" was not honoured,
    leaving diagnostics until pipeline preroll. Fix: extended
    `WpctlRunner` with `list_node_names` (backed by `pw-cli ls Node`),
    added `ensure_node_exists` / `ensure_one_of_exists` to
    `audio::devices::resolve`. Explicit values land in a typed
    `DeviceError::InvalidArgument` with a sample of available
    candidates.
21. **Medium** — `RecordingReport.samples_written` was missing despite
    M0-plan locking it in. Fix: added the field, populated by a
    24-line FLAC `STREAMINFO` parser (`read_flac_total_samples`,
    RFC 9639 § 8.2) reading the closed file after the Null
    transition. Verified: 2 s recording reports 32 037 samples ↔
    `metaflac --show-total-samples` 32 037; 5 s reports 80 513 ↔
    80 513. Two unit tests cover the parser.
22. **Medium** — `record_blocking` propagated `race_stop` errors with
    `?`, bypassing the canonical EOS finalisation and letting `Drop`
    tear the pipeline down via `set_state(Null)` on a still-PLAYING
    pipeline (would truncate the FLAC header on the rare path of a
    failed `tokio::signal::ctrl_c` install). Fix: explicit `match`;
    on race error we still call `recorder.request_stop` +
    `recorder.stop()` to drain cleanly before surfacing the original
    error.

## Test counts

After Phase 7D:

- `cargo test --workspace`: 33 unit tests + 5 cli tests
- `cargo test --workspace --features audio-it`: +1 cli (`record_writes_valid_flac`) → 39 total
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: clean

### Deferred with rationale

- **Default-device user-switch via sound settings** (devils-advocate #1). `pipewiresrc target-object=<name>` is bound at start; a user-driven default reassignment in the desktop sound panel does not stop the recording. **Interpretation**: M0 DoD #5 (`hot-swap default device, degradace musí být detekovaná, ne tichá`) refers to device-disappear events (USB unplug, profile switch, session-manager teardown — all of which we *do* detect via `DeviceLost`). User-intent default change is not "degradace" — pursuing it cleanly requires a `wpctl get-default` poller or PipeWire metadata watcher and is an M3 feature (it lines up naturally with the daemon's session-aware lifecycle).
- **Second Ctrl+C during EOS drain** (devils-advocate #2). Window is the 5 s `EOS_TIMEOUT_SECS` drain. Holding the second SIGINT is a deliberate user choice; the M0 surface area does not reinstall a custom handler over `tokio::signal::ctrl_c`. Re-arming during drain is an M3 daemon concern.
- **Peak-to-peak RSS bound** (devils-advocate #4). Soak script computes least-squares slope, which is the operationalisation of the IDEA.md wording ("slope ≈ 0"). Adding `max(RSS) - min(RSS)` is post-M0 observability.
- **Symlink resolution on `--output`** (devils-advocate #5). M0 is a single-user CLI; a deliberate symlink in the user's own filesystem is the user's choice. Defer hardening to when zwhisper runs under different uid contexts (M3 systemd unit).

## 60-minute soak

Run completed 2026-04-30 16:09 UTC+2. Driver: `scripts/m0-soak.sh 3600 logs`.

- CSV: `logs/m0-soak-20260430-150903.csv` (61 sample rows, RSS ranged 24196–24344 KiB after warmup)
- Log: `logs/m0-soak-20260430-150903.log`
- FLAC: `/tmp/zwhisper-soak-20260430-150903.flac` (101 MiB, 0600 perms)
- `flac -t` output: `zwhisper-soak-20260430-150903.flac: ok`
- `metaflac --show-total-samples`: **57 600 008** (expected 57 600 000; drift = 8 samples ≈ 0.5 ms ≈ ⅛ of one default mono buffer)
- RSS slope (least-squares, 5-min warmup skipped): **0.0147 KiB/s** vs threshold ±4 KiB/s → soak: PASS
- Driver script verdict: `soak: PASS`

## Hot-swap detection (DoD #5)

### Code-side coverage

The `DeviceLost` translation is exercised by these unit tests in
`crates/zwhisper-cli/src/audio/watchdog.rs`:

- `element_message_with_node_removed_classifies_as_device_lost` —
  synthetic `Element` message with `structure name = "node-removed"`
  is classified as `Stop(StopReason::DeviceLost { node })`.
- `device_lost_needle_match_is_case_insensitive` — confirms the
  needle list (`target not found`, `stream error`, `connection lost`,
  `stream disconnected`) matches against a lowercased payload, so
  capitalisation drift in `gst-plugin-pipewire` does not regress the
  detection path.
- `error_classifies_as_stop_bus_error` — non-pipewiresrc Errors fall
  through to a generic `BusError`, ensuring we don't accidentally
  promote unrelated errors to "device lost".

In `crates/zwhisper-cli/src/audio/recorder.rs`, the `Recorder::stop`
path translates `StopReason::DeviceLost { node }` into
`RecordingError::DeviceDisappeared { node }`. `cli::run_record` maps
it through `color_eyre::Result`, which exits the process with
non-zero status (verified by code review of `cli.rs::run_record` →
`main::main` return type).

### Software simulation attempts (informational)

Two non-destructive attempts to provoke the runtime path:

1. **`pw-cli destroy <id>`** — destroyed an unrelated session-manager
   client. The PHL mic source was unaffected on the recorder's side
   (its proxy was still bound) and zwhisper kept streaming. This
   approach also briefly perturbed the maintainer's PipeWire session
   and required a `systemctl --user restart pipewire wireplumber`.
   **Not suitable for automated CI.**
2. **`pactl suspend-source <name> 1`** — PipeWire suspends the
   driver thread but does not remove the node, so `pipewiresrc`
   sees no event. The recorder kept running until SIGINT. Suspend
   semantics differ from removal; this cannot stand in for an
   unplug.

### Maintainer manual test (canonical procedure)

The DoD bullet is closed by the maintainer running this procedure
against a USB microphone:

```sh
# Start the recorder against the USB mic explicitly.
./target/release/zwhisper record \
    --mic alsa_input.usb-Generic_PHL_34B1U5601-00.analog-stereo \
    --monitor default \
    --output /tmp/hotswap.flac \
    --duration 0

# In another terminal: physically unplug the USB device.
# Expected: zwhisper exits within ~2 s with non-zero status,
# logs `recording failed: device disappeared during recording: <node>`,
# and `/tmp/hotswap.flac` is accepted by `flac -t`.

flac -t /tmp/hotswap.flac
echo "exit=$?"
```

- _Pending physical unplug — to be filled in by maintainer._

## Product-engineer verdict (Phase 7)

**Verdict: READY (pending soak).**

The quality gate confirms all eight Phase-7 fixes landed at the
expected `file:line` positions:

- `MAX_WARNINGS = 100` cap → `audio/recorder.rs:34`
- EOS race fix + `wait_for_stop_signal` → `audio/recorder.rs:200, 206, 343`
- Ctrl+C Err bubbles up → `audio/recorder.rs:441`
- Node-name allow-list → `audio/devices.rs:149, 159, 168`
- `UNDERRUN_NEEDLES` substring guard → `audio/watchdog.rs:40, 87`
- Empty-file cleanup → `audio/pipeline.rs:16, 31-32`
- `OutputPath { path, source }` → `audio/error.rs:30`, `audio/pipeline.rs:45, 85, 96`
- Soak script arg validation → `scripts/m0-soak.sh:20-32`

Exit-code propagation for `DeviceLost` confirmed by code path
(`cli.rs::run_record` → `color_eyre::Result` → non-zero exit).

All four deferred items accepted with the rationale recorded above.

### Insist-list before final sign-off

1. **Soak completes cleanly** — CSV slope under threshold, `flac -t`
   accepts the 60-min output, total samples within ±1 buffer of
   57 600 000.
2. **Manual hot-swap test** — unplug USB mic (or `pactl unload-module`)
   during live recording; confirm non-zero exit within ~2 s and
   `flac -t` accepts the partial file.
3. **Sign off this document** — fill the soak and hot-swap sections,
   add date + initials.

## Sign-off

_Add the date and your initials here when all three insist-list items
above have evidence. M0 is closed only after that signature lands._
