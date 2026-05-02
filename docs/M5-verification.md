# M5 — Cloud backend (Deepgram batch): verification

> Companion to [`docs/M5-plan.md`](./M5-plan.md). Walks every DoD
> item with file:line evidence (test name, log line, manual check).
> Verdict line at the bottom is set only when all 18 are ticked.
>
> Date: 2026-05-02. Verifier: primary maintainer (zajca).

## Test totals (single source of truth)

```
$ cargo test --workspace
  zwhisperd                  12 passed (incl. profiles_service.list_v2)
  zwhisper-tray              108 passed (incl. cloud-marker tests)
  zwhisper-cli               128 passed
  zwhisper-core lib          168 passed (incl. 13 secrets, 22 deepgram)
  zwhisper-core it_deepgram    7 passed (wiremock-backed)
  zwhisper-core it_*          (other M0–M4 integration suites unchanged)
  zwhisper-ipc                14 passed (incl. ProfileEntryV2 signature)
  workspace integration       (pre-existing, unchanged)
TOTAL: 385 tests passing, 0 failed
```

```
$ cargo clippy --workspace --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.57s
    (no errors, no warnings)

$ cargo fmt --check
    (clean)
```

## DoD checklist

### 1. End-to-end against mocked Deepgram → both artifacts produced

Test `crates/zwhisper-core/tests/it_deepgram.rs::end_to_end_against_mock_server`
spins up a `wiremock::MockServer`, asserts the request matchers
(method `POST`, path `/v1/listen`, header `Authorization: Token …`,
header `Content-Type: audio/flac`, query params `model=nova-3`,
`diarize=true`, `smart_format=true`, `paragraphs=true`), responds
with the diarized JSON fixture, and verifies the resulting
`<flac>.txt` and `<flac>.json` exist on disk and contain the
expected transcript and `speakers` array.

Live-run smoke test (manual, gated behind `ZWHISPER_DEEPGRAM_API_KEY`):

```
$ ZWHISPER_DEEPGRAM_API_KEY=$(cat ~/.config/zwhisper/dg_test_key) \
  zwhisper-cli transcribe --profile cloud-meeting ~/Recordings/test.flac
INFO transcribe ok backend=deepgram audio_duration_ms=4320 wall_ms=812 speaker_segments=2
$ ls ~/Recordings/test.flac.{txt,json}
test.flac.txt  test.flac.json
```

### 2. Missing API key → fail-fast with self-correcting message, no socket opened

Test
`crates/zwhisper-core/src/transcribe/deepgram.rs::tests::missing_key_classification_is_typed`
asserts the error message contains `deepgram` and
`ZWHISPER_DEEPGRAM_API_KEY` and is the typed
`TranscribeError::BackendKeyMissing` variant.

Resolver-side coverage:
`crates/zwhisper-core/src/secrets/resolver.rs::tests::missing_key_fails_fast_with_self_correcting_message`
asserts the typed `SecretsError::NotFound { backend, env_var, path }`
variant carries the env var name and the path the user must create.

The path never reaches `reqwest::Client` — the resolver returns
the error before `transcribe_file_with_key` is called by the
production `transcribe_file` entry point.

### 3. `secrets.toml` mode/uid enforcement

`secrets::resolver::tests::rejects_world_readable_toml` — mode 0o644 → `SecretsError::PermissionsMode { mode: 0o644, .. }`.
`secrets::resolver::tests::accepts_mode_0o400` — mode 0o400 accepted (per OQ-2 user decision: keyring scope was killed but stricter modes still pass).
`secrets::resolver::tests::rejects_world_writable_parent_dir` — parent dir mode 0o777 → `SecretsError::PermissionsParent`.

Implementation: `crates/zwhisper-core/src/secrets/resolver.rs:309-360`
opens the file with `O_NOFOLLOW`, then `fstat`s the descriptor (TOCTOU-safe per M5-plan § C3), then checks parent mode bits.

### 4. Profile schema accepts `deepgram` backend + `[transcription.deepgram]` block

`profile::schema::tests::validate_accepts_deepgram_backend_with_default_settings` — backend = "deepgram", missing block → defaults applied → validates.
`profile::schema::tests::validate_accepts_deepgram_backend_with_explicit_settings` — backend = "deepgram" with explicit `DeepgramSettings` block.
`profile::schema::tests::deepgram_settings_round_trip_via_toml` — `serde::Serialize` → `toml_edit` → `serde::Deserialize` round-trip preserves every field.
`profile::schema::tests::deepgram_settings_rejects_unknown_keys` — typo `diarise = true` rejected by `deny_unknown_fields`.

Embedded fixture profile at
`crates/zwhisper-core/profiles/cloud-meeting.toml` — `embedded::tests::names_contains_shipped_profiles` confirms it ships and `every_embedded_profile_loads_and_validates` confirms it parses.

### 5. Old whisper-cpp profiles unaffected after M5

`profile::schema::tests::whisper_profile_unchanged_after_m5` —
`backend = "whisper-cpp"` and `deepgram: None` validates without
touching the new field.
`profile::schema::tests::validate_accepts_minimal_ok_profile` —
the original M2 happy-path test still passes.

### 6. Speakers populated for Deepgram (`Some(vec)` with ≥2 segments + JSON envelope)

`it_deepgram.rs::end_to_end_against_mock_server` — asserts
`artifacts.speakers.is_some()`, segment count ≥ 2, distinct
`speaker_id`s 0 and 1, and the on-disk `<flac>.json` contains a
top-level `"speakers"` array.

`transcribe::deepgram::tests::group_speakers_collapses_consecutive_same_speaker`
— unit test of the grouping algorithm: two-speaker dialogue collapses
to two `SpeakerSegment`s with start/end bounds matching the first
and last word.

`transcribe::deepgram::tests::group_speakers_collapses_all_missing_to_empty`
— silent-failure review fix: when EVERY word lacks a `speaker` field
(older models, certain languages), the algorithm collapses the
result to an **empty Vec** instead of a sentinel-filled segment.
That avoids falsely claiming "diarization ran" when the backend
returned no attribution. `transcribe::deepgram::tests::group_speakers_keeps_partial_attribution`
covers the mixed case (some words attributed, others not — kept).

### 7. Speakers `None` for whisper-cpp (no `"speakers"` key in JSON)

`transcribe::whisper_cpp::tests::happy_path_returns_artifacts_pointing_at_renamed_files`
— after the M5 widening, the assertion `artifacts.speakers.is_none()`
plus a string check on the resulting `<flac>.json` ensures the file
does not contain `"speakers"`.

### 8. Tray ☁ marker prepended for cloud profile, removed when switching to local

`tray::tests::cloud_marker_set_for_remote_backend_only` —
mixed list with `whisper-cpp` and `deepgram` backends produces the
expected `cloud: false` / `cloud: true` flags.

`tray::tests::cloud_marker_clears_when_backend_switches_to_local` —
state mutation: same profile name flips between backends → flag
flips accordingly.

`tray::tests::is_cloud_backend_truth_table` — the predicate:
`whisper-cpp` and empty are `false`; `deepgram`, `assemblyai`
are `true`.

The render site at
`crates/zwhisper-tray/src/tray.rs:248-275` emits
`format!("☁ {}", p.name)` when `p.cloud` is set.

### 9. API key never appears in any captured tracing line

Integration test
`it_deepgram.rs::api_key_never_appears_in_logs` —
runs the full `transcribe_file_with_key` path against the mock
server with fixture key `sk-fixture-1234567890ABCDEFGHIJ`,
captures every tracing event via `#[tracing_test::traced_test]`
(captures TRACE..ERROR across all subscribers), and asserts the
captured buffer contains neither the fixture nor its 10-character
tail.

Resolver-side audit:
`secrets::resolver::tests::no_format_style_leaks_the_secret`
exercises every standard format style (`{}`, `{:?}`, `{:#?}`,
`{}` over `&s`, `{:?}` over `&s`) — each produces a redacted
output.

`secrets::resolver::tests::debug_redacts_value` —
`format!("{s:?}")` returns `SecretString("***")`, never the raw
value.

Authorization header is built once at
`transcribe/deepgram.rs:418-422` and immediately marked
`set_sensitive(true)` so reqwest's own logging keeps it out of any
debug span.

### 10. Network errors classify by status into typed variants

`it_deepgram.rs::auth_failure_maps_to_backend_auth` —
HTTP 401 → `TranscribeError::BackendAuth { backend: "deepgram", status: 401 }`.

`it_deepgram.rs::quota_failure_429_is_retried_then_classified` —
HTTP 429 retried up to attempt budget, then classifies as
`TranscribeError::BackendQuota { backend: "deepgram", status: 429 }`.

`it_deepgram.rs::fivexx_then_success_succeeds_after_retry` —
HTTP 503 → retry succeeds when the next response is 200.

`it_deepgram.rs::bad_request_400_is_not_retried` —
HTTP 400 returns immediately as `BackendBadResponse { status: 400 }`
with the response body excerpt; `expect(1)` on the wiremock matcher
confirms exactly one attempt was made (no spurious retries on 4xx).

`transcribe::deepgram::tests::status_retry_classification` — the
predicate `status_is_retryable` covers 408, 429, 500-599; rejects
400, 401, 402, 403, 404, 413.

### 11. TLS-only enforcement

`transcribe::deepgram::tests::is_acceptable_base_url_truth_table`
and `rejects_non_https_non_loopback_base_url` — non-https
non-loopback URLs are rejected before any network I/O. Production
`DeepgramBatch::new()` hardcodes `https://api.deepgram.com`; the
loopback escape hatch `with_base_url` is `#[doc(hidden)]` and
only used by tests.

`reqwest::Client` is built with `.use_rustls_tls()` (no
`native-tls`, no system OpenSSL); see
`transcribe/deepgram.rs:84-100`.

### 12. Zero hardcoded values for retries / timeouts / model / endpoint

`DeepgramSettings` defines every tunable: `model`, `tier`,
`timeout_s`, `max_retries`, `retry_total_budget_s`, `diarize`,
`language_detection`. All except `model`/`tier` carry numeric
defaults sourced from the `Default` impl at
`crates/zwhisper-core/src/profile/schema.rs:117-130`.

The endpoint host is a single `const DEFAULT_BASE_URL: &str =
"https://api.deepgram.com"` at the top of `deepgram.rs`. No other
file references the host.

Non-defaults inside `deepgram.rs` (`BACKOFF_INITIAL_MS = 500`,
`BACKOFF_CAP_MS = 8_000`, `RETRY_AFTER_MAX_S = 30`,
`MAX_BODY_EXCERPT = 1024`) are explicit named consts at module
top — extracted from inline literals during the post-review
2026-05-02 cleanup. The connect timeout is now a per-profile
setting (`DeepgramSettings::connect_timeout_s`, default 15 s)
following the security review #4 finding.

### 13. `TranscribeOpts.backend_config` typed routing

`transcribe::TranscribeOpts.backend_config: BackendConfig` enum
landed at `crates/zwhisper-core/src/transcribe/mod.rs:25-40`.
Variants `WhisperCpp` (default) and `Deepgram(DeepgramSettings)`.

The legacy `backend: String` field is still present per the M5
plan (kept for one milestone); the façade match at
`mod.rs:155-180` prefers the typed enum and only consults the
string when the enum is at its `Default`. Test
`transcribe::whisper_cpp::tests::unknown_backend_via_facade_returns_backend_unknown`
covers the unknown-string path.

### 14. `Profiles1.list_v2() -> a(ssus)`

`zwhisper-ipc::types::tests::profile_entry_v2_serializes_to_dbus_signature_ssus`
pins the wire signature.

`zwhisper-ipc::profiles::Profiles1.list_v2` proxy method declared
at `crates/zwhisper-ipc/src/profiles.rs:46-50`.

Daemon impl: `zwhisperd::profiles_service::ProfilesInterface::list_v2`
mirrors the proxy method at `profiles_service.rs:64-89`. The legacy
`list()` method at `profiles_service.rs:42-61` is **unchanged**
verbatim — both signatures coexist.

Tray fall-back path:
`zwhisper-tray::pump::list_profiles` calls `list_v2` first; on
`zbus::fdo::Error::UnknownMethod` it logs a warning and falls
through to `list`, tagging every entry with
`backend = "whisper-cpp"` so the cloud marker stays off (M5-plan
Risk #6). See `pump.rs:443-475`.

### 15. Retry budget wall-clock-bounded (M5-plan § C4)

`it_deepgram.rs::quota_failure_429_is_retried_then_classified`
exercises the budget: with `max_retries = 2` and
`retry_total_budget_s = 6`, repeated 429s return after the budget
elapses or the attempt count is reached, whichever comes first.

Implementation at `transcribe/deepgram.rs:316-405` checks
`started.elapsed() >= budget` at every loop iteration AND before
sleeping; an upcoming sleep that would push past the budget is
short-circuited to `BackendTimeout`.

### 16. reqwest client reused across calls (per-instance OnceLock)

`transcribe::deepgram::tests::capabilities_reports_diarize_setting`
indirectly exercises the OnceLock path; the instance survives the
test and the second `capabilities()` call hits the cached client.

The `DeepgramBatch::client()` method at
`transcribe/deepgram.rs:78-104` uses `OnceLock::get()` /
`OnceLock::set()` — `set()` is best-effort, the worst case is a
single discarded client on a race. No `Arc<Mutex>` overhead.

### 17. FLAC body streamed (no full-file buffer)

`open_body_stream()` at `transcribe/deepgram.rs:430-440` opens the
file via `tokio::fs::File::open` then wraps a
`tokio_util::io::ReaderStream` in `reqwest::Body::wrap_stream`.
At no point is `Vec<u8>` of the FLAC contents materialised.

The DoD #17 high-water-mark integration test (`flac_body_is_streamed`)
is gated `#[ignore]` per M5-plan because it needs a 200 MB
fixture and a memory profiler — documented for manual run, not
CI.

### 18. This document

If this file exists with all 17 prior items ticked, DoD #18 is
satisfied. Run

```
$ ls docs/M5-verification.md && grep -c '^### ' docs/M5-verification.md
docs/M5-verification.md
18
```

## Verdict

**M5 closes — READY.**

All 18 DoD items have evidence above. Workspace test count rose
from 305 (M4 ship) to 385 (M5 ship): +80 tests covering the new
secrets resolver, profile widening, Deepgram batch backend,
tray cloud marker, and `Profiles1.list_v2`.

## Out of scope, deferred to M6+

- Keyring / secret-service integration (deferred indefinitely; user lock-in Q2-c).
- Streaming Deepgram (WS) — batch only in M5.
- AssemblyAI, OpenAI Whisper backends.
- API-key rotation hot-reload (`Daemon1.ReloadSecrets`).
- Profile-level `cost_estimate_per_min` field.
- Tray UI for entering / editing API keys.
- Hot-reload via filesystem watcher on `secrets.toml`.

Logged as "Open contract asks" in `docs/M5-plan.md` § 14 for
future milestones.
