# M5 — Cloud backend (Deepgram batch): implementation plan

> Target milestone from [IDEA.md § 11](../IDEA.md#11-roadmap). Adds
> the **first remote transcriber** (`Deepgram` batch REST API) to
> the `Transcriber` trait built in M1, behind the M2 profile schema
> and the M3 D-Bus pipeline. M5 DoD verbatim from IDEA.md § 11:
> *"API key v keyring, streaming, ☁ marker v tray menu"*.
>
> **The DoD line in IDEA.md is now partially obsolete**: per the
> 2026-05-02 user lock-ins (Q1, Q2, Q3) M5 ships **batch** REST (not
> streaming), with **no keyring** integration (env var + chmod-600
> TOML only — keyring deferred indefinitely), and exposes a new
> word-level diarization shape via `TranscriptArtifacts.speakers`.
> Treat IDEA.md § 11 as superseded by this plan for M5 only; the
> roadmap row will be re-stated in `docs/M5-verification.md` after
> ship.
>
> Anchors: IDEA.md § 4 (`Transcriber` trait, backend table, API-key
> resolution order), § 5 (sink delivery — unchanged in M5), § 11
> (M5 row), § 12 (open risks). Builds on the frozen contracts of
> M2 (profile schema), M3 (`Recorder1` / `Profiles1` wire format)
> and M4 (tray menu builder + state-file writer).
>
> **Frozen wire surface (do NOT mutate in M5):**
> `crates/zwhisper-ipc/src/{recorder.rs,types.rs,error.rs}`. The
> `Profiles1` interface gains an additive method (`list_v2`) — the
> existing `list()` keeps the `(ssu)` signature word-for-word. See
> § "`Profiles1` D-Bus contract decision" for the full rationale.
>
> **Internal `zwhisper-core` surface widening (additive only):**
> `TranscribeOpts` grows a new `backend_config` enum field;
> `TranscriptArtifacts` grows `speakers: Option<Vec<SpeakerSegment>>`.
> Old call sites compile unchanged via a `Default` impl on
> `BackendConfig`.

## Status snapshot (2026-05-02)

| Area | State | Evidence |
|---|---|---|
| `Transcriber` trait + `WhisperCppLocal` impl | done | `crates/zwhisper-core/src/transcribe/mod.rs:67-77`, `transcribe/whisper_cpp.rs:169-200` |
| Backend enum already lists `Deepgram`, `AssemblyAi`, `OpenAi` | done | `crates/zwhisper-core/src/profile/schema.rs:31-50` |
| Profile validator rejects non-`WhisperCpp` backends | live (must change) | `schema.rs:191-196` raises `ProfileError::BackendUnknown` on `Deepgram` |
| `SUPPORTED_BACKENDS_M2 = &["whisper-cpp"]` | live (must change) | `crates/zwhisper-core/src/profile/error.rs:11` |
| Façade `transcribe_file` only knows `"whisper-cpp"` | live (must change) | `crates/zwhisper-core/src/transcribe/mod.rs:84-95` |
| `TranscribeOpts` carries `backend / model / language` only | live (must extend) | `transcribe/mod.rs:34-42` |
| `TranscriptArtifacts` carries no diarization data | live (must extend) | `transcribe/mod.rs:48-62` |
| Daemon dispatch: builds `TranscribeOpts` from `LifecycleHooks` | live (must extend) | `crates/zwhisperd/src/lifecycle.rs:172-177` |
| CLI dispatch: builds `TranscribeOpts` from profile or args | live (must extend) | `crates/zwhisper-cli/src/commands/transcribe.rs:28-49` |
| `Profiles1.List` wire-format `(ssu)` is sticky | done | `crates/zwhisper-ipc/src/types.rs:40-44` + signature test at `:60` |
| Tray reads `state.profiles: Vec<ProfileEntry>` and renders submenu | done | `crates/zwhisper-tray/src/tray.rs:112-121` |
| reqwest / tokio-util / zeroize workspace deps | not done | absent from `Cargo.toml:15-70` |
| `crates/zwhisper-core/src/secrets/` module | not done | does not exist |
| `~/.config/zwhisper/secrets.toml.example` | not done | does not exist |

**Verdict.** M5 is a focused two-cut change: (a) add a Deepgram
backend that fulfills the existing `Transcriber` trait without
mutating it; (b) widen two PoD types (`TranscribeOpts`,
`TranscriptArtifacts`) and one profile sub-table additively. The
M3 wire surface for recording is **untouched**. The `Profiles1`
list response gets a `list_v2()` companion so the tray can render
the cloud (☁) marker without breaking the M3-locked `(ssu)` shape.

**M5 unlocks.** M6 (hotkey toggle) does NOT depend on M5 — it
only consumes `Recorder1.StartRecording`. M7 (settings GUI) and M8
(packaging) DO depend on M5 having defined the cloud-backend
contract: M7 needs the `[transcription.deepgram]` sub-table to
build a settings form; M8's `secrets.toml.example` ships under
`/etc/zwhisper/` per packaging conventions.

## Definition of done

Each item below is a testable assertion. Items 1–12 mirror the
tech-lead's 12 DoDs verbatim in intent; the verification commands
and test names are kept stable. Items 13–18 lock in the
architectural decisions reached in this plan (TranscribeOpts shape,
Profiles1 evolution path, retry budget cap, etc.).

1. `zwhisper-cli transcribe --profile cloud-meeting <flac>` produces
   a `<flac>.txt` and `<flac>.json` end-to-end via the Deepgram
   batch API on a live key, prints `transcript ok`, exits 0. The
   shipped fixture profile is `cloud-meeting.toml` (model `nova-3`,
   `language = "auto"`, `diarize = true`).
2. With `ZWHISPER_DEEPGRAM_API_KEY` unset and no `secrets.toml`,
   the same command fails fast at startup with
   `SecretsError::NotFound { backend: "deepgram", searched: [..] }`,
   prints a self-correcting message ("set `ZWHISPER_DEEPGRAM_API_KEY`
   or create `~/.config/zwhisper/secrets.toml` with mode 0600"),
   exits non-zero, never opens a network socket. Verified by
   `tests/secrets_resolver.rs::missing_key_fails_fast` plus a
   tracing capture asserting no `reqwest::*` log line.
3. `~/.config/zwhisper/secrets.toml` with mode `0o644` is rejected
   with `SecretsError::Permissions { path, mode: 0o644, uid }`
   carrying the absolute path; mode `0o600` is accepted; mode
   `0o400` is accepted (read-only-by-user is also safe — see OQ-2).
   uid mismatch (file owned by another user) is rejected with the
   same variant. Test:
   `secrets_resolver::rejects_world_readable_toml`.
4. Profile schema accepts `transcription.backend = "deepgram"` plus
   the `[transcription.deepgram]` sub-table; rejects unknown keys
   inside the sub-table; round-trips via `serde::{Serialize,
   Deserialize}`. Test:
   `profile::schema::tests::deepgram_profile_validates`.
5. Old whisper-cpp profiles compile and load unchanged — no
   migration needed because the new sub-table is `Option<...>`
   on the `Transcription` struct. Test:
   `profile::schema::tests::whisper_profile_unchanged_after_m5`.
6. `TranscriptArtifacts.speakers` is `Some(vec![..])` for a
   Deepgram run with `diarize = true` and at least two distinct
   `speaker` ids in the response; the resulting JSON file contains
   a top-level `"speakers"` array. Test:
   `transcribe::deepgram::tests::groups_words_into_speaker_segments`.
7. `TranscriptArtifacts.speakers` is `None` for every whisper-cpp
   run; the resulting JSON file does NOT contain a `"speakers"`
   key (omitted via `serde(skip_serializing_if = "Option::is_none")`).
   Test: `transcribe::whisper_cpp::tests::artifacts_speakers_none`.
8. The tray menu prepends `☁ ` to the row label of every profile
   whose `backend != "whisper-cpp"`. The whisper-cpp profile rows
   stay unprefixed. Verified by
   `zwhisper_tray::tray::tests::cloud_marker_prepends_for_remote_backend`.
9. No log line at any tracing level (TRACE..ERROR) anywhere in
   the codebase contains the literal API-key fixture string.
   Test: `transcribe::deepgram::tests::api_key_never_logged` —
   uses `tracing_test::traced_test` to capture all subscribers
   and `assert!(!captured.contains(FIXTURE_KEY))`.
10. Network errors carry a `backend` field. `reqwest::Error::is_connect()`
    maps to `BackendNetwork { backend: "deepgram", source }`;
    `is_timeout()` maps to `BackendTimeout`; HTTP `401`/`403` to
    `BackendAuth`; HTTP `402`/`429` to `BackendQuota`; any other
    non-2xx to `BackendBadResponse { status, body_excerpt }`. Test:
    `transcribe::deepgram::tests::error_classification_table` —
    table-test with wiremock.
11. The reqwest client is built with `rustls-tls` only (no
    `native-tls`, no `default-tls`); requests to
    `http://api.deepgram.com/...` are rejected at request time
    with `BackendBadResponse { status: 0, body_excerpt: "non-https
    URL" }` — a hard assertion before we even hit DNS. Test:
    `transcribe::deepgram::tests::rejects_plaintext_url`.
12. **Zero hardcoded values** for retries, timeouts, model name,
    or endpoint. The model defaults to `"nova-3"` via `serde(default
    = "...")` and is overridable per-profile; the endpoint is a
    single `const DEEPGRAM_LISTEN_URL: &str = "..."` next to a
    deny-list assertion that the host is `api.deepgram.com`. Test:
    `transcribe::deepgram::tests::endpoint_is_constant_https`.
13. `TranscribeOpts` grows a single `pub backend_config:
    BackendConfig` field (enum with `WhisperCpp`/`Deepgram` variants);
    the existing `backend: String` field is kept for one milestone
    as a redundant routing key, marked `#[deprecated(since =
    "M5", note = "use backend_config")]` for M6 to drop. See
    § "Public API rules (M5 lock-ins)" for the rationale.
14. `Profiles1.list_v2()` returns `Vec<ProfileEntryV2>` where
    `ProfileEntryV2 = (name, description, schema_version, backend)`
    — D-Bus signature `a(ssus)`. The legacy `list()` keeps the
    M3-locked `(ssu)` shape and is still valid for clients that
    have not been re-generated. Test:
    `profiles_service::tests::list_v2_includes_backend`.
15. Total wall-clock budget for retries is bounded by
    `transcription.deepgram.retry_total_budget_s` (default 90 s).
    A connection that flaps for 5 minutes does NOT cost 5 minutes
    of Deepgram billing. Test: `transcribe::deepgram::tests::
    retry_budget_caps_total_wall_time`.
16. The reqwest client lives on a `OnceCell<reqwest::Client>` per
    `DeepgramBatch` instance and is reused across calls. Verified
    indirectly via wiremock: 100 sequential `transcribe_file`
    calls in a hot loop create at most 1 underlying TCP connection
    pool. Test:
    `transcribe::deepgram::tests::client_reused_across_calls`.
17. The FLAC body is streamed via `reqwest::Body::wrap_stream`
    over `tokio_util::io::ReaderStream` — at no point is the
    entire FLAC buffered in process memory. Verified by feeding
    a 200 MB fixture and asserting peak RSS growth < 32 MB.
    Test: `transcribe::deepgram::tests::flac_body_is_streamed` —
    gated `#[ignore]` for CI but documented in
    `docs/M5-verification.md`.
18. `docs/M5-verification.md` ticks all of the above with file:line
    evidence (test name, log line excerpt, manual command output).
    Verdict line "M5 closes …" only after all 18 are ticked.

## Out of scope (deferred to M6+)

- **Keyring / secret-service integration** — explicitly killed by
  user 2026-05-02. Not "deferred to M6" — deferred indefinitely.
  The `keyring` crate is NOT added to workspace deps; do NOT add it
  as a "future hook" or feature flag.
- **Streaming WS** — Deepgram supports a websocket streaming API.
  M5 ships batch only. Streaming would require a different
  `Transcriber` shape (`async fn transcribe_stream(&self, audio:
  impl AsyncRead) -> impl Stream<Item = TranscriptDelta>`) and is
  on the R&D queue.
- **AssemblyAI / OpenAI Whisper** — second/third remote backends.
  Same shape as Deepgram once M5 lands; deferred to a future
  milestone (no commitment).
- **Tray UI for entering API keys** — M5 surfaces a clear startup
  error message and a `secrets.toml.example` file. Settings GUI
  (M7) will own a real form for this.
- **Settings GUI / FLTK profile editor / model downloader** — M7.
- **Hotkey binding** (xdg-desktop-portal GlobalShortcuts) — M6.
- **`Recorder1` D-Bus changes** — recording wire surface is
  untouched in M5. `Profiles1` may grow `list_v2()` (additive).
- **Bit-depth / sample-rate negotiation per backend** — Deepgram
  accepts 16-bit FLAC at the project's existing 16 kHz default
  (per IDEA.md § 3). Cross-backend audio renegotiation deferred.
- **`transcript.json` schema versioning bump** — adding the optional
  `"speakers"` array is backward-compatible; no schema bump in M5.
  A versioned migration framework for the JSON sidecar is M7+.
- **Cross-cloud failover** ("if Deepgram returns 5xx, try AssemblyAI")
  — deferred indefinitely. Single-backend per profile is the
  contract.
- **Telemetry beyond `tracing`** — no Prometheus, no OpenTelemetry.
  Structured `tracing` JSON in `logs/app.log` is the single
  source of truth for backtesting (per CLAUDE.md global instruction).
- **Cost-per-minute fields on profiles / cost preview in tray** —
  IDEA.md § 12 risk; deferred. Documented as an open contract ask.

## Architecture for M5

### ASCII diagram — cloud transcription path

```
                          ┌────────────────────────────────────────┐
                          │       D-Bus session bus                │
                          │   cz.zajca.Zwhisper1                   │
                          └────────────────────────────────────────┘
                                         ▲
                                         │ Recorder1.StartRecording
                                         │ (unchanged in M5)
                                         │ Profiles1.List   (M3, (ssu))
                                         │ Profiles1.list_v2 (M5 NEW, (ssus))
                                         │
   ┌─────────────────────┐    ┌──────────┴──────────┐    ┌──────────────────┐
   │ zwhisper-cli        │    │ zwhisperd           │    │ zwhisper-tray    │
   │ commands/transcribe │    │ recorder_service.rs │    │ tray.rs          │
   │   builds            │    │ profiles_service.rs │    │  ☁ marker via    │
   │   TranscribeOpts    │    │ lifecycle.rs:172    │    │  list_v2 backend │
   │   {backend_config}  │    └──────────┬──────────┘    └──────────────────┘
   └─────────┬───────────┘               │
             │                           │
             ▼                           ▼
   ┌────────────────────────────────────────────────────────────┐
   │ zwhisper-core::transcribe::transcribe_file (façade)        │
   │   match opts.backend_config {                              │
   │     BackendConfig::WhisperCpp { .. } => WhisperCppLocal    │
   │     BackendConfig::Deepgram { cfg }  => DeepgramBatch::new │
   │   }                                                        │
   └────────────────────────────────────────────────────────────┘
                                         │
                                         ▼
   ┌────────────────────────────────────────────────────────────┐
   │ zwhisper-core::transcribe::deepgram::DeepgramBatch         │
   │  ┌──────────────────────────────────────────────────────┐  │
   │  │ 1. resolve_api_key(secrets::resolver) -> SecretString│  │
   │  │ 2. assert URL host == api.deepgram.com && scheme=https│ │
   │  │ 3. open FLAC -> tokio::fs::File                      │  │
   │  │ 4. reqwest::Body::wrap_stream(ReaderStream::new(file))│ │
   │  │ 5. POST /v1/listen?model=&language=&diarize=&...     │  │
   │  │      Authorization: Token <secret>                   │  │
   │  │      Content-Type: audio/flac                        │  │
   │  │ 6. retry on 408/429/5xx + connect, exp backoff +     │  │
   │  │      jitter, total wall-clock cap                    │  │
   │  │ 7. parse JSON -> walk words[] -> SpeakerSegments     │  │
   │  │ 8. write transcript.txt + transcript.json (with      │  │
   │  │      "speakers" array) next to audio file            │  │
   │  └──────────────────────────────────────────────────────┘  │
   └────────────────────────────────────────────────────────────┘
                                         │
                                         ▼
                       TranscriptArtifacts {
                         txt_path, json_path,
                         duration, audio_duration,
                         language, model,
                         speakers: Some(Vec<SpeakerSegment>),  ← M5
                       }
                                         │
                                         ▼
                            daemon emits TranscriptComplete
                            (M3 wire format unchanged)
```

### Public API rules (M5 lock-ins)

1. **`Transcriber` trait surface unchanged.** No methods added,
   no methods removed. Adding a streaming method now would force
   `WhisperCppLocal` to implement it as a no-op or `unreachable!()`
   — both are anti-patterns. Streaming lives on a sibling trait
   (`StreamingTranscriber`) when streaming actually ships.

2. **`TranscriptArtifacts.speakers: Option<Vec<SpeakerSegment>>`
   — additive only.** `None` for every backend that does not
   emit speaker labels (whisper-cpp today, OpenAI Whisper REST
   when it ships). `Some(empty_vec)` is reserved for "backend
   supports diarization but found one speaker only" — the JSON
   writer treats `Some(vec![])` and `None` differently: `None`
   omits the key, `Some([])` emits `"speakers": []` so a downstream
   consumer can distinguish "the backend tried" from "the backend
   doesn't support it".

3. **`TranscribeOpts` shape — DECIDED: tagged enum.**
   The tech-lead briefing left the choice between (a) extending
   `TranscribeOpts` with a backend-tagged enum field, or (b)
   adding a separate `Option<DeepgramOpts>` field. **Decision:
   (a) tagged enum, with `BackendConfig::WhisperCpp { .. }` as
   the default variant.**

   ```rust
   // crates/zwhisper-core/src/transcribe/mod.rs (M5)
   #[derive(Debug, Clone)]
   pub struct TranscribeOpts {
       pub backend: String,            // legacy routing key, kept
       pub model: String,              // shared by both backends
       pub language: String,           // shared by both backends
       pub backend_config: BackendConfig, // M5 NEW
   }

   #[derive(Debug, Clone)]
   pub enum BackendConfig {
       WhisperCpp(WhisperCppOpts),
       Deepgram(DeepgramOpts),
   }

   impl Default for BackendConfig {
       fn default() -> Self { Self::WhisperCpp(WhisperCppOpts::default()) }
   }
   ```

   **Why (a) wins over (b):**
   - Adding a third backend in M6+ (AssemblyAI) is a new variant,
     not a new `Option<AssemblyAiOpts>` field — keeps
     `TranscribeOpts` from growing N optional fields, one per
     backend.
   - Compiler-enforced exhaustiveness: the façade `match` arm
     in `transcribe_file` must handle every variant; forgetting
     a backend produces a hard error, not a silent fallthrough.
   - The redundancy between `backend: String` (routing key) and
     `BackendConfig` variant tag is a known smell. The plan keeps
     `backend: String` for one milestone with a `#[deprecated]`
     annotation, then drops it in M6 once every call site has
     been migrated. Removing it now would be a wider refactor
     and the tech-lead capped P5 at ~1 h.

   **What you give up (trade-off):** the enum is non-`Copy`, so
   `TranscribeOpts` becomes non-`Copy`. It already isn't (it owns
   `String`s), so no real cost.

4. **`Transcription` profile struct grows
   `pub deepgram: Option<DeepgramTomlConfig>`.** The flat
   `[transcription]` block stays additive; only profiles whose
   `backend = "deepgram"` need the sub-table. Validator enforces:
   if `backend == "deepgram"`, then `deepgram.is_some()`; if
   `backend == "whisper-cpp"`, `deepgram` is silently ignored
   (warn at validate-time only).

5. **Façade `transcribe_file` dispatch is the single owner of
   the backend ↔ config matching.** Daemon and CLI never
   instantiate `DeepgramBatch` directly — they always call
   `transcribe_file(&audio, &opts)`. This keeps the API-key
   resolution, retry policy, and reqwest-client lifecycle as
   internal `zwhisper-core` concerns.

6. **`Profiles1` D-Bus contract — DECIDED: additive `list_v2()`.**
   Per M3-plan § 8 the `Profiles1` wire format is sticky.
   `ProfileEntry = (name, description, schema_version)` is signed
   `(ssu)` and pinned by the test at
   `crates/zwhisper-ipc/src/types.rs:60`. Three options were
   considered:

   | Option | Wire-break? | Tray complexity | Verdict |
   |---|---|---|---|
   | (i) Extend `ProfileEntry` to `(ssus)` in-place | YES — every M3 client breaks | Low | **REJECTED** |
   | (ii) Add `Profiles1.list_v2() -> Vec<ProfileEntryV2>` | NO — additive | Tray prefers `list_v2`, falls back to `list` | **CHOSEN** |
   | (iii) Add a separate `Profiles1.GetBackend(name) -> s` method, called per profile | NO — additive | One round-trip per profile (N+1) | REJECTED — performance |

   **Lock-in.** New method:
   ```text
   list_v2() -> a(ssus)   // [(name, description, schema_version, backend)]
   ```
   where `backend` is `profile.transcription.backend.as_str()`
   (one of `"whisper-cpp"`, `"deepgram"`, `"assemblyai"`,
   `"openai"`).

   The tray's signal pump tries `list_v2()` first; on
   `zbus::Error::MethodError("org.freedesktop.DBus.Error.UnknownMethod",
   _)` it falls back to `list()` and treats every profile as
   `backend = "whisper-cpp"` (no ☁ marker rendered). Older
   daemons ↔ newer trays therefore degrade gracefully — see
   stress-test C5.

   **Risk / rollback.** `list_v2` is a strict additive surface.
   Rolling back is `-#[zbus::interface] fn list_v2`. No client
   has the right to depend on `list_v2` existing in M5 — only
   the M5+ tray uses it.

### Threading model

```
┌──── tokio runtime (CLI / daemon — both share this shape) ────┐
│                                                              │
│  Caller -> transcribe_file() -> DeepgramBatch::transcribe_file
│                                                              │
│  Per-instance OnceCell<reqwest::Client> reused across calls  │
│  reqwest is built with rustls-tls; the connection pool       │
│  lives on the surrounding tokio runtime.                     │
│                                                              │
│  No spawn_blocking. No extra OS threads. The JSON parse step │
│  runs on the calling task — Deepgram responses are bounded   │
│  (~hundreds of KB for a 30-min recording).                   │
│                                                              │
│  FLAC body streaming: tokio::fs::File + ReaderStream +       │
│  reqwest::Body::wrap_stream. Backpressure honoured by        │
│  reqwest's own send loop.                                    │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

The daemon's lifecycle task already runs `transcribe_file` on its
own tokio worker (`crates/zwhisperd/src/lifecycle.rs:177`). M5 does
NOT change that. The reqwest client lives on the same runtime —
no separate executor.

### Sink trait

Unchanged. The tray's sink dispatcher (M4) reads the transcript
file and the artifacts struct. It does NOT consume `speakers`
data — clipboard receives plain text, notification body is
unchanged. A future milestone may surface speaker-formatted
text in the clipboard ("`[Speaker 0]: hello\n[Speaker 1]: hi`")
behind a profile flag; that is **not** in M5 scope.

### Profile schema delta

Concrete TOML for the shipped fixture
`crates/zwhisper-core/profiles/cloud-meeting.toml`:

```toml
schema_version = 1
name = "cloud-meeting"
description = "Cloud transcription via Deepgram (nova-3, diarized)"

[sources]
mic = "default"
system_output = "@DEFAULT_MONITOR@"
mode = "mixed"

[recording]
codec = "flac"
sample_rate = 16000
max_duration_minutes = 120

[transcription]
backend = "deepgram"
model = "nova-3"
language = "auto"
auto = true

[transcription.deepgram]
language_detection = true
diarize = true
smart_format = true
paragraphs = true
timeout_s = 600
max_retries = 3
retry_total_budget_s = 90

[[outputs]]
type = "file"
path = "~/Recordings/{timestamp}-{profile}.flac"

[[outputs]]
type = "clipboard"

[[outputs]]
type = "notification"
```

Validator rules (added in P2):

- `backend = "deepgram"` requires `[transcription.deepgram]`.
- `backend = "whisper-cpp"` does NOT require it; if present,
  emits a `tracing::warn!` at load time but does not fail.
- `model` defaults to `"nova-3"` when omitted under
  `[transcription]` AND `backend = "deepgram"`.
- Unknown keys inside `[transcription.deepgram]` are rejected
  (`#[serde(deny_unknown_fields)]`) — typo-detection at the
  source.
- `timeout_s` must be `>= 30` and `<= 1800` (Deepgram batch
  caps requests at 30 minutes worst-case for a long file).
  `max_retries` must be `<= 10`. `retry_total_budget_s` must be
  `>= 10` (lower bound prevents accidental zero-budget config).

### `Profiles1` D-Bus contract decision

(See § "Public API rules (M5 lock-ins)" item 6 for the full
reasoning.) Concretely:

```rust
// crates/zwhisper-ipc/src/types.rs (M5 ADDED, M3 ProfileEntry kept)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ProfileEntryV2 {
    pub name: String,
    pub description: String,
    pub schema_version: u32,
    pub backend: String, // "whisper-cpp" | "deepgram" | ...
}

// signature test:
assert_eq!(ProfileEntryV2::SIGNATURE.to_string(), "(ssus)");

// crates/zwhisper-ipc/src/profiles.rs (M5 ADDED)
#[zbus::proxy(...)]
pub trait Profiles1 {
    fn list(&self) -> zbus::Result<Vec<ProfileEntry>>;       // M3
    fn list_v2(&self) -> zbus::Result<Vec<ProfileEntryV2>>;  // M5
    fn get_active(&self) -> zbus::Result<String>;            // M3
    fn set_active(&self, name: &str) -> zbus::Result<()>;    // M3
    fn reload(&self) -> zbus::Result<()>;                    // M3
}
```

The daemon's `ProfilesInterface` impl gains a sibling method that
calls `profile::listing::list_entries()` (already returns the
backend implicitly via the loaded `Profile`) and emits
`ProfileEntryV2`. The existing `list()` impl is left
**unchanged**; both methods read from disk on every call (M3
lock-in C12).

### Speaker grouping algorithm

Deepgram's response shape (relevant subset, batch with `diarize=true`):

```json
{
  "results": {
    "channels": [{
      "alternatives": [{
        "transcript": "hello world ...",
        "words": [
          {"word": "hello", "start": 0.04, "end": 0.32, "speaker": 0, "speaker_confidence": 0.91},
          {"word": "world", "start": 0.33, "end": 0.71, "speaker": 0},
          {"word": "yes",   "start": 1.10, "end": 1.30, "speaker": 1}
        ]
      }]
    }],
    "summary": { ... }
  }
}
```

Grouping pseudocode (`crates/zwhisper-core/src/transcribe/deepgram.rs`):

```text
input:  words: &[DeepgramWord]
output: Vec<SpeakerSegment>

if words.is_empty():
    return vec![]

let mut segments = Vec::new()
let mut current: Option<SpeakerSegment> = None

for w in words:
    let speaker_id = w.speaker.unwrap_or(MISSING_SPEAKER_SENTINEL)
    // ^ MISSING_SPEAKER_SENTINEL = u32::MAX. Older models / certain
    //   languages may omit `speaker`; we keep these words in their
    //   own synthetic segment instead of dropping them.

    match &mut current:
        Some(seg) if seg.speaker_id == speaker_id:
            seg.end_s = w.end
            seg.text.push(' ')
            seg.text.push_str(&w.word)
        _:
            if let Some(prev) = current.take():
                segments.push(prev)
            current = Some(SpeakerSegment {
                speaker_id,
                start_s: w.start,
                end_s: w.end,
                text: w.word.clone(),
            })

if let Some(last) = current:
    segments.push(last)

return segments
```

**Edge cases (covered by table tests):**

| Case | Expected output |
|---|---|
| `words = []` | `Vec::new()` (an `Option<Vec<_>>::Some` empty vec at the call site for diarize-on, `None` for diarize-off) |
| Single word | One-segment vec, `start_s == w.start`, `end_s == w.end` |
| All same speaker | Single segment spanning all words |
| Every word missing `speaker` field | Single segment with `speaker_id = u32::MAX`; `tracing::warn!` once per response noting the model returned no speaker labels |
| Some words missing `speaker` field | Synthetic segments grouped by `MISSING_SPEAKER_SENTINEL` interleaved with real segments — fidelity preserved, never dropped |
| Speaker ids non-monotonic (`0, 1, 0, 2, 0`) | Five segments, in input order |

The text join uses a single space — no smart punctuation. The
text in `SpeakerSegment` is the raw concatenation of
`DeepgramWord.word`. The `transcript.json` writer emits
`speaker_confidence` only if every word in the segment carries
it; otherwise the field is omitted. Punctuation in the
transcribed text is already handled server-side via
`smart_format=true`.

### API key flow diagram

```
┌─────────────────────────────────────────────────────────────┐
│  resolve_api_key(backend = "deepgram")                      │
│                                                             │
│   1. Read env var ZWHISPER_DEEPGRAM_API_KEY                 │
│      ─ found, non-empty  ─►  SecretString::new(value)       │
│                              (zeroize on drop, redacted     │
│                              in Debug/Display)              │
│      ─ unset / empty     ─►  fall through to step 2         │
│                                                             │
│   2. Resolve TOML path: ~/.config/zwhisper/secrets.toml     │
│                          (XDG_CONFIG_HOME aware)            │
│      ─ file does not exist                                  │
│                ─►  Err(SecretsError::NotFound {             │
│                       backend, searched: [env, toml] })     │
│                                                             │
│   3. Open the path with O_NOFOLLOW (refuse symlinks),       │
│      then fstat() the open fd:                              │
│                                                             │
│      ─ st_uid != geteuid()                                  │
│                ─►  Err(SecretsError::Permissions {          │
│                       path, mode, uid })                    │
│      ─ st_mode & 0o777 ∉ {0o600, 0o400}                     │
│                ─►  Err(SecretsError::Permissions { ... })   │
│      ─ ok      ─►  read content from the SAME fd            │
│                    (no second open — closes TOCTOU window;  │
│                    see C3)                                  │
│                                                             │
│   4. Parse TOML, look up `deepgram.api_key` (string)        │
│      ─ missing key                                          │
│                ─►  Err(SecretsError::NotFound {             │
│                       backend, searched: [env, toml] })     │
│      ─ found  ─►  SecretString::new(value)                  │
│                                                             │
│  Header construction in DeepgramBatch::transcribe_file:     │
│                                                             │
│   header_value = HeaderValue::from_str(&format!(            │
│                    "Token {}", secret.expose()))            │
│                  .map_err(|_| BackendBadResponse { ... })   │
│   header_value.set_sensitive(true)                          │
│   ─► reqwest scrubs this header from any error formatting   │
│                                                             │
│  SecretString lifetime ends at the end of transcribe_file;  │
│  Drop impl calls zeroize on the underlying buffer.          │
└─────────────────────────────────────────────────────────────┘
```

**Failure surface table** (every branch maps to a typed error
that the CLI's exit-code mapper translates to a clear stderr
message — no panics, no `expect`):

| Step failure | Variant | Exit code (CLI) | Message hint |
|---|---|---|---|
| 1 + 2 both miss | `SecretsError::NotFound` | 64 | "Set ZWHISPER_DEEPGRAM_API_KEY or create secrets.toml" |
| 3 mode/uid wrong | `SecretsError::Permissions` | 65 | "secrets.toml must be 0600 or 0400 and owned by you" |
| 3 file is symlink | `SecretsError::Permissions` (re-uses variant; field `mode` is `0` sentinel) | 65 | "secrets.toml must not be a symlink" |
| 4 invalid TOML | `SecretsError::Parse` | 66 | "secrets.toml parse error: {source}" |
| 4 missing key | `SecretsError::NotFound` | 64 | (same as 1+2) |

## Stress-test corrections

Three binding amendments distilled from challenging the tech-lead's
phase plan against the locked-in user decisions and the existing
codebase. C1–C4 are mandatory; severity column maps to which
milestone they ship in.

### C1 (M5). Word-level diarization can return `speaker = null` on older models — defensive sentinel, not panic

**Trigger.** Deepgram's `nova-3` consistently emits `speaker: u32`
on every word when `diarize=true`. But (a) earlier models
(`nova-2`, `enhanced`, `base`) intermittently omit the field on
silence-adjacent words, and (b) some non-English language pipelines
return `speaker_confidence` but no integer `speaker`. The naive
`#[serde(deserialize_with = "required_u32")]` deserializer would
panic the whole transcription run on a single missing field —
turning a recoverable response into a hard failure.

**Lock-in.** `DeepgramWord.speaker: Option<u32>` (NOT plain `u32`).
The grouping algorithm treats `None` as a synthetic
`MISSING_SPEAKER_SENTINEL = u32::MAX` segment. A response with
≥ 50 % missing-speaker words emits a single
`tracing::warn!(backend = "deepgram", missing_pct = N, model =
%opts.model, "diarization incomplete; consider model upgrade")`
log line — no error, transcript still ships.

**Test.** Wiremock fixture
`tests/fixtures/deepgram-partial-speakers.json` returning 5 words
where words 2 + 4 omit `speaker`. Assert: `transcribe_file` returns
`Ok`; `artifacts.speakers.unwrap().len() == 4` (three real
segments + one synthetic).

### C2 (M5). reqwest client SHOULD be a per-`DeepgramBatch` instance, NOT a global / per-call

**Trigger.** Naive code paths for both extremes are bad:

- Per-call `reqwest::Client::new()`: re-builds the rustls
  config + connection pool every call. On a daemon transcribing
  10 short clips in 60 s, that is 10 fresh TCP+TLS handshakes
  to `api.deepgram.com`, ~200 ms of latency burnt per call.
- Global static `OnceCell<Client>`: bad for testing (wiremock
  needs a fresh client with a different base URL); also leaks
  the connection pool across the whole daemon lifetime, which
  on long-running daemons accumulates idle keepalive pings.

**Lock-in.** `DeepgramBatch` owns
`client: OnceCell<reqwest::Client>`, lazily initialised on first
`transcribe_file` call. The façade `transcribe_file` constructs
one `DeepgramBatch` per call (matches the existing whisper-cpp
shape) BUT the caller may also construct it once and reuse —
the daemon's lifecycle task does the latter via a per-daemon
`OnceCell<DeepgramBatch>` introduced in P5.

**Trade-off.** The CLI single-shot path (`zwhisper-cli
transcribe`) builds a fresh client per process — fine, the
process exits anyway. The daemon path reuses one client across
all sessions — saves the TLS handshake, but means a TLS
mid-life rotation on Deepgram's side requires a daemon restart.
That is acceptable: Deepgram rotates keys via header rolling, not
via TLS cert rolling at the socket level.

**Test.** `tests::client_reused_across_calls` (DoD #16) — wiremock
records the number of distinct TCP connections; with 100 sequential
calls on the same `DeepgramBatch`, the count is `<= 4` (matching
reqwest's default pool size).

### C3 (M5). secrets.toml stat-then-open is a TOCTOU race — open-then-fstat

**Trigger.** The tech-lead briefing mentions `libc::stat` returning
mode + uid before opening the file. Between `stat` and `open`, a
malicious local process can swap the file for a symlink pointing
elsewhere (`/etc/shadow`, `~/.ssh/id_rsa`, …). On a single-user
desktop the threat model is low, but the fix is one syscall and the
CLAUDE.md global instruction "Security > Quality > Simplicity >
Time" is binding.

**Lock-in.**

```rust
// crates/zwhisper-core/src/secrets/resolver.rs
let fd = std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
    .open(&path)
    .map_err(|source| match source.raw_os_error() {
        Some(libc::ELOOP) => SecretsError::Permissions {
            path: path.clone(), mode: 0, uid: 0,
        }, // symlink detected
        _ => SecretsError::Io { path: path.clone(), source },
    })?;

let metadata = fd.metadata()?; // fstat under the hood
let mode = metadata.permissions().mode() & 0o777;
let uid = metadata.uid();
if uid != unsafe { libc::geteuid() } {
    return Err(SecretsError::Permissions { path, mode, uid });
}
if !matches!(mode, 0o600 | 0o400) {
    return Err(SecretsError::Permissions { path, mode, uid });
}
let mut buf = String::new();
(&fd).read_to_string(&mut buf)?;
```

`O_NOFOLLOW` rejects symlinks at open-time — no race window.
The metadata read goes through `fd`, never re-resolving the path.

**Test.** `secrets_resolver::rejects_symlink_target` — create
`secrets.toml` as a symlink to a tempfile with mode 0600; assert
`SecretsError::Permissions` with `mode = 0`.

### C4 (M5). Retry budget MUST be wall-clock-bounded, not just attempt-bounded

**Trigger.** The tech-lead's P4 says "exponential backoff with
jitter, max-retries from config". With `max_retries = 5` and
exponential 1, 2, 4, 8, 16 s the worst-case sleep is 31 s. With
network flapping (each attempt itself takes the request timeout
of 600 s) the same 5 attempts can burn **51 minutes** of
wall-clock time, all of which the user is paying for if any of
them succeed mid-flap. That is unacceptable for a CLI tool and
unacceptable for a daemon (which would block the lifecycle task
for 51 minutes, preventing new recordings).

**Lock-in.** Two independent caps:

1. `max_retries: u32` (default 3) — attempt count cap.
2. `retry_total_budget_s: u64` (default 90) — wall-clock cap
   across all attempts. The `Instant::now()` at the start of
   `DeepgramBatch::transcribe_file` is the budget origin; every
   retry checks `elapsed >= budget` and converts to
   `BackendTimeout { backend: "deepgram", elapsed }` at that
   point — even if `max_retries` has not been exhausted.

Whichever fires first wins. The default 90 s is calibrated for
batch (where the request itself takes at most 30–60 s for a
30-minute audio file): three attempts of ~30 s each.

**Test.** `transcribe::deepgram::tests::retry_budget_caps_total_wall_time`
— wiremock returns `503` indefinitely; assert: `transcribe_file`
errors after 90 ± 10 s with `BackendTimeout`, and the wiremock
hit count is whatever fits into 90 s, not the full
`max_retries`.

### Summary of binding amendments

| ID | Area | Lock-in |
|---|---|---|
| C1 | Diarization parsing | `speaker: Option<u32>`, sentinel for missing, no panic |
| C2 | reqwest client | per-`DeepgramBatch` `OnceCell`, daemon caches one |
| C3 | secrets.toml race | `O_NOFOLLOW` open-then-fstat, no second resolve |
| C4 | Retry budget | wall-clock-bounded in addition to attempt-bounded |

## Phased breakdown

Phase 1 lands first; Phase 8 ships M5. Each phase is one PR-sized
commit set. The tech-lead's 8-phase decomposition is preserved
verbatim in scope; the per-phase "Files touched" line numbers below
were re-checked against HEAD.

### Phase 1 — Workspace deps + secrets resolver (~3 h)

**Goal.** Add the network and secrets crates to the workspace;
implement `SecretString` + `secrets::resolver` so P4 has a
non-mock dependency.

**Files touched.**
- `Cargo.toml` (workspace) — add deps:
  ```toml
  reqwest = { version = "0.12", default-features = false,
              features = ["rustls-tls", "json", "stream"] }
  tokio-util = { version = "0.7", features = ["io"] }
  zeroize = { version = "1.8", features = ["zeroize_derive"] }
  url = "2.5"
  ```
  No `keyring` crate. `libc` is already at workspace level
  (`Cargo.toml:44`).
- `crates/zwhisper-core/src/lib.rs` — add `pub mod secrets;`.
- `crates/zwhisper-core/src/secrets/mod.rs` (new) — public
  re-exports + `SecretString` newtype with manual `Debug` impl
  emitting `"<redacted>"`, `Drop` impl calling `zeroize`,
  `expose() -> &str` accessor.
- `crates/zwhisper-core/src/secrets/resolver.rs` (new) — three
  free functions: `resolve_api_key(backend: &str) ->
  Result<SecretString, SecretsError>`, plus the open-then-fstat
  path (C3), plus `secrets_toml_path() -> PathBuf` honouring
  `XDG_CONFIG_HOME` via the existing `dirs` workspace dep.
- `crates/zwhisper-core/src/secrets/error.rs` (new) —
  `SecretsError` enum: `NotFound { backend, searched }`,
  `Permissions { path, mode, uid }`, `Io { path, source }`,
  `Parse { path, source }`.
- `crates/zwhisper-core/tests/secrets_resolver.rs` (new) —
  integration tests using `tempfile::tempdir()` and
  `std::os::unix::fs::PermissionsExt` to set / change file modes.

**Interface delta.**
```rust
// crates/zwhisper-core/src/secrets/mod.rs
pub struct SecretString(Vec<u8>);
impl SecretString {
    pub fn new(s: impl Into<Vec<u8>>) -> Self;
    pub fn expose(&self) -> &str; // returns &str view
}
impl Debug for SecretString { /* "<redacted>" */ }
impl Drop for SecretString { /* zeroize() */ }

pub fn resolve_api_key(backend: &str) -> Result<SecretString, SecretsError>;
```

**Tests.**
- `secret_string_debug_redacts` — `format!("{:?}", s)` does NOT
  contain the secret value.
- `secret_string_drop_zeroizes` — uses `unsafe` raw-pointer
  inspection inside an `#[allow(unsafe_code)]` test-only block to
  confirm the buffer is zeroed after drop.
- `resolve_api_key_env_wins` — sets
  `ZWHISPER_DEEPGRAM_API_KEY=foo`, asserts the value is `foo`,
  even with a valid TOML on disk.
- `resolve_api_key_toml_fallback` — env unset, TOML mode 0600,
  contains `[deepgram] api_key = "bar"`, asserts value `bar`.
- `resolve_api_key_missing_fails_fast` — env unset, no TOML,
  asserts `SecretsError::NotFound`.
- `rejects_world_readable_toml` — mode 0644, asserts
  `SecretsError::Permissions { mode: 0o644, .. }`.
- `accepts_mode_400` — mode 0400, asserts ok.
- `rejects_symlink_target` — C3 verification.
- `rejects_uid_mismatch` — skipped on CI (requires root); manual
  step in `docs/M5-verification.md`.

**Estimate.** 3 h. The C3 fix adds ~30 minutes vs the briefing.

### Phase 2 — Profile schema widening (~2 h)

**Goal.** Replace `SUPPORTED_BACKENDS_M2` with a wider list that
includes `deepgram`; add the `[transcription.deepgram]` sub-table
on the `Transcription` struct; ship the fixture profile.

**Files touched.**
- `crates/zwhisper-core/src/profile/error.rs:11` — rename
  `SUPPORTED_BACKENDS_M2` to `SUPPORTED_BACKENDS_M5` (or, less
  invasively: keep the old name as a re-export and add
  `SUPPORTED_BACKENDS_M5 = &["whisper-cpp", "deepgram"]`).
  Decision: introduce `SUPPORTED_BACKENDS_M5` and `#[deprecated]`
  the old one; the symbol is `pub` and any external consumer (M5
  doesn't have one) sees the migration.
- `crates/zwhisper-core/src/profile/schema.rs:75-81` — extend
  `Transcription` with `pub deepgram: Option<DeepgramTomlConfig>`
  (defaults via `#[serde(default)]`).
- `crates/zwhisper-core/src/profile/schema.rs:191-196` — replace
  the `matches!(backend, Backend::WhisperCpp)` guard with a
  positive list check against `SUPPORTED_BACKENDS_M5`. Add
  cross-field validation: `Backend::Deepgram` requires
  `deepgram.is_some()`.
- `crates/zwhisper-core/src/profile/schema.rs` — add new struct:
  ```rust
  #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(deny_unknown_fields)]
  pub struct DeepgramTomlConfig {
      #[serde(default = "default_language_detection")] pub language_detection: bool,
      #[serde(default = "default_diarize")]            pub diarize: bool,
      #[serde(default = "default_smart_format")]       pub smart_format: bool,
      #[serde(default = "default_paragraphs")]         pub paragraphs: bool,
      #[serde(default = "default_timeout_s")]          pub timeout_s: u64,
      #[serde(default = "default_max_retries")]        pub max_retries: u32,
      #[serde(default = "default_retry_budget_s")]     pub retry_total_budget_s: u64,
      #[serde(default)]                                pub tier: Option<String>,
  }
  ```
  Defaults are private fns returning the IDEA-aligned values
  (90 s budget, 600 s timeout, etc.). Zero hardcoded values inside
  call sites — every constant lives in this struct's defaults.
- `crates/zwhisper-core/profiles/cloud-meeting.toml` (new) — the
  fixture profile shown in § "Profile schema delta".
- `crates/zwhisper-core/src/profile/listing.rs` — update the
  embedded profile list to include `cloud-meeting.toml`.

**Interface delta.** Backend enum is unchanged (it already lists
`Deepgram`, `AssemblyAi`, `OpenAi`). Validator behavior is the
only behavioural change.

**Tests.**
- `deepgram_profile_validates` — load `cloud-meeting.toml`,
  assert `Ok(_)` and `transcription.deepgram.is_some()`.
- `whisper_profile_unchanged_after_m5` — load the existing
  `default.toml`, assert `Ok(_)` and `transcription.deepgram.is_none()`.
- `deepgram_profile_without_subtable_rejected` — fixture profile
  with `backend = "deepgram"` and no `[transcription.deepgram]`,
  assert `ProfileError::Validation { .. }` mentioning the missing
  sub-table.
- `deepgram_subtable_rejects_unknown_keys` — fixture profile with
  `[transcription.deepgram] foobar = 1`, assert
  `ProfileError::Parse { .. }`.
- `assemblyai_still_rejected_in_m5` — ensures M5 only widens to
  `deepgram`, not the whole `Backend` enum.

**Estimate.** 2 h. Most of the work is fixture + test scaffolding;
the validator change is ~10 lines.

### Phase 3 — `TranscriptArtifacts` widening + `SpeakerSegment` (~1 h)

**Goal.** Additive changes to the result type and the JSON
sidecar writer.

**Files touched.**
- `crates/zwhisper-core/src/transcribe/mod.rs:48-62` — append
  `pub speakers: Option<Vec<SpeakerSegment>>` to
  `TranscriptArtifacts`.
- `crates/zwhisper-core/src/transcribe/mod.rs` — add
  `pub mod speakers;`.
- `crates/zwhisper-core/src/transcribe/speakers.rs` (new):
  ```rust
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub struct SpeakerSegment {
      pub speaker_id: u32,
      pub start_s: f64,
      pub end_s: f64,
      pub text: String,
  }
  ```
- `crates/zwhisper-core/src/transcribe/whisper_cpp.rs:412` —
  add `speakers: None` to the `TranscriptArtifacts` constructor.
- `crates/zwhisper-core/src/transcribe/whisper_cpp.rs` — find
  every test that constructs a `TranscriptArtifacts` literal and
  add `speakers: None`.
- JSON sidecar writer — locate the codepath that serialises the
  whisper-cpp JSON and either (a) append `"speakers"` when
  `Some`, or (b) defer the append to the Deepgram path only and
  document that the whisper-cpp JSON file is verbatim from the
  binary. Decision (b): the M1 contract is "we move whisper-cpp's
  `transcript.json` next to the audio file as-is". Adding a
  `speakers` key would mutate that file and break round-trip
  expectations. The Deepgram path writes its OWN JSON file (via a
  new `transcribe::json_writer::write_v1(&artifacts) -> Result`
  helper) and the whisper-cpp path stays unchanged.

**Interface delta.**
```rust
pub struct TranscriptArtifacts {
    pub txt_path: PathBuf,
    pub json_path: PathBuf,
    pub duration: Duration,
    pub audio_duration: Duration,
    pub language: String,
    pub model: String,
    pub speakers: Option<Vec<SpeakerSegment>>, // M5 NEW
}

pub struct SpeakerSegment {
    pub speaker_id: u32,
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
}
```

**Tests.**
- `speaker_segment_roundtrip` — serde round-trip through
  `serde_json` for a fixture vec.
- `artifacts_speakers_none_serializes_without_key` — JSON of a
  `TranscriptArtifacts { speakers: None, .. }` does not contain
  `"speakers"`.
- `artifacts_speakers_some_empty_serializes_as_empty_array` — JSON
  of `Some(vec![])` contains `"speakers": []`.
- (whisper-cpp) `artifacts_speakers_none` — running the M1
  whisper-cpp pipeline yields `speakers: None`.

**Estimate.** 1 h.

### Phase 4 — `DeepgramBatch` backend (~5 h)

**Goal.** The actual cloud transcriber. This is the largest phase
and the one with the highest test density.

**Files touched.**
- `crates/zwhisper-core/src/transcribe/mod.rs` — add
  `pub(crate) mod deepgram;`.
- `crates/zwhisper-core/src/transcribe/deepgram.rs` (new):
  ```rust
  pub struct DeepgramOpts {
      pub config: DeepgramTomlConfig, // mirrored from profile
  }

  pub struct DeepgramBatch {
      api_key: SecretString,
      client: OnceCell<reqwest::Client>,
      endpoint: &'static str, // const DEEPGRAM_LISTEN_URL
  }

  impl DeepgramBatch {
      pub fn new() -> Result<Self, TranscribeError> { /* resolves key via secrets::resolver */ }
  }

  #[async_trait]
  impl Transcriber for DeepgramBatch {
      fn id(&self) -> &'static str { "deepgram" }
      fn capabilities(&self) -> Capabilities {
          Capabilities {
              streaming: false,
              true_diarization: true,
              languages: vec!["auto"], // Deepgram supports many; "auto" suffices at trait level
          }
      }
      async fn transcribe_file(&self, audio: &Path, opts: &TranscribeOpts)
          -> Result<TranscriptArtifacts, TranscribeError>;
  }
  ```
- `crates/zwhisper-core/src/transcribe/error.rs` — extend
  `TranscribeError` with the M5 variants:
  ```rust
  BackendNetwork  { backend: &'static str, source: reqwest::Error }
  BackendAuth     { backend: &'static str, status: u16 }
  BackendQuota    { backend: &'static str, status: u16 }
  BackendTimeout  { backend: &'static str, elapsed: Duration }
  BackendBadResponse { backend: &'static str, status: u16, body_excerpt: String }
  Secrets         { #[from] source: SecretsError }
  ```
  All variants carry `backend: &'static str` so DoD #10 is met.
- `crates/zwhisper-core/src/transcribe/mod.rs:84-95` — façade
  `transcribe_file` matches `opts.backend_config` instead of
  `opts.backend`. The legacy `match opts.backend.as_str()` arm is
  kept temporarily for binary back-compat with any caller still
  passing the old shape, but the new arm is the source of truth.
- `crates/zwhisper-core/tests/deepgram_backend.rs` (new) —
  wiremock-driven integration tests.
- `Cargo.toml` workspace deps — add `wiremock = "0.6"` to dev-deps
  (workspace level) for use in `crates/zwhisper-core/tests/`.

**Interface delta.**
```rust
// transcribe/mod.rs
pub struct TranscribeOpts {
    pub backend: String,
    pub model: String,
    pub language: String,
    pub backend_config: BackendConfig, // M5 NEW
}

pub enum BackendConfig {
    WhisperCpp(WhisperCppOpts),
    Deepgram(DeepgramOpts),
}

pub struct WhisperCppOpts; // empty for M5; future fields go here
pub struct DeepgramOpts { pub config: DeepgramTomlConfig }
```

**Tests.**
- `groups_words_into_speaker_segments` — wiremock returns a
  fixture response with 6 words across 2 speakers; assert 2
  segments with correct `start_s`/`end_s`/`text`.
- `single_speaker_yields_single_segment` — DoD #6 edge case.
- `empty_words_yields_some_empty_vec` — diarize=true, audio
  empty: assert `speakers == Some(vec![])` (not `None`).
- `partial_speakers_uses_sentinel` — C1 verification.
- `error_classification_table` — table-driven with rows for
  401, 402, 408, 413, 429, 500, 502, 503, connect-error,
  timeout. Each row asserts the correct `TranscribeError` variant.
- `retries_408_429_5xx_only` — wiremock returns 408 then 200;
  `transcribe_file` succeeds. Wiremock returns 401 immediately;
  `transcribe_file` fails on first attempt (no retry).
- `retry_budget_caps_total_wall_time` — C4 verification.
- `client_reused_across_calls` — C2 verification (via
  wiremock's `Mock::register` count).
- `api_key_never_logged` — DoD #9. Uses
  `tracing_test::traced_test`.
- `rejects_plaintext_url` — DoD #11. Build a `DeepgramBatch`
  with an `http://...` endpoint via a private test-only ctor;
  assert `BackendBadResponse`.
- `endpoint_is_constant_https` — `assert_eq!(DEEPGRAM_LISTEN_URL,
  "https://api.deepgram.com/v1/listen")`.
- `flac_body_is_streamed` — `#[ignore]`-gated. Feeds a 200 MB
  fixture through wiremock; asserts peak RSS growth via
  `procfs::process::Process::statm` < 32 MB.
- `query_params_are_correct` — wiremock matches against
  `model=nova-3&detect_language=true&diarize=true&smart_format=true&paragraphs=true`
  for a profile with `language = "auto"`.
- `language_explicit_omits_detect_flag` — profile with `language
  = "cs"`: query is `language=cs` and **no** `detect_language`.

**Estimate.** 5 h. The retry+timeout state machine and the
wiremock fixtures are most of the time.

### Phase 5 — Wire dispatch (CLI + daemon) (~1.5 h)

**Goal.** Thread `BackendConfig::Deepgram { .. }` through the
CLI and daemon so a user-facing recording produces a Deepgram
transcript end-to-end.

**Files touched.**
- `crates/zwhisper-core/src/transcribe/mod.rs:84-95` — the
  façade match arm:
  ```rust
  match &opts.backend_config {
      BackendConfig::WhisperCpp(_) => {
          let backend = whisper_cpp::WhisperCppLocal::new();
          backend.transcribe_file(audio, opts).await
      }
      BackendConfig::Deepgram(_) => {
          let backend = deepgram::DeepgramBatch::new()?; // resolves key
          backend.transcribe_file(audio, opts).await
      }
  }
  ```
  `BackendUnknown` is no longer reachable from the façade (the
  enum is exhaustive); the variant is kept for the
  legacy `backend: String` translation step at `TranscribeOpts`
  construction time.
- `crates/zwhisperd/src/lifecycle.rs:58-65, :172-177` — extend
  `LifecycleHooks` with `pub(crate) transcribe_backend_config:
  BackendConfig` and pass it into the `TranscribeOpts` constructor.
  `LifecycleHooks` is built in `crates/zwhisperd/src/recorder_service.rs`
  from `Profile::transcription`; map
  `(backend, deepgram)` → `BackendConfig` via a tiny helper
  `BackendConfig::from_profile(&profile.transcription)`.
- `crates/zwhisper-cli/src/commands/transcribe.rs:28-49` —
  extend both branches of the `if let Some(name) = &args.profile`
  match to populate `backend_config` via the same helper.
  CLI args path (`args.backend`, `args.model`, `args.language`)
  defaults to `BackendConfig::WhisperCpp(_)`; passing
  `--backend deepgram` requires `--profile <name>` (no flat-arg
  Deepgram config — the surface area of `[transcription.deepgram]`
  is too wide for clap flags).
- `crates/zwhisper-cli/src/commands/transcribe.rs` — add an early
  `Result` propagation for `SecretsError`-via-`TranscribeError`
  to print a concrete startup message and exit before
  `transcribe_file` is called.

**Interface delta.**
```rust
// new in profile/schema.rs (or transcribe/mod.rs — picked: schema.rs
// because it's the schema-side adaptor):
impl BackendConfig {
    pub fn from_profile(t: &Transcription) -> Result<Self, ProfileError>;
}
```

**Tests.**
- `lifecycle::tests::deepgram_dispatch_threads_config` — fake
  profile with `backend = "deepgram"`, no real network: asserts
  `BackendConfig::Deepgram` is what the lifecycle constructs.
- `cli::transcribe::tests::profile_overrides_flat_args` — when
  `--profile cloud-meeting` is set, `--backend whisper-cpp` flag
  is ignored and the profile's deepgram config wins.
- End-to-end: `tests/end_to_end_deepgram.rs` (new, gated
  `#[cfg(feature = "live-deepgram")]` — requires real key,
  documented in `docs/M5-verification.md`).

**Estimate.** 1.5 h.

### Phase 6 — Tray ☁ marker via `Profiles1.list_v2()` (~1.5 h)

**Goal.** Tray menu shows `☁ ` prefix on profile rows whose
backend is not `whisper-cpp`. Implements the M5 evolution path
for `Profiles1` decided in § "Public API rules (M5 lock-ins)"
item 6.

**Files touched.**
- `crates/zwhisper-ipc/src/types.rs:40-44` — add
  `pub struct ProfileEntryV2 { name, description, schema_version,
  backend }`. Signature test:
  `assert_eq!(ProfileEntryV2::SIGNATURE.to_string(), "(ssus)");`.
- `crates/zwhisper-ipc/src/profiles.rs:43` — add
  `fn list_v2(&self) -> zbus::Result<Vec<ProfileEntryV2>>;` to
  the proxy trait.
- `crates/zwhisperd/src/profiles_service.rs` — add the server-side
  impl. Same disk-read-on-every-call pattern as `list()`.
- `crates/zwhisper-tray/src/state.rs:226` — extend
  `state.profiles` to a new `Vec<TrayProfileEntry>` carrying
  `backend: String`.
- `crates/zwhisper-tray/src/pump.rs` — refresh path: call
  `list_v2()` first; on `MethodError` with `UnknownMethod`, fall
  back to `list()` and synthesise `backend = "whisper-cpp"` for
  every entry. Log once at INFO when the fallback fires.
- `crates/zwhisper-tray/src/tray.rs:115-119` — extend
  `ProfileMenuEntry` with `cloud_marker: bool`, computed as
  `entry.backend != "whisper-cpp"`. The label rendered in the
  menu is `if cloud_marker { format!("☁ {name}") } else { name
  }`.

**Tests.**
- `profiles_service::tests::list_v2_includes_backend` — server
  returns `[ProfileEntryV2 { backend: "whisper-cpp", ... },
  ProfileEntryV2 { backend: "deepgram", ... }]` for the two
  fixtures.
- `profile_entry_v2_signature_is_ssus` — wire-format pin.
- `tray::tests::cloud_marker_prepends_for_remote_backend` —
  pure-data test on `menu_flags_for(state)` with two profile
  entries; assert the deepgram one is rendered with `☁ `.
- `tray::tests::list_v2_unknown_method_falls_back` — fake
  proxy returns `MethodError("UnknownMethod", _)`; assert the
  pump falls through to `list()` and the menu shows no
  cloud markers.

**Estimate.** 1.5 h.

### Phase 7 — Logging-redaction audit (~1 h)

**Goal.** Verification phase, not implementation. Confirms that
no derived `Debug` impl, no `Display` impl, no `tracing::*!`
call site, anywhere in the codebase, formats a `SecretString`
or the raw API key.

**Files touched.**
- `crates/zwhisper-core/src/secrets/mod.rs` — manual `Debug`
  impl already in P1; this phase just adds the test.
- `crates/zwhisper-core/src/transcribe/deepgram.rs` — pass an
  audit: every `tracing::info!(...)` / `error!(...)` / `warn!`
  in this file must NOT pass `self.api_key` or any string
  derived from it. The grep is:
  ```
  rg -n 'api_key|deepgram_key|secret' crates/zwhisper-core/src/transcribe/deepgram.rs
  ```
  expected: zero hits inside `tracing::*!` macros.
- `crates/zwhisper-core/tests/log_redaction.rs` (new) — a single
  integration test using `tracing-test` that runs the entire
  `transcribe_file` happy-path against wiremock with a fixture
  key `"DG-TEST-KEY-XYZZY"` and asserts that the captured log
  output never contains the substring `"DG-TEST-KEY-XYZZY"`.

**Tests.**
- `log_redaction::api_key_never_logged_e2e` — DoD #9.
- `secrets::tests::secret_string_debug_redacts` — already in P1.
- Audit script note in `docs/M5-verification.md` listing the
  exact `rg` invocations and expected output.

**Estimate.** 1 h.

### Phase 8 — Docs + IDEA.md update + verification (~2 h)

**Goal.** Ship-ready milestone.

**Files touched.**
- `IDEA.md:578` — strike through "API key v keyring, streaming"
  and reference the M5-plan.md decisions verbatim (no mutation
  of historical text — append a "**M5 actually shipped:**" line
  next to the row).
- `IDEA.md:243-251` (§ 4 API key resolution) — replace the three
  bullet points with a "M5 lock-in" admonition: keyring removed,
  order is `env -> ~/.config/zwhisper/secrets.toml (chmod 600 or
  400, uid check, O_NOFOLLOW)`.
- `README.md` — add an M5 section: cloud backend setup, env-var
  usage, `secrets.toml` setup instructions including `chmod
  600`, link to `secrets.toml.example`.
- `secrets.toml.example` (new, repo root) — `[deepgram] api_key =
  "<paste-your-key-here>"`. Comment block at top: "MUST be saved
  to `~/.config/zwhisper/secrets.toml` with mode 0600 (`chmod
  600 ~/.config/zwhisper/secrets.toml`)".
- `docs/M5-verification.md` (new) — walks all 18 DoD items with
  file:line evidence (test name, log line, manual command output,
  cost note: live key test was N seconds at $0.0077/min).
- `Cargo.lock` — committed.

**Tests.** All `cargo build --workspace`, `cargo test --workspace`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo fmt --check` clean.

**Definition of done for the milestone.** All 18 DoD items
ticked in `docs/M5-verification.md`; product-engineer issues
READY verdict.

**Estimate.** 2 h.

**Total.** ~17 h, matches the tech-lead's ~16–17 h envelope.

## Risks / open questions

### 1. (M5+, severity high) Deepgram API contract drift

**Risk.** Deepgram's response shape is documented but the
`words[].speaker` field is not part of any formally versioned
API surface. A future Deepgram dashboard change ("speaker_id"
instead of "speaker") would break the parser.

**Mitigation.** Every field we deserialise is wrapped in
`#[serde(default)]` or `Option<_>` per the researcher's
"Real-world footguns" note. The `transcribe::deepgram::tests::
groups_words_into_speaker_segments` test runs against a checked-in
JSON fixture, NOT against a live response — schema drift surfaces
in the live e2e test (gated `#[cfg(feature = "live-deepgram")]`),
not in CI noise.

**Open question.** Should the parser tolerate AND log the
emergence of new top-level fields in `results.channels[0].alternatives[0]`?
Default: yes (`#[serde(deny_unknown_fields)]` is NOT set on the
response struct). Alternative: deny + emit `BackendBadResponse`.
Recommendation: tolerate, log at DEBUG. Confirm with user.

### 2. (M5, severity medium) `chmod 600 OR 0400` — should `0o400` be allowed?

**Question for user.** The tech-lead's briefing says "chmod 600
+ uid check". `0o400` (read-only-by-user, no write) is *also*
safe and is a common hardened-config-file mode. The plan locks in
`mode ∈ {0o600, 0o400}`. Confirm: should `0o400` be accepted, or
should we be strictly `0o600` only?

**Default if no answer.** Accept both. Rationale: `0o400` is
strictly more locked-down than `0o600`; rejecting it would surprise
users who run their own hardening scripts.

### 3. (M5+, severity medium) Daemon-wide single `DeepgramBatch` vs per-session

**Risk.** Per § C2 the daemon caches one `DeepgramBatch` instance
across sessions. If the user rotates their API key (edits
`secrets.toml` and restarts… but the daemon is still running),
the stale key persists in memory until daemon restart.

**Mitigation.** Documented in `docs/M5-verification.md`:
"Rotating an API key requires `systemctl --user restart
zwhisperd`." Adding hot-reload is M6+ work (would require a
filesystem watcher on `secrets.toml` and a key-rotation
signal — non-trivial; outside M5 scope).

**Open question.** Is restart-to-reload acceptable, or is a
filesystem-watch hot-reload an M5 requirement? Default: restart-only.

### 4. (M5+, severity low) Cost preview in tray menu

**Risk.** A user running `cloud-meeting` for 8 h burns ~$3.69 of
Deepgram credit (8 h × 60 min × $0.0077). A naive user might not
realise. IDEA.md § 12 already flags cost surprises.

**Mitigation (deferred).** Profile-level
`cost_estimate_per_min` field, tray tooltip showing
"~$X.XX for current session". Not in M5 scope; logged in
"Open contract asks for M6+".

### 5. (M5, severity high) Network egress audit

**Risk.** A user with no internet (laptop on a flight) selects
`cloud-meeting` → recording stops → transcribe fails 90 s later
(C4 budget) → audio file is preserved but the user has no
transcript. Worse: the daemon retried and burned 90 s of CPU
spinning.

**Mitigation.** The first attempt's `is_connect()` error fires
fast (< 5 s usually); the retry delay of 1 s + 2 s + 4 s + …
caps wall-clock to budget. The `BackendNetwork` error message
includes "check your internet connection" hint. The audio file is
preserved by the daemon (DoD already true via M3 lifecycle).
**No change in M5.**

**Open question.** Should the daemon fail fast (1-attempt only)
on `is_connect()` errors, vs apply the full retry budget? The
plan's default is "retry connect errors with the same budget" —
matches the researcher's note. Alternative: short-circuit on
`is_connect()` after attempt 1 (since DNS/TCP failure usually
isn't transient on a 90 s scale). Recommendation: keep retries on
connect errors; users on flaky tethering DO benefit. Confirm.

### 6. (M5+, severity medium) `Profiles1.list_v2()` is not auto-discovered by older trays

**Risk.** A user upgrades the daemon to M5 but somehow runs a
M4-vintage tray binary. The tray still calls `list()` and shows
no ☁ marker. Functionally fine (graceful degradation per § 6
in "Public API rules") — but visually inconsistent.

**Mitigation.** `docs/M5-verification.md` notes "for the ☁
marker to render, both daemon AND tray must be at M5 vintage";
the systemd unit version-pin is M8 packaging work.

**No M5 fix needed**, logged for M8 packaging coordination.

### 7. (M5, severity medium) `secrets.toml` parent directory permissions

**Risk.** We check `secrets.toml`'s mode and uid, but not the
parent directory `~/.config/zwhisper/`. A world-writable
parent directory means an attacker can `mv` the file
underneath us. `O_NOFOLLOW` on the file itself does not save
us if the parent is the attack vector.

**Mitigation.** Add a parent-directory mode check: refuse if
`~/.config/zwhisper/` is mode-other-writable. Implementation is
a second `metadata()` call in P1's `secrets/resolver.rs`;
~10 lines.

**Lock-in for M5.** Yes, ship the check. Reject if parent is
group- or other-writable (`mode & 0o022 != 0`).
**Update DoD #3 to reflect this** (already implicit via the
"Permissions" variant; `docs/M5-verification.md` explicitly
covers it).

### 8. (M5+, severity low) `tier` field in `[transcription.deepgram]`

**Risk.** Deepgram historically had a `tier` query param
(`enhanced`, `nova`, `base`) for model selection. With `nova-3`
the `model` param is the full identifier and `tier` is moot
for new accounts. But existing Deepgram dashboards may still
expose it.

**Mitigation.** `tier: Option<String>` in `DeepgramTomlConfig`,
threaded into the query string only when `Some`. Documented in
`docs/M5-verification.md` as "rarely needed; older accounts
only". No DoD coverage; `[transcription.deepgram]` round-trip
test covers serialisation.

## Validation strategy

| DoD # | Test name | Verification command |
|---|---|---|
| 1 | end-to-end CLI on live key | `cargo run -p zwhisper-cli -- transcribe --profile cloud-meeting fixtures/short.flac` (manual; live-key required) |
| 2 | `secrets_resolver::missing_key_fails_fast` | `cargo test -p zwhisper-core --test secrets_resolver missing_key_fails_fast` |
| 3 | `secrets_resolver::rejects_world_readable_toml` + parent-dir check (R7) | `cargo test -p zwhisper-core --test secrets_resolver` |
| 4 | `profile::schema::tests::deepgram_profile_validates` | `cargo test -p zwhisper-core profile::schema::tests::deepgram_profile_validates` |
| 5 | `profile::schema::tests::whisper_profile_unchanged_after_m5` | `cargo test -p zwhisper-core profile::schema::tests::whisper_profile_unchanged_after_m5` |
| 6 | `transcribe::deepgram::tests::groups_words_into_speaker_segments` | `cargo test -p zwhisper-core --test deepgram_backend groups_words_into_speaker_segments` |
| 7 | `transcribe::whisper_cpp::tests::artifacts_speakers_none` | `cargo test -p zwhisper-core whisper_cpp::tests::artifacts_speakers_none` |
| 8 | `tray::tests::cloud_marker_prepends_for_remote_backend` | `cargo test -p zwhisper-tray cloud_marker_prepends_for_remote_backend` |
| 9 | `log_redaction::api_key_never_logged_e2e` | `cargo test -p zwhisper-core --test log_redaction` |
| 10 | `transcribe::deepgram::tests::error_classification_table` | `cargo test -p zwhisper-core --test deepgram_backend error_classification_table` |
| 11 | `transcribe::deepgram::tests::rejects_plaintext_url` | `cargo test -p zwhisper-core --test deepgram_backend rejects_plaintext_url` |
| 12 | `transcribe::deepgram::tests::endpoint_is_constant_https` | `cargo test -p zwhisper-core --test deepgram_backend endpoint_is_constant_https` |
| 13 | `transcribe::tests::backend_config_enum_exhaustive` | `cargo test -p zwhisper-core backend_config_enum_exhaustive` |
| 14 | `profiles_service::tests::list_v2_includes_backend` + `profile_entry_v2_signature_is_ssus` | `cargo test -p zwhisperd list_v2_includes_backend && cargo test -p zwhisper-ipc profile_entry_v2_signature_is_ssus` |
| 15 | `transcribe::deepgram::tests::retry_budget_caps_total_wall_time` | `cargo test -p zwhisper-core --test deepgram_backend retry_budget_caps_total_wall_time` |
| 16 | `transcribe::deepgram::tests::client_reused_across_calls` | `cargo test -p zwhisper-core --test deepgram_backend client_reused_across_calls` |
| 17 | `transcribe::deepgram::tests::flac_body_is_streamed` | `cargo test -p zwhisper-core --test deepgram_backend flac_body_is_streamed -- --ignored` (manual, RSS-bound) |
| 18 | `docs/M5-verification.md` checklist | manual |

| Layer | Approach |
|---|---|
| Unit tests | secrets resolver, speaker grouping algorithm, query-param construction, error classification — all pure functions, table tests |
| Integration tests | wiremock-driven Deepgram endpoint with hand-crafted fixtures for happy path, partial diarization, every error code |
| Live-network tests | gated `#[cfg(feature = "live-deepgram")]`; require `ZWHISPER_DEEPGRAM_API_KEY` env. Cost note: a 5-second fixture run is ≈ $0.0006 — budget-irrelevant on the $200 free tier |
| Manual verification | `~/.config/zwhisper/secrets.toml` chmod 600 + 0o644 + 0o400 paths; tray ☁ marker on KDE Plasma 6; daemon restart-on-config-rotation flow |
| Daemon-without-cloud | A whisper-cpp profile still works under M5 binaries — no regression, the BackendConfig::WhisperCpp arm is the default |
| Cost-aware test design | All wiremock tests use a fixture key `DG-TEST-KEY-XYZZY` that is NEVER sent to a real endpoint — wiremock intercepts at the local-loopback level |

## Open contract asks (logged for M6+)

1. **`Backend.HealthCheck` D-Bus method** — diagnostic
   `cz.zajca.Zwhisper1.Backend1.HealthCheck(s backend) -> (b ok, s message)`
   so a settings GUI / tray can probe "is the Deepgram API key
   valid right now?" without running a full transcription. Useful
   for M7 settings GUI validation, M8 packaging post-install
   checks. Not in M5.

2. **Profile-level `cost_estimate_per_min: Option<f64>`** —
   reuses § Risks #4. Lets the tray render an estimated cost in
   the tooltip while recording. Profile schema bump to
   `schema_version = 2` (M2-compatible migration since the field
   is `Option`).

3. **`Profiles2.ProfilesChanged` signal** — already logged by
   M4-plan. The `list_v2()` addition does not subsume it — the
   tray still polls every 60 s. Genuine push-driven refresh
   stays an M6+ contract bump.

4. **`StreamingTranscriber` sibling trait** — when a streaming
   backend is genuinely needed (Deepgram WS, AssemblyAI WS).
   `transcribe_stream(&self, audio: impl AsyncRead) -> impl
   Stream<Item = TranscriptDelta>`. M5 ships the schema room for
   it (`Capabilities.streaming: bool`) but no impl.

5. **API-key rotation signal** — `cz.zajca.Zwhisper1.Daemon1.ReloadSecrets()`
   D-Bus method so editing `secrets.toml` does not require a
   daemon restart. Per § Risks #3.

6. **`secrets.toml` per-backend section validation** — currently
   the resolver looks up `[deepgram] api_key`. As more backends
   land (`[assemblyai] api_key`, `[openai] api_key`), the
   resolver should fail-clearly when the requested backend's
   section is missing AND another backend's section is present
   ("you have an `[openai]` key but the profile uses `deepgram`").

All six are pure additions through new versioned interfaces or
additive schema fields — none break the M3 / M5 wire surface.
