# M1 — Whisper.cpp post-process: implementation plan

> Target milestone from [IDEA.md § 11](../IDEA.md#11-roadmap). Builds
> on the M0 walking skeleton (commit `4418d52`, fix bundle `fa0f25a`)
> and turns the stubbed `zwhisper transcribe` command into a working
> local-only post-process pipeline that hands an M0 FLAC to a
> user-installed `whisper.cpp` build and persists the resulting
> transcript next to the audio.

## Status snapshot (2026-04-30)

| Area | State | Evidence |
|---|---|---|
| `zwhisper transcribe` subcommand parses args | done | `crates/zwhisper-cli/src/cli.rs:36` (`TranscribeArgs`); `run_transcribe` currently `bail!`s with "not implemented yet — pending M1 whisper.cpp integration" |
| `whisper-cli` discovery | not done | no module yet; IDEA.md § 4 lists 5-step lookup, none implemented |
| `Transcriber` trait | not done | IDEA.md § 4 defines the interface; no Rust code yet |
| Model resolution from name → path | not done | layout fixed (`~/.local/share/zwhisper/models/ggml-{name}.bin`) but no resolver |
| `--transcribe` flag on `record` subcommand | not done | M0 `RecordArgs` has no `--transcribe`/`--model`/`--lang` mirror |
| Transcript output (`.txt` / `.json`) | not done | IDEA.md § 11 DoD requires both shapes valid |
| Backtest logging of transcribe runs | not done | personal CLAUDE.md mandates `logs/` JSON for backtesting |

**Verdict:** M1 is greenfield. Scaffolding (CLI args, error wrapping)
exists; the entire transcription path needs to be built.

## Definition of done (verbatim from IDEA.md § 11)

1. `zwhisper record --mic default --monitor default --output x.flac
   --duration 60 --transcribe whisper-cpp --model small --lang cs`
   produces a valid FLAC **and** a transcript next to it
   (`x.flac.txt` + `x.flac.json`).
2. `zwhisper transcribe x.flac --backend whisper-cpp --model small
   --language cs` consumes an existing FLAC and writes the same
   transcript pair.
3. `whisper-cli` (or `whisper-cpp`) is **detected from `$PATH`**, not
   hardcoded. Detection order matches IDEA.md § 4:
   1. `ZWHISPER_WHISPER_CLI` env var (explicit path)
   2. `whisper-cli` in `$PATH`
   3. `whisper-cpp` in `$PATH` (other-distro alias)
   4. `~/.local/bin/whisper-cli`
4. Models are resolved by **name only** from
   `~/.local/share/zwhisper/models/ggml-{name}.bin`; cesta v profilu
   nikdy. (IDEA.md § 4 "Modely")
5. Failures are **typed errors**, not stderr regex matches: missing
   binary, missing model, exec failure, output-file missing — each
   is its own `TranscribeError` variant. Sad-path tests cover each.
6. Validní transcript: `.txt` is non-empty UTF-8 (when audio is
   non-silent) and `.json` parses as the whisper.cpp segment array
   shape (`{transcription: [{ timestamps, offsets, text }, …]}`).

Out of scope (deferred to later milestones):

- Streaming / live transcription (M5 — Deepgram / AssemblyAI)
- Diarization (M5; whisper.cpp doesn't do true diarization)
- Cloud backends (M5)
- Schema-versioned profiles selecting models (M2)
- Daemon orchestration / D-Bus `TranscriptComplete` signal (M3)
- Tray indicator wiring transcribe state (M4)
- Settings GUI / model downloader (M7)
- Channel-attributed stereo split transcribe (M2 with profiles)

## Non-goals for M1

- Building or vendoring `whisper.cpp` ourselves. The user installs it
  through their package manager or upstream releases; we **detect**
  it. PKGBUILD will declare `optdepends`, not `depends`.
- Bundling models. The user supplies `ggml-*.bin` files. M1 surfaces
  a clear "model not found" error with the canonical path; M7 adds
  a downloader.
- Streaming partial transcripts to stdout. M1 is post-process; the
  audio file is closed before transcription starts.
- Re-encoding the FLAC. whisper.cpp accepts WAV natively but copes
  with FLAC via libsndfile; if the host's whisper-cli rejects FLAC,
  we add a single `flac → wav` step inside the runner — but **not
  another GStreamer pipeline**. Use the `flac` CLI (already required
  by M0 host setup), not `gstreamer`.

## Architecture for M1

Single binary `zwhisper`, same workspace as M0. New module layout
inside `zwhisper-cli`, mirroring the `audio/` shape so the M3
daemon split stays mechanical:

```
crates/zwhisper-cli/src/
├── main.rs            # entrypoint (M0)
├── cli.rs             # clap args (extended in M1: --transcribe/--model/--lang on record)
├── audio/             # M0
└── transcribe/
    ├── mod.rs         # public façade: `transcribe_file` + `Transcriber` trait
    ├── error.rs       # `TranscribeError` (thiserror) — variants stable across M1→M3
    ├── discovery.rs   # 5-step `whisper-cli` lookup (env, PATH, ~/.local/bin)
    ├── models.rs      # `~/.local/share/zwhisper/models/ggml-<name>.bin` resolver
    └── whisper_cpp.rs # `WhisperCppLocal` impl: runs the subprocess
```

Rationale: identical separation as M0 (`error`, `state`, `pipeline`,
`recorder`). M3 daemon split moves both `audio/` and `transcribe/`
into a `zwhisper-core` crate; keeping them under `src/` for now is
less yak-shaving than splitting prematurely.

### Public API rules (M3 lock-ins)

These are non-negotiable for M1 because reversing them later means
rewriting M3 D-Bus surface (IDEA.md § 2.3 `TranscriptComplete`
signal):

1. **`Transcriber` trait, sync for now.** IDEA.md § 4 defines the
   trait as `async fn transcribe_file`. M1 implements it
   synchronously because there is exactly one impl
   (`WhisperCppLocal`) and no streaming. The trait method signature
   stays `async fn` — implementors can be sync today, await-able
   tomorrow:

   ```rust
   #[async_trait::async_trait]
   pub(crate) trait Transcriber: Send + Sync {
       fn id(&self) -> &'static str;
       fn capabilities(&self) -> Capabilities;
       async fn transcribe_file(
           &self,
           audio: &Path,
           opts: &TranscribeOpts,
       ) -> Result<TranscriptArtifacts, TranscribeError>;
   }
   ```

2. **No `whisper.cpp`-specific types in any `pub` signature.**
   `TranscribeOpts { model: String, language: String }` stays
   plain Rust. The internal subprocess spawn lives `pub(crate)`.
   Backend identifier is a static string (`"whisper-cpp"`) — M5
   will add `"deepgram"`, `"assemblyai"`, `"openai"`.

3. **`TranscriptArtifacts` is the canonical return shape.**
   ```rust
   pub(crate) struct TranscriptArtifacts {
       pub txt_path: PathBuf,
       pub json_path: PathBuf,
       pub duration: Duration,        // wall-clock wall of the call
       pub audio_duration: Duration,  // FLAC duration the backend saw
       pub language: String,           // resolved (after `auto` detection)
       pub model: String,              // resolved, e.g. "small"
   }
   ```
   M3 D-Bus `TranscriptComplete(s session_id, s txt_path, s json_path)`
   takes both paths verbatim — adding them as an afterthought later
   means a wire-format break.

4. **`TranscribeError` is `thiserror`-based with one variant per
   failure class.** No `String`-blob errors:
   - `BackendUnavailable { searched: Vec<PathBuf> }` — discovery
     failed; `searched` lets the user see exactly which paths we
     looked at.
   - `ModelNotFound { name: String, expected: PathBuf }` — model
     name resolved to a path that does not exist.
   - `BackendExitedNonZero { tool: PathBuf, status: ExitStatus,
     stderr: String }` — backend ran but failed.
   - `OutputMissing { path: PathBuf }` — backend exited 0 but did
     not produce the expected file.
   - `OutputUnreadable { path: PathBuf, source: io::Error }` —
     produced but cannot be opened.
   - `BackendSpawn { tool: PathBuf, source: io::Error }` — could
     not spawn at all.
   - `InputAudio { path: PathBuf, source: io::Error }` — input FLAC
     not openable.

5. **Output file paths derived from the audio path.** Given
   `x.flac`, write `x.flac.txt` and `x.flac.json`. **Not** `x.txt`
   — IDEA.md § 4 implicitly assumes the audio path is the join key
   for retroactive transcription, and M3 will key by audio path
   too. This also avoids stomping a sibling `x.txt`.

### Subprocess invocation contract

Locked in by M1; M3 moves the spawn into the daemon process.

- Working directory: `tempfile::TempDir` per call. We never let the
  user's `$PWD` influence whisper-cli's relative-path output flags.
- Environment: forwarded verbatim (whisper-cli reads no env). We do
  **not** clear `$PATH` because some packages depend on it.
- Stdin: closed.
- Stdout/stderr: piped, captured, and logged (truncated to 4 KiB
  each on success; full body on failure). `tracing` event at
  `info`, never `eprintln!`.
- Args:
  ```
  whisper-cli
      --model    <resolved model path>
      --language <iso code | "auto">
      --output-txt
      --output-json
      --output-file <stem>      # writes <stem>.txt and <stem>.json
      <input audio>
  ```
- Exit-code interpretation: 0 = success; any non-zero ⇒
  `BackendExitedNonZero`. Stderr is preserved verbatim — whisper.cpp
  prints model-load progress to stderr even on success, so we do
  **not** treat any-stderr-output as failure.

## Phased plan

Each phase is a single PR-sized commit set. Phases run sequentially;
each builds on the previous one's verification artefacts.

### Phase 0 — Host prerequisites + research (~1 h)

- Append to `docs/M0-host-setup.md` (do not split — same host setup):
  - `whisper.cpp` install paths on Arch (AUR `whisper.cpp` /
    `whisper.cpp-cuda`) and the upstream "build from source" link
  - `flac` CLI (already required by M0 soak verification — re-state)
- Confirm against `whisper-cli --help` on the maintainer's host that
  `--output-txt`, `--output-json`, `--output-file <stem>` exist with
  the semantics the plan assumes. If upstream renamed any flag,
  lock the actual flag name in this section before Phase 2 starts.
- Verify the JSON output schema by running `whisper-cli` against a
  10-second test clip and checking the top-level shape matches the
  validator we'll write in Phase 5.
- Do not assume model presence; document the
  `~/.local/share/zwhisper/models/` layout and the SHA256
  manifest URL pattern (used by M7 settings UI later, but referenced
  here so the resolver code points at the right path now).

**Done when**: `docs/M0-host-setup.md` lists whisper.cpp + flac;
flag-name reality check committed as a small note in this plan if
upstream diverged.

### Phase 1 — Dependencies + module skeleton (~1 h)

- Add to `workspace.dependencies`:
  - `async-trait = "0.1"` — IDEA.md trait shape
  - `serde = { version = "1", features = ["derive"] }`
  - `serde_json = "1"`
  - `which = "6"` — for `$PATH` lookups (cross-distro alias safe)
- Add to `zwhisper-cli/Cargo.toml`: same plus `tokio` `process` /
  `io-util` features (already partially enabled by M0 for
  `signal::ctrl_c`).
- Create `transcribe/{mod.rs, error.rs, discovery.rs, models.rs,
  whisper_cpp.rs}` as empty/stub modules wired through
  `transcribe/mod.rs` and into `main.rs`.
- Confirm `cargo build --workspace` and `cargo test --workspace`
  still pass. Do not flip the existing `bail!` yet.

**Done when**: `cargo build --workspace` clean; `cargo clippy
--workspace --all-targets --all-features -- -D warnings` clean.

### Phase 2 — `whisper-cli` discovery + model resolver (~3 h)

- `discovery.rs` exposes:
  ```rust
  pub(crate) fn locate_whisper_cli() -> Result<PathBuf, TranscribeError>;
  ```
  implementing the 5-step lookup from IDEA.md § 4. The function is
  testable via a `Locator` trait so the env/`$PATH`/filesystem
  lookups can be mocked.
- `models.rs` exposes:
  ```rust
  pub(crate) fn resolve_model(name: &str) -> Result<PathBuf, TranscribeError>;
  ```
  Path = `dirs::data_local_dir()
  .unwrap_or_else(|| ~/.local/share)
  .join("zwhisper/models")
  .join(format!("ggml-{name}.bin"))`. Validates the name with the
  same allow-list shape `[A-Za-z0-9._-]+` (no `:` because models
  don't carry media-class qualifiers; no `/` because traversal).
- Tests cover: every step of the discovery lookup; `auto` model
  rejected with a clear message; `..`-style traversal rejected;
  missing model surfaces `ModelNotFound { expected }` with the path
  the user can ls.

**Done when**:
- `cargo test --workspace --lib transcribe::` runs ≥ 12 unit tests
  green.
- Manual smoke: `ZWHISPER_WHISPER_CLI=/usr/bin/whisper-cli
  zwhisper transcribe --backend whisper-cpp --model nonexistent
  /tmp/x.flac` returns a `ModelNotFound` typed error, not a
  whisper-cli stderr blob.

### Phase 3 — `WhisperCppLocal` runner (~4 h)

- `whisper_cpp.rs` builds the subprocess per the contract above.
- Uses `tokio::process::Command` so M3 daemon can `select!` against
  cancellation tokens. M1 simply `await`s the spawned process.
- Subprocess working directory is a per-call `TempDir`. After a
  successful run, the `<stem>.txt` and `<stem>.json` are
  `std::fs::rename`d next to the audio file (atomic on the same
  filesystem; if cross-fs, fall back to copy + remove with a typed
  error path).
- Captures stderr/stdout. Emits `tracing` events: at `info` start
  (model + language + audio path), at `info` end (duration, output
  paths, audio duration parsed from JSON), at `error` if the
  subprocess fails.
- Backtest log: structured JSON line written to
  `${XDG_STATE_HOME:-~/.local/state}/zwhisper/transcribe.log`
  (one line per call: `{ts, audio, model, language, status, duration_ms,
  txt_path, json_path}`). Honours global "no transcript text in
  logs" rule from IDEA.md § 7 — log only paths and metadata.

**Done when**:
- 1-second silent FLAC produces an empty-but-valid `.txt` and a
  `.json` with `transcription: []`.
- 10-second voice clip produces a non-empty `.txt` and a `.json`
  with at least one segment carrying `text`.
- Failure modes (exit 1, missing model, missing binary) each
  surface as the matching `TranscribeError` variant in unit + smoke
  tests.

### Phase 4 — Wire CLI surfaces (~2 h)

- `cli::TranscribeArgs` already has `backend`, `model`, `language`.
  Replace the `bail!` in `run_transcribe` with a call into
  `transcribe::transcribe_file`, mapping errors via `color_eyre`.
- Extend `cli::RecordArgs` with three new optional flags:
  ```rust
  /// Backend for post-record transcription (omit to skip).
  #[arg(long)]
  pub(crate) transcribe: Option<String>,

  /// Model name (required when --transcribe is set).
  #[arg(long, requires = "transcribe", default_value = "small")]
  pub(crate) model: String,

  /// Language ISO code or `auto`.
  #[arg(long, requires = "transcribe", default_value = "auto")]
  pub(crate) lang: String,
  ```
  After a successful `record_blocking`, if `--transcribe` is set,
  dispatch to `transcribe::transcribe_file` against the just-written
  FLAC. Failures are surfaced as the recording-then-transcribe
  outcome: the FLAC is **not** deleted on transcribe failure (DoD #1
  for M0 already met; user keeps the audio).
- The `--transcribe whisper-cpp` value is the only one accepted in
  M1; unknown values fail with a typed `BackendUnknown` error
  listing the supported set. M5 widens this.

**Done when**:
- `zwhisper record --output /tmp/x.flac --duration 3 --transcribe
  whisper-cpp --model small --lang en` produces `x.flac`,
  `x.flac.txt`, `x.flac.json`.
- `zwhisper transcribe /tmp/x.flac --backend whisper-cpp --model
  small --language en` consumes an existing FLAC and produces the
  same outputs.
- The status banner stays accurate (`zwhisper status` is M3+
  territory; for M1 it stays at "walking skeleton" wording).

### Phase 5 — Tests (~3 h)

- Unit tests (no `whisper.cpp` install needed):
  - `discovery.rs`: 5 paths × found/not-found combos.
  - `models.rs`: name validation, traversal rejection, missing file.
  - JSON shape validator: parses a recorded fixture committed under
    `crates/zwhisper-cli/tests/fixtures/whisper-cpp-segments.json`.
  - CLI parsing: `--transcribe whisper-cpp --model small --lang cs`
    materialises the right argv struct; `--model` without
    `--transcribe` is rejected by clap's `requires`.
- Integration tests (`#[test]`, runtime-skip pattern from M0):
  - `transcribe_writes_txt_and_json` — runs against a real
    `whisper-cli` if found; runtime-skips with `[SKIP]` log if
    `locate_whisper_cli()` returns `BackendUnavailable` or model is
    missing. Mirrors the M0 `record_writes_valid_flac` runtime-skip
    pattern; no compile-time feature flag.
  - `record_then_transcribe_end_to_end` — combines the M0 live
    capture with the M1 transcribe step; runtime-skipped on hosts
    without both PipeWire and whisper-cli.

**Done when**:
- `cargo test --workspace` is green; runtime skips are visible in
  test output (no silent gaps).
- `cargo test --workspace --no-default-features` still compiles
  (audio-it default but transcribe tests not feature-gated).

### Phase 6 — Verification + sign-off (~1 h)

- Add `docs/M1-verification.md` with the same checklist shape as
  M0:
  - DoD items 1–6 each linked to evidence (test name, log line,
    file path).
  - Captured `whisper-cli --help` output as a frozen snapshot so
    a future flag rename is caught quickly.
  - JSON-shape sample committed alongside the fixture; documented
    that any whisper.cpp upstream change to this shape requires
    re-running Phase 0 verification.
- Update `docs/M0-plan.md` status snapshot table to mark M1 as
  in-progress / done.

**Done when**: `M1-verification.md` is committed with all six DoD
items ticked and links to artefacts.

## Risks (what could push us back)

- **whisper.cpp upstream renames a flag.** Mitigation: Phase 0
  flag-name lock-in; CI smoke run on the maintainer's box catches
  drift early because the integration test doesn't feature-gate.
- **JSON output shape drift.** Mitigation: shape validator runs in
  unit tests against a committed fixture; integration test parses
  the live output and asserts the same shape.
- **Big-model RAM pressure.** Worst case `ggml-large-v3.bin` ≈ 3 GiB
  resident; M1 leaves resource limits to the OS / user, but
  documents the fact in `M0-host-setup.md`. Settings UI (M7) will
  add per-profile model recommendations.
- **`flac → wav` fallback bloat.** If whisper-cli on the host
  rejects FLAC (older builds without libsndfile), we shell out to
  `flac --decode` into the per-call `TempDir`. This adds a system
  dep already required by M0 soak; do **not** introduce a
  GStreamer-based decoder.
- **Sandbox/permission model creep.** The transcribe call has to
  read the audio file and write `.txt`/`.json` next to it. M3
  daemon hardening (sysd `ProtectHome=`) will need to allow that
  directory; flagged here so M3 doesn't get blindsided.
- **whisper.cpp models not on disk.** Surface as `ModelNotFound`
  with the canonical expected path, **not** as a generic IO error.
  M7 settings UI will add the downloader; until then, the error
  message must be enough for the user to copy/paste into a
  download flow.

## Out of scope, on purpose (re-statement)

- Streaming partial transcripts during recording (M5 streaming
  backends only)
- Diarization (M5 — and only Deepgram/AssemblyAI; whisper.cpp does
  no true diarization)
- Profile-driven backend selection (M2 — IDEA.md § 5)
- Daemon orchestration of transcribe jobs / queue (M3)
- Tray indicator progress (M4)
- Cloud secret-service (M5)
- Settings UI / model downloader (M7)
- Hotkey toggle with auto-transcribe (M6)

## Definition-of-done sign-off

M1 is closed only when `docs/M1-verification.md` is committed with
all six DoD items ticked and links to artefacts (test logs, JSON
fixture, sample `.txt` + `.json`, `whisper-cli --help` snapshot,
`ZWHISPER_WHISPER_CLI` env-var smoke). Until then, M1 stays open.
