# RFC: Actionable Error and Empty States

## Status

Proposed. Tracks [#28](https://github.com/zajca/zwhisper/issues/28), part of the
[#24](https://github.com/zajca/zwhisper/issues/24) UI-direction epic (M11).

Scope decisions taken before drafting (2026-09-24):

- **Muted microphone refuses the start.** A definitive mute blocks
  `Recorder1.StartRecording` with a coded reason. An *inconclusive* probe never
  blocks.
- **A missing model or uncompiled backend does NOT refuse the start.** It is
  reported before capture so the user can act immediately, and the recording
  proceeds — see § F6. This narrows the original scope decision after
  implementation showed refusing would discard irreplaceable audio to prevent a
  recoverable transcription failure.
- **An empty transcript with a level diagnosis fails the job.** A non-empty
  transcript is never downgraded to a failure.
- The "model loading" row of the issue table is a *state*, not a failure reason,
  and belongs to [#32](https://github.com/zajca/zwhisper/issues/32)
  (daemon-side transcribing state). This RFC defines the failure vocabulary and
  leaves that row to #32.

## Summary

Every failure zwhisper can produce today collapses to one of two things: the
bare string `"failed"` on `Recorder1.StateChanged`, or a free-form
`TranscribeError` Display string on `Jobs1.JobFailed`. Neither carries a
machine-readable code, and neither carries a suggested action. Three of the five
sites that emit `"failed"` persist nothing at all.

The product's own README admits the gap:

> Empty result? Your mic gain is almost certainly too high; see Microphone level.

The diagnosis is known well enough to be written down. It belongs in the failure
message, at the moment it happens.

This RFC introduces a structured **failure reason** — a stable code, a human
message, and a concrete action — produced at every failure site, carried over a
new `Diagnostics1` D-Bus interface alongside the frozen `Recorder1` surface, and
surfaced in the CLI exit message, the desktop notification, and the status
tooltip. It adds the two detections the product cannot make today: a muted
microphone (before capture) and clipping/silence (from levels measured during
capture, at no extra analysis pass).

## Goals

- Every failure carries a stable, machine-readable code, a human message, and an
  action the user can run or a setting they can change.
- The muted-microphone, clipping, and silence cases — the failures that look
  like success — are named at the moment they happen.
- A missing model prints the exact `zwhisper model install` command.
- The reason reaches the user in three places: the CLI exit message, the desktop
  notification, and the Waybar status tooltip.
- `zwhisper status` becomes useful after a failure: today it can never show
  `failed` at all.
- Thresholds are configurable per profile and live in one config struct, not
  inline.
- A real transcript is never turned into an error.

## Non-goals

- No change to the frozen `Recorder1` / `Profiles1` method or signal
  signatures. The existing `"failed"` state string stays exactly as it is.
- No new analysis pass over the recorded FLAC. Levels are measured during
  capture or not at all.
- No voice-activity detection, no speech/noise classification. RMS and peak over
  the whole recording are the entire signal.
- No retry logic. A diagnosis explains; it does not act.
- No "model loading" progress state (#32).
- No mid-recording level warnings or live OSD (#29).

## Current architecture (what exists today)

### The five `"failed"` emit sites

| # | site | persists a reason? | reaches the user? |
|---|---|---|---|
| 1 | `recorder_service.rs:229` — `Recorder::start` failed | no (typed `RpcError::RecordingFailed` return only) | only as an RPC error to the caller |
| 2 | `lifecycle.rs:161` — lifecycle task panicked | no | no |
| 3 | `lifecycle.rs:288` — auto-transcribe job failed | via the job (history + `JobFailed`) | notification, from `JobFailed` |
| 4 | `lifecycle.rs:297` — job `done` channel dropped | no | no |
| 5 | `lifecycle.rs:336` — `await_completion` failed | history `last_error` only | no notification |

`emit_terminal_state` (`lifecycle.rs:389`) funnels 2–5 and asserts the state is
`"idle" | "failed"`.

### What the wire carries

- `Status` is frozen at `(sst)` — `state`, `active_profile`, `duration_ms`
  (`zwhisper-ipc/src/types.rs:24`). No error detail.
- `StateChanged(s new_state, s session_id)` — no error payload
  (`recorder_service.rs:387`).
- `Recorder1.GetStatus` can only ever return `"idle"` or `"recording"`
  (`recorder_service.rs:369-385`). **`"failed"` reaches a client exclusively via
  the signal**, so the one-shot `zwhisper status` can never render it.
- `Jobs1.JobFailed(s job_id, s error)` (`jobs_service.rs:217`) is the only
  signal in the system carrying an error string. Recording-side failures never
  produce one.
- `HistorySession` is `(stssssssss)` with a flattened `last_error: String`.
  `History1` is explicitly *not* frozen the way `Recorder1` is — its freeze test
  exists to make changes deliberate, not to forbid them.

### What already exists and is reusable

- `zwhisper_core::setup::level::analyze(&[f32]) -> LevelStats { peak_db, rms_db,
  frames }` — peak/RMS in dBFS with a finite `SILENCE_FLOOR_DB = -120.0`
  sentinel instead of `-inf`, 16 unit tests.
- `zwhisper_core::setup::volume::Volume { linear, muted }` — `wpctl get-volume`
  output including the `[MUTED]` flag, already parsed, already surfaced by
  `zwhisper audio devices --json`, and **branched on by nothing**.
- `zwhisper_core::setup::PipewireControl` — a mockable trait over
  `pw-dump` / `wpctl`, so the mute probe is unit-testable without PipeWire.
- `StopReason::DeviceLost { node }` (`audio/state.rs:71`), produced by
  `watchdog::classify` from either a `pipewiresrc` bus error or a `node-removed`
  element message, mapped to `RecordingError::DeviceDisappeared { node }`.
- `TranscribeError` (`transcribe/error.rs`) already has precise variants for
  every transcription failure, and `ModelBundleIncomplete` already names
  `zwhisper model install {id}`.
- `Backend::is_compiled_in()` / `required_feature()` (`profile/schema.rs:88`) —
  the v0.6.0 backend-availability source of truth, used only by the CLI.

### What is missing

1. Nothing computes peak/RMS over a *recorded* session. `RecordingReport.pcm` is
   `Some` only for Parakeet auto-transcribe profiles, and no consumer analyses
   it.
2. No preflight of any kind at `StartRecording`: not the mute flag, not the
   backend, not the model. A Parakeet profile on a build without the feature
   records happily and fails after the audio is captured.
3. No structured reason anywhere: no code, no action, no persistence for three
   of the five failure sites.

## Proposed architecture

### Module layout

```text
crates/zwhisper-core/src/diagnostics/
├── mod.rs          # FailureReason, FailureCode, From<&RecordingError/&TranscribeError>
├── config.rs       # DiagnosticsConfig + every threshold constant
└── levels.rs       # LevelSummary + the clipping/silence verdicts
```

`diagnostics` is **not** feature-gated (it compiles with
`default-features = false`); the `From` impls that reference `RecordingError` /
`TranscribeError` are gated on `audio` / `transcribe` respectively. That keeps
the code vocabulary available to any consumer, including one that links neither
GStreamer nor reqwest.

### F1 — `FailureCode`: the stable vocabulary

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureCode {
    MicMuted,
    MicClipping,
    MicSilent,
    DeviceLost,
    EmptyTranscript,
    ModelMissing,
    BackendNotCompiled,
    BackendUnsupported,
    CloudAuth,
    CloudKeyMissing,
    CloudQuota,
    CloudNetwork,
    RecordingFailed,
    TranscribeFailed,
    JobCancelled,
    Interrupted,
}
```

`as_str` produces the wire code (`"mic_muted"`, `"mic_clipping"`, …) and
`from_str` is its exact inverse. Both directions are covered by a round-trip
unit test over an exhaustive `ALL: &[FailureCode]` slice, so a variant added
without a wire string fails the build's test run rather than silently
serialising as something else.

The code is the **stable** part of the contract. The message and action are
human text and may be reworded freely.

### F2 — `FailureReason`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureReason {
    pub code: FailureCode,
    /// What went wrong, in one sentence, naming the concrete device / model /
    /// backend involved.
    pub message: String,
    /// A command the user can run or a setting they can change. Never
    /// "check your microphone".
    pub action: String,
}
```

The `action` field carries a hard rule, enforced by a unit test over every
constructor: it is non-empty, and it either contains a backtick-quoted command
or names a concrete file path or setting key.

### F3 — The vocabulary, in full

| Code | Detected by | Message names | Action |
|---|---|---|---|
| `mic_muted` | `StartRecording` preflight — `wpctl get-volume <id>` reports `[MUTED]` | device description + `node.name` | `wpctl set-mute <id> 0` |
| `mic_clipping` | stop time — peak ≥ `clip_peak_db` in ≥ `clip_window_ratio` of windows | clipped-window percentage + max peak dBFS | `zwhisper audio calibrate --apply` |
| `mic_silent` | stop time — aggregate RMS < `silent_rms_db` | the resolved mic node + measured RMS + duration | `zwhisper audio meter --source <node>` |
| `device_lost` | `StopReason::DeviceLost` | the node that disappeared + the kept partial FLAC path | `zwhisper audio devices` |
| `empty_transcript` | empty/whitespace transcript with *healthy* levels | backend + model + language | check `transcription.language`, try a larger model |
| `model_missing` | `ModelNotFound` / `ModelBundleIncomplete` | model id + expected path | `zwhisper model install <id>` |
| `backend_not_compiled` | `BackendNotCompiled` | backend + missing feature | the full `cargo build --features …` line |
| `backend_unsupported` | `BackendUnsupported` | backend | `zwhisper backend list` |
| `cloud_auth` | `BackendAuth` (401/403) | backend + status + **where the key came from** | rotate the key; `zwhisper backend health --backend <b>` |
| `cloud_key_missing` | `BackendKeyMissing` | env var + secrets path (from `SecretsError`) | set `<ENV>` or create the file with mode 0600 |
| `cloud_quota` | `BackendQuota` (402/429) | backend + retry-after | wait / top up |
| `cloud_network` | `BackendNetwork` / `BackendTimeout` | backend + timeout | check connectivity; `zwhisper backend health` |
| `recording_failed` | any other `RecordingError` | the Display string | `journalctl --user -u zwhisperd` |
| `transcribe_failed` | any other `TranscribeError` | the Display string | `journalctl --user -u zwhisperd` |
| `job_cancelled` | `Jobs1.Cancel` | — | `zwhisper retry <session>` |
| `interrupted` | startup recovery of a `transcribing` entry | — | `zwhisper retry <session>` |

`cloud_auth` naming the key source is a genuine gap today:
`secrets::ResolveSource` is `Env(String)` (the variable name) or
`File(PathBuf)`, it is returned with every successful resolution, and
`deepgram.rs:404` throws it away after one debug log. `TranscribeError::BackendAuth`
gains a `key_source: String` field carrying the rendered source (the env var
*name* or the file *path* — never the value).

### F4 — Level measurement during capture

A `level` element (gst-plugins-good, already in `packaging/arch/PKGBUILD`
`depends`) is inserted into the capture pipeline immediately after the mono
caps filter and before the `tee` / `flacenc`, so it measures exactly the signal
that lands in the artifact, after the optional `input_gain_db` trim.

```text
… ! audio/x-raw,format=S16LE,rate={native},channels=1
  ! level name=zw_level interval={interval_ns} post-messages=true
  ! flacenc ! filesink …
```

The element posts one element message per interval, structure name `level`,
with `peak` / `rms` / `decay` as `GValueArray` of `f64` **already in dBFS**
(verified locally against GStreamer 1.28.7: a 0.5-amplitude sine reports
`peak = -6.02`, `rms = -9.03`). `glib::ValueArray` is available through
`gstreamer` 0.25's `glib` 0.22, so the payload is readable from Rust without
FFI.

`watchdog::classify` gains one arm:

```rust
Classification::Level { peak_db: f32, rms_db: f32, duration_ns: u64 }
```

The recorder's existing bus thread folds each message into a constant-memory
accumulator — no PCM is retained and memory does not grow with recording
length:

```rust
pub struct LevelSummary {
    pub max_peak_db: f32,
    /// Duration-weighted mean square, kept linear so windows of unequal
    /// length combine correctly; converted to dB once, at read time.
    pub mean_square: f64,
    pub total_duration_ms: u64,
    pub windows: u32,
    pub clipped_windows: u32,
}
```

`RecordingReport` gains `levels: Option<LevelSummary>` — `None` when no `level`
message arrived at all (a pipeline that failed before `Playing`, or a recording
shorter than one interval).

This is deliberately independent of `capture_pcm`: level measurement must work
for every profile, not only the Parakeet auto-transcribe ones that happen to
buffer PCM.

### F5 — The verdicts

```rust
pub fn diagnose_levels(
    summary: &LevelSummary,
    mic_node: &str,
    cfg: &DiagnosticsConfig,
) -> Option<FailureReason>
```

Returns `None` unless a verdict is confident:

- Recordings shorter than `min_analysis_ms` produce `None` — too few windows to
  be worth a claim.
- `mic_silent` when `rms_db < silent_rms_db`.
- `mic_clipping` when `clipped_windows / windows >= clip_window_ratio`. A single
  transient never flags.
- Silence wins over clipping when both somehow hold — silence is the stronger,
  less ambiguous statement.

**The verdict is never a gate.** It is consulted only when the transcript is
empty or whitespace-only. A non-empty transcript is delivered exactly as it is
today, and the verdict is logged at `warn` and persisted in history for later
inspection, nothing more.

### F6 — Preflight at `StartRecording`

Three checks, in this order, before any GStreamer work. They produce two
different kinds of outcome — a **refusal** and an **advisory** — and the split
is the load-bearing decision of this section:

1. **Backend compiled in** — `Backend::is_compiled_in()`, via
   `transcribe::preflight`. Produces an **advisory** `backend_not_compiled`.
2. **Model resolvable** — the same `transcribe::preflight`, which runs exactly
   the resolution `transcribe_source` performs (registry first, then the legacy
   `ggml-<id>.bin` whisper.cpp fallback) so a check that passes here cannot fail
   differently at transcribe time. Produces an **advisory** `model_missing`
   carrying the exact install command. Remote-managed models (Deepgram) resolve
   trivially.
3. **Microphone not muted** — resolve `sources.mic` to a PipeWire node id via
   `setup::PipewireControl::dump_nodes` + `build_devices`, then `get_volume`.
   Produces a **refusal** `mic_muted`.

Only the muted microphone refuses:

- A muted microphone makes the capture worthless by construction. There is
  nothing to keep, so refusing costs nothing and saves the user a wasted
  sentence.
- A missing model or uncompiled backend breaks only the *transcription*. The
  audio is real, irreplaceable, and still transcribable with
  `zwhisper transcribe <file>` once the model is installed. Refusing would throw
  away the one thing that cannot be recreated in order to prevent a failure the
  user can recover from — precisely the "gate in front of a good result" the
  issue's own risk section warns against.

An advisory is still reported through `FailureReported` at start time, so the
notification fires when the user presses the hotkey rather than after they have
finished speaking. Checks 1 and 2 are skipped entirely when
`transcription.auto` is off: a record-only profile promises no transcript, so a
missing model is not a problem it has.

Checks 1 and 2 are pure and cheap. Check 3 spawns `pw-dump` + `wpctl` and is the
only one with a runtime cost: **12–17 ms** measured on the developer box, which
is imperceptible next to the pipeline startup it precedes. It is governed by
`DiagnosticsConfig::mute_probe` and has a hard rule:

> **An inconclusive probe never blocks a recording.** `SetupError::CommandFailed`
> (no `wpctl`, no `pw-dump`), `SetupError::Parse`, a node that cannot be
> resolved, or a probe that exceeds `mute_probe_timeout_ms` are all logged at
> `debug` and the recording proceeds. Only a definitive `muted == true` refuses.

This matters because the probe is the one place where a diagnostic could take
the product away from the user. A tool missing from `PATH` must not cost
anybody their dictation.

The daemon needs `zwhisper-core`'s `setup` feature for check 3. `setup` is
GStreamer-free and pulls only `serde_json`, which `zwhisperd` already depends
on, so the cost is a parse module, not a dependency tree.

### F7 — `Diagnostics1`: the wire surface

`Recorder1` is frozen and `Status` is `(sst)`. The detail goes alongside it, in
a new interface:

```text
interface cz.zajca.Zwhisper1.Diagnostics1 {
    // The last terminal failure the daemon observed. `session_id` is empty
    // when the daemon has not failed since it started.
    method GetLastFailure() -> (s session_id, s job_id, s code, s message,
                                s action, t at_ms);          // (ssssst)

    // Emitted strictly BEFORE the corresponding Recorder1.StateChanged
    // "failed" / Jobs1.JobFailed for the same work item.
    signal FailureReported(s session_id, s job_id, s code, s message,
                           s action);                        // (sssss)
}
```

`job_id` is empty for a recording-side failure; `session_id` is empty for a
standalone `zwhisper transcribe --queue` job.

The ordering guarantee mirrors the existing
`RecordingComplete`-before-`StateChanged` contract and is locked by a
signal-ordering test: a client already listening for `StateChanged "failed"`
has, by the time it arrives, already received the reason.

`FailureReported` is emitted at **all five** `"failed"` sites plus the job
failure arm — including the two sites that persist nothing today (a panicked
lifecycle task, a dropped `done` channel). Those get the `recording_failed` /
`transcribe_failed` catch-all codes rather than continuing to vanish.

`GetLastFailure` exists because `GetStatus` can never report `failed`: it is
what makes the one-shot `zwhisper status` able to say anything at all about a
failure that already happened.

### F8 — History

`HistoryEntry` gains two optional persisted fields:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub(crate) last_error_code: Option<String>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub(crate) last_error_action: Option<String>,
```

Both are `#[serde(default)]`, so `history.json` round-trips without a
`HISTORY_SCHEMA_VERSION` bump and an older file loads unchanged.

`HistorySession` (the wire projection) gains `last_error_code` and
`last_error_action`, moving the signature from `(stssssssss)` to
`(stssssssssss)`. This is a deliberate, reviewed change to a **non-frozen**
interface, made in the same commit as the freeze-test snapshot, and it is safe
across versions because `PROTOCOL_VERSION` is checked before any RPC and bumps
in lockstep with the workspace version.

`set_status` keeps its current semantics (a `Some` error overwrites, a `None`
leaves the previous string alone) and gains the same rule for the two new
fields, so they can never drift out of sync with `last_error`.

### F9 — Configuration

`DiagnosticsConfig` mirrors `SetupConfig`: every threshold is a named constant
with a struct field and a `validate()` that fails fast.

```rust
pub const DEFAULT_LEVEL_INTERVAL_MS: u64 = 100;
pub const DEFAULT_CLIP_PEAK_DB: f32 = -0.1;
pub const DEFAULT_CLIP_WINDOW_RATIO: f32 = 0.02;
pub const DEFAULT_SILENT_RMS_DB: f32 = -60.0;
pub const DEFAULT_MIN_ANALYSIS_MS: u64 = 500;
pub const DEFAULT_MUTE_PROBE: bool = true;
pub const DEFAULT_MUTE_PROBE_TIMEOUT_MS: u64 = 1_500;
```

`-60 dBFS` for silence sits a comfortable 15 dB below the RFC-mic-setup idle
floor ceiling (`-45`), which is itself well below any speech. A recording whose
whole-session RMS is under `-60` contains no speech on any microphone.

`-0.1 dBFS` for clipping is the conservative reading of "at full scale"; the
2 % window ratio means roughly one clipped window in fifty before we say
anything.

The issue is explicit that these are hardware-dependent, so they are overridable
per profile via a new optional table, following the `[transcription.deepgram]`
precedent exactly (`#[serde(default, deny_unknown_fields)]`, an explicit
`Default`, `skip_serializing_if = "Option::is_none"` on the parent field, so
every existing profile round-trips byte-for-byte):

```toml
[diagnostics]
clip_peak_db = -0.5
clip_window_ratio = 0.05
silent_rms_db = -55.0
mute_probe = true
```

Validated in `Profile::validate` next to the existing backend sub-checks:
finite floats, `clip_window_ratio` in `(0.0, 1.0]`, `silent_rms_db` above
`SILENCE_FLOOR_DB`, a non-zero level interval.

### F10 — Surfacing

**CLI exit message.** `zwhisper record` subscribes to `FailureReported`
alongside `StateChanged` (before `StartRecording`, same as its existing
subscriptions) and prints, on failure:

```text
recording failed: microphone `Family 17h/19h HD Audio Controller Analog Stereo`
  (alsa_input.pci-0000_0f_00.6.analog-stereo) is muted
  → unmute it: `wpctl set-mute 52 0`
```

`zwhisper transcribe --queue` does the same around `JobFailed`. Exit codes are
unchanged.

**Notification.** `deliver --listen` switches its failure handler from
`Jobs1.JobFailed` to `Diagnostics1.FailureReported`. Because every `JobFailed`
is now preceded by a `FailureReported` from the same code path, no coverage is
lost and no de-duplication is needed. The notification gains a summary derived
from the code — `Microphone is muted`, `Input is clipping`, `Nothing was
recorded`, `Model not installed`, `Transcription failed` — instead of today's
single `Transcription failed` for everything, and a body of
`"{message}\n→ {action}"`. Urgency is `Critical` for the codes the user must act
on.

This also closes a real gap: a *recording* failure raises no notification at all
today.

**Status.** `WaybarStatus` keeps `text: "failed"` verbatim so existing CSS and
bar configs are untouched. It gains the code as an extra CSS class
(`["zwhisper", "failed", "mic_muted"]`) and renders the tooltip as the message
plus the action. `StatusJson` gains an optional `last_failure` object. The
one-shot `zwhisper status` calls `GetLastFailure` and prints a failure block
when one is present; `--watch` keeps the last `FailureReported` in `WatchState`
and clears it on the next `starting`.

**README.** The quickstart's "Empty result? Your mic gain is almost certainly
too high" becomes a pointer, because the product now says it at the moment it
happens. The Microphone-level troubleshooting section survives as reference and
loses its role as the primary diagnosis channel.

## Data flow (end to end)

```text
StartRecording
  ├─ backend compiled in?  ──no──► FailureReported(backend_not_compiled) ─► record anyway
  ├─ model resolvable?     ──no──► FailureReported(model_missing)        ─► record anyway
  ├─ mic muted?            ──yes─► FailureReported(mic_muted)  ─► REFUSE  [inconclusive ⇒ proceed]
  └─ capture …  level element ──► bus ──► watchdog ──► LevelSummary (constant memory)
       │
       ├─ RecordingError ──► FailureReason ──► FailureReported ──► StateChanged "failed"
       │                                   └─► history{last_error,_code,_action}
       └─ ok ──► transcribe job
             ├─ TranscribeError ──► FailureReason ──► FailureReported ──► JobFailed
             ├─ transcript empty + LevelSummary verdict ──► FailureReported(mic_silent|mic_clipping)
             ├─ transcript empty + healthy levels ──────► FailureReported(empty_transcript)
             └─ transcript non-empty ──► delivered unchanged; verdict logged + persisted only
```

## Security & correctness considerations

- **No secret values.** `cloud_auth` names the *source* of the key — the env
  var name or the secrets file path — and never the key. `SecretString` already
  renders as `***` in both `Debug` and `Display`; the new `key_source` field is
  built from `ResolveSource`, which by construction holds a name or a path.
- **The mute probe cannot deny service.** Only a definitive `muted == true`
  refuses a recording; every error path proceeds.
- **`pw-dump` / `wpctl` stay shell-free.** The probe reuses
  `setup::SystemPipewire`, which already spawns with separate argv elements,
  validates numeric ids, and caps `pw-dump` stdout at 8 MiB.
- **Constant memory.** The level accumulator is five scalars. A 4-hour recording
  costs the same as a 4-second one.
- **No `-inf` / `NaN` leakage.** Level dB values pass through the existing
  `SILENCE_FLOOR_DB` floor before any comparison, and `diagnose_levels` rejects
  non-finite inputs rather than producing a verdict from them.
- **A good transcript is never rejected.** The verdict is consulted only on an
  empty transcript; this is asserted by a test that feeds a clipped
  `LevelSummary` together with a non-empty transcript and requires success.

## Testing strategy

Unit (no hardware, no daemon):

- `FailureCode` ↔ wire string round-trip over an exhaustive slice; a missing
  mapping fails the test. **(#28 acceptance criterion: every reason has a stable
  code covered by a unit test.)**
- Every `FailureReason` constructor: non-empty action, action names a command or
  a concrete path/setting.
- `From<&RecordingError>` / `From<&TranscribeError>` cover every variant —
  exhaustive `match`, so a new error variant fails to compile rather than
  falling into a catch-all unnoticed.
- `diagnose_levels`: silence, clipping at exactly the ratio boundary, one
  transient below the ratio, a healthy recording, a too-short recording,
  non-finite inputs.
- `LevelSummary` folding: unequal window lengths combine to the correct
  duration-weighted RMS.
- `DiagnosticsConfig::validate` rejects each invalid field.
- Profile `[diagnostics]` round-trips byte-for-byte when absent; validation
  rejects out-of-range values.
- Mute probe against `MockPipewire`: muted ⇒ refusal; unmuted ⇒ proceed; each
  `SetupError` variant ⇒ proceed; a `.monitor` node never resolves as the mic.
- Preflight outcome: a missing model is an advisory and never a refusal; a
  `transcription.auto = false` profile gets neither.

Wire:

- `Diagnostics1` added to the freeze test: interface name, method and signal
  signatures.
- `HistorySession` signature snapshot updated in the same commit as the struct.
- Signal ordering: `FailureReported` arrives before `StateChanged "failed"` for
  the same session.

CLI:

- `zwhisper status --json` renders `last_failure`; `--waybar` carries the code
  class and the action in the tooltip while `text` stays `"failed"`.
- `record` prints message + action on a failure.

Hardware verification (developer box, not possible in CI):

- Mute the mic in `pavucontrol` → `zwhisper record` refuses with the unmute
  command, and the command works. **Not yet verified on the runtime box** —
  the probe reads the real `pw-dump`/`wpctl` output correctly (confirmed
  against `zwhisper audio devices --json`: the default source resolves with
  its `muted` flag), but the muted branch has only been exercised against
  `MockPipewire`.
- Raise the input to saturation → a recording reports `mic_clipping` and points
  at `zwhisper audio calibrate`.
- Record with the mic physically disconnected → `mic_silent` naming the node.
- Unplug a USB mic mid-recording → `device_lost` naming the node and the kept
  partial FLAC.
- A profile naming an uninstalled model → a notification with the exact install
  command at the moment recording starts, and the recording still happens.

## Verification performed

Measured on the developer box (Ryzen HD Audio, PipeWire 1.x, GStreamer 1.28.7):

- **The `level` element costs no audio.** Two 4-second captures of the exact
  generated pipeline, with and without the element, produced 133495 and
  132696 bytes of FLAC — a 0.6 % difference, i.e. noise.
- **Levels are measured end to end.** A 5-second recording through the real
  daemon on a private bus folded 51 windows (exactly 5 s at the 100 ms
  interval) reporting `max_peak_db = -11.1`, `rms_db = -29.3`,
  `clipped_windows = 0` — a plausible quiet-room reading.
- **A pre-existing capture flake was ruled out as a cause.** Repeated
  back-to-back captures on this box intermittently yield only 8591 samples
  regardless of the recording length, tripping the
  `verify_samples_match_duration` gate. Reproduced 3 times in 4 runs on
  **unmodified `main`**, so it is device contention, not this change. Worth a
  separate issue.

## Phasing

Each phase is independently reviewable and leaves the tree green.

1. **Core vocabulary.** `diagnostics` module, `DiagnosticsConfig`, profile
   `[diagnostics]` table, `From` impls, `BackendAuth.key_source`. Pure; all
   tests unit.
2. **Level measurement.** `level` element in the pipeline, watchdog
   classification, `LevelSummary`, `RecordingReport.levels`, `diagnose_levels`.
3. **Preflight.** Backend / model / mute checks at `StartRecording`;
   `zwhisperd` gains the `setup` feature.
4. **Wire.** `Diagnostics1` interface and proxy, `HistorySession` widening,
   freeze tests.
5. **Daemon emission.** `FailureReported` at all six sites, empty-transcript
   diagnosis, history persistence, `GetLastFailure` state.
6. **CLI surfaces.** `record`, `transcribe`, `status` (one-shot, `--watch`,
   `--json`, `--waybar`), `deliver` notifications.
7. **Docs.** README quickstart reduction, CHANGELOG.

## Resolved decisions

- **Muted mic refuses the start** rather than recording and reporting
  afterwards. Speaking a whole sentence into a muted microphone is the failure
  the issue is about; catching it 30 ms earlier is the entire value.
- **A missing model or uncompiled backend is reported but does not refuse.**
  Audio is irreplaceable; a transcription failure is recoverable. Refusing would
  trade the former for the latter.
- **An empty transcript with a level verdict fails the job.** The issue is
  explicit that a clipped recording must not return empty text as a success. A
  non-empty transcript is never downgraded.
- **A new `Diagnostics1` interface** rather than widening `Recorder1` (frozen)
  or `Jobs1` (would not cover recording-side failures).
- **`level` element rather than an appsink tap.** It is a pass-through element
  from a plugin already in `depends`, it reports dBFS directly, and it works for
  every profile — the appsink branch exists only for Parakeet.
- **Thresholds live on the profile**, not in a global config, because they are
  per-microphone and profiles are already the per-setup unit.

## Out of scope (this RFC)

- "Model loading" as a user-visible state (#32).
- Warning before `max_duration_minutes` (#27).
- OSD surfacing (#29).
- Retry driven by a diagnosis. `History1.Retry` remains the stub it is today.
- Mid-recording level feedback. The verdict is computed once, at stop.
