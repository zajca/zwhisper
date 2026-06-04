# RFC: Daemon Role — Transcription Jobs, History/Retry, and Session Delivery

## Status

Proposed.

This RFC describes a target architecture for expanding `zwhisperd` from a
recording-only daemon into the always-on owner of transcription jobs and
durable session history, plus a session-bound delivery consumer that restores
the clipboard/notification UX lost when the tray was cut. It is a design
document, not an implementation plan.

It was shaped by an architecture review (`architect`) and an adversarial review
(`devils-advocate`); the findings of both are folded in as decided design rules
or Open Decisions. Confidence-scored findings at or above 80 are treated as
binding constraints here.

## Summary

The product pivoted to **CLI-only**: `zwhisper-tray` and `zwhisper-settings`
are excluded from the workspace. That pivot left three gaps the daemon is the
natural owner of:

1. **Standalone transcription is untracked.** `zwhisper transcribe <file>` runs
   fully in the short-lived CLI process (`crates/zwhisper-cli/src/commands/transcribe.rs`).
   It cannot be queued, queried via `status`, or retried, and it blocks the
   terminal for the whole run.
2. **There is no history or retry.** Nothing records past sessions or lets a
   failed transcription be re-run from the persisted FLAC. The adjacent
   `docs/RFC-audio-source-model.md` explicitly assumes a retry-from-artifact
   path exists but does not provide its owner.
3. **Transcript delivery goes nowhere.** `profile.outputs` already declares
   `OutputDest::{Clipboard, Notification}` (`crates/zwhisper-core/src/profile/schema.rs:356`)
   but only `File` is honoured, because the consumer (the tray) was cut.

This RFC adds two new D-Bus interfaces (`Jobs1`, `History1`) and one new
session-bound consumer (`zwhisper deliver --listen`). The daemon stays
**session-agnostic** (no compositor/clipboard/`WAYLAND_DISPLAY` access, per
IDEA.md §2). The existing `Recorder1` and `Profiles1` wire contracts are kept
**frozen and unchanged**; everything new lands on new interfaces and new
signals.

The load-bearing correction over the initial sketch: routing
`zwhisper transcribe` through the daemon unconditionally is a **reliability
regression** (it breaks the headless guarantee of IDEA.md §5). Daemon-tracked
transcription is therefore **opt-in**, and the local zero-dependency path
remains the default.

## Goals

- Give standalone transcription an optional async, queryable, retryable form
  without removing the local zero-dependency path.
- Make the daemon the single durable owner of session history and retry, with
  the persisted FLAC remaining the real source of truth.
- Restore best-effort Clipboard/Notification delivery via a session-bound
  consumer, honouring the already-declared `profile.outputs`.
- Keep the `Recorder1` + `Profiles1` wire contracts frozen; isolate all new
  surface on new interfaces and a per-interface `ProtocolVersion`.
- Preserve the M3 concurrency guarantee that a new recording can start while a
  previous transcription is still running (the C5 release-before-transcribe
  invariant).
- Fail fast and explicitly; no silent defaults, no hidden auto-retry that could
  silently re-bill a cloud backend.

## Non-Goals

- This RFC does not resurrect the tray or settings GUI; the product is CLI-only.
- This RFC does not put any compositor/clipboard/session access into the daemon.
- This RFC does not redesign the audio or model pipeline; it consumes the
  `docs/RFC-audio-source-model.md` model registry as given (and states the
  sequencing dependency explicitly — see Resolved Decisions).
- This RFC does not add a persistent delivery outbox or replay queue; a missed
  delivery means the transcript is on disk only (IDEA.md §5 rule preserved).
- This RFC does not change cloud consent, secrets handling, or billing policy.

## Current Architecture

`zwhisperd` is a tokio current-thread + zbus daemon. It claims `cz.zajca.Zwhisper1`
at `/cz/zajca/Zwhisper1` and hosts:

- `cz.zajca.Zwhisper1.Recorder1`
  - `StartRecording(s profile) -> (s session_id)`
  - `StopRecording(s session_id) -> (s session_id)`
  - property `Status` = `(sst)` `{ state, active_profile, duration_ms }`
    (`crates/zwhisper-ipc/src/types.rs:24`)
  - property `ProtocolVersion`
  - signal `StateChanged(s new_state, s session_id)`
  - signal `RecordingComplete(s session_id, s audio_path)`
  - signal `TranscriptComplete(s session_id, s transcript_path, x bytes, s backend)`
- `cz.zajca.Zwhisper1.Profiles1`: `List`, `GetActive`, `SetActive`, `Reload`.

`SessionManager` (`crates/zwhisperd/src/session.rs`) holds a **single** recording
slot. The lifecycle task (`crates/zwhisperd/src/lifecycle.rs`) finalizes the
recording, **releases the slot, then runs auto-transcribe** — so a new recording
may begin while the prior transcription is still in flight. `shutdown()`
(`crates/zwhisperd/src/main.rs`) awaits in-flight lifecycle tasks before
releasing the bus name, but only on graceful SIGINT/SIGTERM, not SIGKILL/OOM.

The whisper-cpp backend shells out to a `whisper-cli` subprocess; Deepgram is an
HTTP upload. Both write `<audio>.txt` + `<audio>.json` next to the source.

## Proposed Architecture

```text
                 ┌─────────────────────────────────────────────┐
 zwhisper CLI    │ zwhisperd (session-agnostic)                 │
 (thin client)   │                                             │
  toggle/status ─┼─► Recorder1  (FROZEN: recording slot)        │
  transcribe ────┼─► Jobs1      (transcription job queue) ──────┼─► JobQueue task
  history/retry ─┼─► History1   (durable index, single writer) ─┼─► HistoryWriter task
  profile ───────┼─► Profiles1  (FROZEN)                        │
                 │                                             │
                 │   signals: Jobs1.JobCompleted / JobFailed    │
                 └──────────────────┬──────────────────────────┘
                                    │ D-Bus signal (best-effort)
              ┌─────────────────────▼───────────────────────────┐
 zwhisper     │ zwhisper deliver --listen                       │
 deliver      │  graphical-session.target unit                  │
 (session)    │  honours OutputDest::{Clipboard, Notification}  │
              └─────────────────────────────────────────────────┘
```

Three independent units of work. Each is specified below with the review
findings that constrain it.

### Feature 1 — Transcription jobs (`Jobs1`)

A new interface, deliberately **not** folded onto `Recorder1` (recording is a
single-slot resource; transcription is a multi-item queue — different lifetimes,
different concurrency).

```text
interface cz.zajca.Zwhisper1.Jobs1 {
    // Enqueue a standalone transcription. Returns immediately with a job id.
    TranscribeFile(s path, s backend, s model, s lang) -> (s job_id);

    // Cancel a queued or running job (best-effort; running whisper-cli is
    // killed via its process group).
    Cancel(s job_id) -> ();

    // Snapshot the queue. New interface ⇒ free to choose a rich shape; no
    // frozen struct to preserve.
    ListJobs() -> (a(ssst));   // [(job_id, state, label, submitted_ms)]

    property ProtocolVersion;

    // DISTINCT from Recorder1.TranscriptComplete — never reuse that signal
    // (Finding arch#1 / DA#3). job_id is a job namespace, not a session id.
    // submit_mode ∈ "foreground" | "detached" | "auto" drives the consumer's
    // intent-based stale-clipboard guard (F3.3).
    signal JobCompleted(s job_id, s submit_mode, s profile, aas outputs, s transcript_path, x bytes, s backend);
    signal JobFailed(s job_id, s error);
    signal JobProgress(s job_id, s state);   // queued → running → done/failed
}
```

Design rules (each closes a review finding):

- **F1.1 — Local path stays the default (DA#1, conf 95).** `zwhisper transcribe
  <file>` continues to run **locally in the CLI process** with zero daemon
  dependency, preserving the headless/ssh/cron guarantee of IDEA.md §5.
  Daemon-tracked transcription is opt-in: `--queue` (enqueue + wait via signal)
  or `--detach` (enqueue, print `job_id`, return). Only daemon-routed jobs enter
  history (Feature 2). This keeps the scriptable contract intact while adding
  the async/tracked form.
- **F1.2 — `--wait`/`--queue` reconnect semantics (DA#2, conf 88).** The
  blocking CLI path MUST specify: a bounded wait timeout, a defined exit code on
  D-Bus connection loss, and that a job in flight at daemon death is recovered
  daemon-side via Feature 2 startup recovery (the waiting CLI exits non-zero and
  instructs the user to `zwhisper history` / `zwhisper retry`). The CLI never
  hangs indefinitely.
- **F1.3 — Sibling queue, not the recording slot (arch#2, conf 88).** The job
  queue is a separate component from `SessionManager`. Recording keeps its own
  single slot; transcription jobs run in their own serialized lane (default
  concurrency 1 — whisper-cli is heavy — configurable). Recording and
  transcription proceed concurrently, preserving the C5 invariant
  (`lifecycle.rs` releases the slot before transcribe). Auto-transcribe,
  `TranscribeFile`, and `Retry` are all jobs on this one lane. **[Decided:
  global serialized concurrency = 1, configurable.]** Per-backend lanes (e.g.
  parallel I/O-bound Deepgram jobs) are deferred until a real need appears
  (YAGNI), not built up front.
- **F1.4 — Path + input validation.** `path` is validated as a regular,
  readable file inside an allowed root (no traversal, no device files); backend
  and model resolve through the existing resolution chain; no secrets are ever
  placed in `JobCompleted`/`JobFailed` payloads.

### Feature 2 — History and retry (`History1`)

```text
interface cz.zajca.Zwhisper1.History1 {
    ListSessions(u limit, u offset) -> (a(...));   // recent entries + status
    GetSession(s id)                -> (struct);
    Retry(s id)                     -> (s job_id);  // re-transcribe from FLAC
    Forget(s id, b delete_files)    -> ();
    property ProtocolVersion;
}
```

Storage and correctness rules:

- **F2.1 — Versioned, disposable index over the real source of truth (arch#3,
  conf 85).** Persist at `$XDG_STATE_HOME/zwhisper/history.json`, NOT in
  `~/Recordings`. The index carries its own `schema_version` and migration
  chain, mirroring the profile schema. The FLAC files remain the source of
  truth; the index is a queryable cache that can be rebuilt by scanning the
  recordings directory. An entry:
  `{ session_id, created_at, profile, audio_path, codec, native_rate, channels,
  transcript_paths, backend, model, lang, status, last_error, whisper_pid? }`,
  `status ∈ recorded | transcribing | interrupted | done | failed`.
- **F2.2 — One serialized writer task (arch#2 / DA#6, conf 88).** The
  "daemon is the single writer" rule is made **structural**: a dedicated
  `HistoryWriter` task owns the file exclusively and is fed via an mpsc channel.
  Concurrent jobs (e.g. a post-record auto-transcribe and a `TranscribeFile`
  running at once) send updates to that one task; there is no independent
  read-modify-write + rename from multiple callers (which would lose updates).
  Writes are atomic (temp + fsync + rename).
- **F2.3 — Startup recovery without double-transcribe (DA#7, arch#4, conf
  82).** On start, entries left in `transcribing` are marked `interrupted` (a
  distinct state, not silently `failed`). Recovery does **not** auto-retry —
  auto-retry could re-bill Deepgram or race a surviving subprocess. Before any
  `Retry` re-runs whisper-cpp, the daemon checks whether the recorded
  `whisper_pid` (or its process group) is still alive and whether output files
  are being written; an orphaned subprocess is reaped first. whisper-cli is
  spawned in its own process group so daemon shutdown/kill can propagate.
- **F2.4 — Retry is gated on the model registry (DA#9, conf 83). [Decided:
  wait for the audio RFC.]** `Retry` re-resolves the model from the
  `docs/RFC-audio-source-model.md` registry and decodes PCM from the FLAC. Rather
  than carry a legacy `transcribe_file` fallback and maintain two resolution
  paths, `Retry` lands only after the audio RFC. `History1` ships its read +
  housekeeping surface (`ListSessions`, `GetSession`, `Forget`) in Phase 2; the
  `Retry` method is registered but returns a typed `RetryUnavailable` error until
  Phase 4 wires it to the registry. This keeps history queryable immediately
  without duplicating model resolution.
- **F2.5 — `Forget` and retention semantics (DA#8, conf 85). [Decided: no
  silent purge.]** There is **no auto-purge by default** — user audio is never
  silently deleted. `Forget(id, delete_files)` removes the index entry and, only
  if `delete_files`, the referenced audio/transcript files. `retention_days`
  (IDEA §5) is strictly opt-in per profile; when enabled it atomically removes
  both the file and the entry. The index itself is unbounded (JSON is cheap);
  only the `ListSessions` display may be limited. `ListSessions`/`Retry` over an
  entry whose `audio_path` no longer exists returns a typed `AudioNotFound`, not
  an opaque I/O error.

### Feature 3 — Session-bound delivery (`zwhisper deliver --listen`)

The daemon emits a completion signal; a session-scoped consumer performs the
session-bound side effects. The daemon never touches the clipboard.

- **F3.1 — The daemon DOES change slightly (arch#5 / DA#10, conf 87/80).** The
  initial sketch claimed "no daemon change". That is wrong: the consumer must
  not re-resolve the profile from disk (it can diverge from the daemon's cached
  view and from the profile that was active at recording time). Therefore the
  daemon **resolves `profile.outputs` at completion time and carries it in the
  signal** (`Jobs1.JobCompleted(... s profile, aas outputs ...)`), and persists
  the profile name in the history entry. The consumer acts on the signal
  payload, never on a fresh disk read.
- **F3.2 — Best-effort only; missed signals are lost (DA#4, conf 90).** The
  consumer is a systemd `--user` unit bound to `graphical-session.target`. D-Bus
  signals are not buffered for late subscribers and IDEA §5 forbids a persistent
  outbox. If the consumer is not subscribed when the signal fires (login race,
  crash/restart window, headless, multiple graphical sessions), the transcript
  is on disk only. This limitation is documented, not hidden; the manual
  fallback is `zwhisper output last --to clipboard|notify`.
- **F3.3 — Stale-clipboard guard, intent-based (DA#5, conf 85). [Decided:
  intent, not a timer.]** Unconditional clipboard injection of a job that
  completed long after submission (`--detach` + a long recording) is a
  paste-bomb hazard: thousands of words land in whatever window has focus. The
  decisive signal is **user intent, not elapsed time**: a synchronous foreground
  transcription (`transcribe --wait` / dictation) means the user is actively
  waiting for the clipboard, whereas `--detach` and post-record background
  auto-transcribe mean they are not. Therefore, for `OutputDest::Clipboard`:
  synchronous/foreground jobs inject directly; `--detach` and background jobs
  raise a `Notification` with a "copy to clipboard" action instead of injecting.
  A configurable completion-latency threshold (default ~10 s) is retained only as
  a secondary guard for the foreground path's edge cases. The job's submission
  mode is carried in the `JobCompleted` payload so the consumer can apply this
  without re-deriving intent.
- **F3.4 — Single consumer.** Running two `deliver --listen` instances would
  double-deliver. The unit is single-instance; a second invocation detects the
  bus name / lock and exits.
- **F3.5 — Packaging and activation (arch#5). [Decided: same package,
  auto-enable.]** The consumer ships in the same package as the daemon, and its
  systemd `--user` unit is **auto-enabled** on install. The headless concern is
  closed structurally by the `graphical-session.target` binding (F3.2): on a host
  with no graphical session the target is never reached, so an auto-enabled unit
  simply never activates and logs nothing — auto-enable is therefore safe even
  for headless installs. The unit must still exit cleanly (not error-loop) if it
  is somehow started without `WAYLAND_DISPLAY`/`DISPLAY`.

### Protocol versioning

- **F4.1 — Per-interface version with presence fallback (arch#6/#7, conf
  90/80).** The frozen `Recorder1.Status` `(sst)` struct is **not** extended;
  job and history state live on the new interfaces. Each new interface carries
  its own `ProtocolVersion`. A client probes for `Jobs1`/`History1` presence and
  degrades gracefully against an older daemon that lacks them (the CLI prints a
  "daemon too old for this command" hint rather than crashing).

## CLI Surface

- `zwhisper transcribe <file>` — local by default (unchanged contract);
  `--queue` (daemon job, wait), `--detach` (daemon job, print job_id).
- `zwhisper jobs` — list queued/running jobs; `zwhisper jobs cancel <id>`.
- `zwhisper history [--limit N]` — list recent sessions + status.
- `zwhisper retry <id>` — re-transcribe a session from its FLAC.
- `zwhisper output last --to clipboard|notify` — one-shot manual delivery
  (also the documented fallback for missed best-effort delivery).
- `zwhisper deliver --listen` — the session consumer (run via systemd unit).

## Migration Strategy

Phasing is reordered from the initial sketch because Feature 3 is the highest
user value and — once F3.1 is accepted — its daemon dependency is just adding a
field to a *new* signal, which only exists once Feature 1 lands. So jobs come
first after all.

### Phase 1 — `Jobs1` + sibling job queue

- Add the `Jobs1` interface, the sibling `JobQueue` task (serialized lane,
  configurable concurrency), and `JobCompleted`/`JobFailed`/`JobProgress`.
- Route auto-transcribe (post-record) onto the queue without changing recording
  concurrency (F1.3).
- CLI: `transcribe --queue/--detach` (local default unchanged), `jobs`.
- Bounded `--wait` with defined connection-loss exit code (F1.2).

### Phase 2 — `History1` read + durable store

- Add the `HistoryWriter` single-writer task and the versioned `history.json`.
- Record every daemon-routed job (recording auto-transcribe + queued
  transcribe) into history.
- Startup recovery (`interrupted` state, no auto-retry, orphan reap) (F2.3).
- `History1` ships `ListSessions`/`GetSession`/`Forget`. The `Retry` method is
  registered but returns `RetryUnavailable` until Phase 4 (F2.4) — no legacy
  fallback path is built.
- CLI: `history`. (`retry` prints the "available after the audio RFC" hint.)

### Phase 3 — `deliver --listen` consumer

- New session consumer + auto-enabled `graphical-session.target` systemd
  `--user` unit (F3.5).
- Daemon carries `submit_mode` + resolved `profile` + `outputs` in
  `JobCompleted` (F3.1/F3.3) and persists the profile in history.
- Intent-based stale-clipboard guard (F3.3); document best-effort limitation
  (F3.2).
- CLI: `output last --to …` one-shot fallback.

### Phase 4 — Audio RFC convergence

- Once `docs/RFC-audio-source-model.md` ships, wire `Retry` to model-registry
  re-resolution + FLAC decode and remove the `RetryUnavailable` stub (F2.4).
  This phase is a hard dependency, not a fallback swap.

## Testing Strategy

### Unit
- Job queue: serialization, configurable concurrency, cancel of queued vs
  running, recording-concurrent-with-transcribe (C5 preserved).
- History writer: concurrent updates from two jobs do not lose entries (F2.2);
  atomic write under simulated crash; index `schema_version` migration.
- Startup recovery: `transcribing` → `interrupted`; no auto-retry; orphan
  whisper-cli detection.
- `Forget`/retention: index+file deletion semantics; `AudioNotFound` on retry of
  purged entry.
- Delivery: stale-clipboard threshold (inject vs notify-with-action); single
  consumer lock; signal payload (`profile`+`outputs`) drives the action.

### Integration
- `transcribe` local path works with the daemon stopped (headless guarantee).
- `transcribe --queue` survives a daemon restart mid-job with a defined CLI exit
  and a recoverable history entry.
- `JobCompleted` is distinct from `Recorder1.TranscriptComplete`; an old
  subscriber on the latter is unaffected.
- Old client vs new daemon and new client vs old daemon (no `Jobs1`/`History1`)
  both degrade gracefully.

### Failure
- SIGKILL the daemon mid-transcribe: no double-transcribe on restart; orphan
  reaped; transcript not corrupted.
- `deliver --listen` not subscribed when signal fires: transcript on disk, no
  crash, fallback command works.
- whisper-cli output collision is impossible after recovery.

## Alternatives Considered

- **Fold jobs/history onto `Recorder1` + extend `Status`.** Rejected: breaks the
  frozen `(sst)` wire struct and the locked signal ordering (arch#1/#6).
- **Single queue for recording + transcription.** Rejected: re-couples resources
  the M3 C5 invariant deliberately decoupled (arch#2/DA#6).
- **Route `transcribe` through the daemon unconditionally.** Rejected: regresses
  the headless zero-dependency guarantee (DA#1) — the single most dangerous
  assumption of the initial sketch.
- **Daemon performs clipboard delivery itself.** Rejected: violates the
  session-agnostic boundary (IDEA §2).
- **Persistent delivery outbox / replay.** Rejected: IDEA §5; replaying stale
  clipboard injections is worse than a missed delivery.

## Risks

- A long `--detach` job + Clipboard output is a paste-bomb hazard; mitigated by
  the stale-clipboard guard (F3.3) but the threshold is a tuning risk.
- Best-effort delivery will generate "my transcript wasn't copied" reports;
  mitigated only by clear documentation (F3.2).
- History index can diverge from disk if the user edits the recordings directory
  manually; mitigated by treating the index as a rebuildable cache (F2.1).
- `Retry` is partially blocked on the audio RFC; mitigated by the legacy
  fallback (F2.4) but the two RFCs must be sequenced explicitly.
- Orphaned whisper-cli subprocesses after OOM are a correctness hazard;
  mitigated by process groups + recovery checks (F2.3).

## Resolved Decisions

All five Open Decisions from the initial draft were resolved with the
maintainer (2026-06-04):

- **Audio RFC sequencing → wait for the audio RFC.** No legacy
  `transcribe_file` retry fallback is built. `History1` ships its read/housekeeping
  surface in Phase 2; `Retry` returns `RetryUnavailable` until Phase 4 wires it
  to the model registry (F2.4). Avoids maintaining two model-resolution paths.
- **Stale-clipboard guard → intent-based, not a timer.** Synchronous/foreground
  jobs inject to the clipboard; `--detach` and background jobs notify-with-action.
  `submit_mode` rides in `JobCompleted`. A ~10 s latency threshold remains only as
  a secondary foreground guard (F3.3).
- **Default job concurrency → global serialized = 1, configurable.** Per-backend
  parallel lanes are deferred (YAGNI) (F1.3).
- **History retention → no auto-purge by default.** `retention_days` strictly
  opt-in per profile; user audio is never silently deleted; the index is
  unbounded (F2.5).
- **`deliver --listen` packaging → same package, auto-enabled unit.** Safe for
  headless because the `graphical-session.target` binding prevents activation
  where there is no display (F3.5).

## Remaining Open Questions

- Exact `history.json` `schema_version` 1 field set and the migration harness it
  reuses from the profile schema.
- Whether `JobProgress` should carry coarse percent/ETA for long jobs or stay a
  pure state-transition signal.
- The bounded `--wait` timeout value and its exit-code contract on daemon
  restart (F1.2) — needs a named config value, no silent default.

## Recommended Direction

Adopt the thick-daemon role, staged. Keep `Recorder1`/`Profiles1` frozen and add
`Jobs1` + `History1` as new versioned interfaces, with a sibling job queue that
never re-couples recording and transcription. Keep `zwhisper transcribe` local
by default and make daemon-tracked transcription opt-in, so the headless
guarantee survives. Restore delivery through a session-bound consumer that acts
on a signal payload (resolved profile + outputs) the daemon emits, with an
explicit best-effort contract and a stale-clipboard guard. This gives the daemon
a coherent always-on role — recording engine, transcription job queue, history
source, and event bus — without breaking any frozen contract or the
session-agnostic boundary.
