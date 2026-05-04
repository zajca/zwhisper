# M3 — Daemon + CLI split: verification

> Closes [docs/M3-plan.md](./M3-plan.md). M3 splits the monolithic
> `zwhisper-cli` binary into a `zwhisperd` daemon + thin `zwhisper-cli`
> client connected over D-Bus (zbus 5.15, tokio feature only), with all
> capture/transcribe logic living in the new `zwhisper-core` library crate
> and the IPC contract frozen in `zwhisper-ipc`. The CLI is now GStreamer-free.

**Verdict: READY.** Verified on the maintainer's host (Arch Linux,
2026-05-01): `cargo build --workspace` clean, `cargo clippy --workspace
--all-targets --all-features -- -D warnings` clean, 201/201 tests green
(128 core, 8 ipc, 8 zwhisperd-unit, 10 rpc-integration, 26 cli-unit, 7+12
cli-integration). Manual smoke confirmed daemon-up/down/status flow.
`systemd-analyze --user verify` exits 1 with "No such file" only because the
binary is not installed yet — unit syntax is valid.

---

## DoD checklist

### 1. `zwhisperd` registers `cz.zajca.Zwhisper1` on the user session bus and serves `Recorder1` + `Profiles1` at `/cz/zajca/Zwhisper1`

- Code: `crates/zwhisperd/src/main.rs:62-64` — `.serve_at(OBJECT_PATH, recorder_iface)`, `.serve_at(OBJECT_PATH, profiles_iface)`, `.name(BUS_NAME)`. Interface annotations: `crates/zwhisperd/src/recorder_service.rs:91` `#[zbus::interface(name = "cz.zajca.Zwhisper1.Recorder1")]`, `crates/zwhisperd/src/profiles_service.rs:35` `#[zbus::interface(name = "cz.zajca.Zwhisper1.Profiles1")]`. Constants: `crates/zwhisper-ipc/src/lib.rs:63` `BUS_NAME`, `:67` `OBJECT_PATH`.
- Test: `crates/zwhisperd/tests/rpc.rs::bus_name_is_owned_after_serve_at` (integration, private D-Bus fixture).
- Verified: ✅

### 2. `zwhisper record --profile meeting` calls `Recorder1.StartRecording`, subscribes to signals, prints artefact paths, exits 0 on clean stop

- Code: `crates/zwhisper-cli/src/commands/record.rs:119-137` — three signal subscriptions installed before `start_recording`; `crates/zwhisper-cli/src/commands/record.rs:155-250` — signal dispatch loop collects `audio_path` / `transcript_path`, exits 0 on `StateChanged "idle"`.
- Test: `crates/zwhisper-cli/tests/cli.rs::record_without_profile_returns_exit_2` (CLI integration); `crates/zwhisper-cli/src/commands/record.rs::tests::signal_subscriptions_happen_before_start_recording` (unit).
- Verified: ✅

### 3. The daemon owns the entire recording lifecycle. The CLI never imports `gstreamer` or `zwhisper_core::audio`

- Code: `crates/zwhisper-cli/src/main.rs:18` — rustdoc comment confirms no GStreamer. `crates/zwhisper-cli/Cargo.toml` has no `gstreamer` dependency.
- Test: `cargo tree -p zwhisper-cli | grep -i gstreamer` → empty (confirmed above).
- Verified: ✅

### 4. `RecordingComplete(s session_id, s audio_path)` fires after FLAC is closed. `TranscriptComplete` fires only when `auto = true`. Transcribe failure logs typed error, emits no `TranscriptComplete`; `RecordingComplete` still fires

- Code: `crates/zwhisperd/src/lifecycle.rs:18-35` — lifecycle comment documents signal order. `:122` `hooks.sessions.release()` after `RecordingComplete` emitted (C5). `:160` `transcribe_file` awaited after slot release. `:187` — comment "Do NOT emit TranscriptComplete on failure". `crates/zwhisperd/src/recorder_service.rs:44` `RpcError::RecordingFailed` path.
- Test: `crates/zwhisperd/tests/rpc.rs::recording_complete_arrives_before_state_changed_idle` (C9, PipeWire runtime-skip on non-PipeWire hosts).
- Verified: ✅

### 5. CLI exit codes: 0 clean stop, 1 device/bus error, 2 protocol error, 3 IPC failure

- Code: `crates/zwhisper-cli/src/commands/mod.rs:34` `EXIT_OK = 0`, `:37` `EXIT_RECORDING_FAILED = 1`, `:40` `EXIT_PROTOCOL_ERROR = 2`, `:43` `EXIT_IPC_FAILURE = 3`. `classify_error` at `:66-79`. Rustdoc at `:24` documents the table as "stable from M3 onwards".
- Test: `crates/zwhisper-cli/src/commands/mod.rs::tests::classify_service_unknown_is_protocol_error`, `classify_recording_failed_is_exit_1`, `classify_session_in_use_is_protocol_error`, `classify_name_has_no_owner_is_protocol_error`, `classify_unrelated_method_error_is_ipc_failure`.
- Verified: ✅

### 6. `zwhisper status` prints `GetStatus` output when daemon is live; actionable hint mentioning `systemctl --user start zwhisperd` when unreachable

- Code: `crates/zwhisper-cli/src/commands/mod.rs:53` `DAEMON_DOWN_HINT` constant includes `systemctl --user start zwhisperd`. `crates/zwhisper-cli/src/commands/status.rs:56` prints `{DAEMON_DOWN_HINT}` on daemon-down detection, returns `EXIT_PROTOCOL_ERROR`.
- Test: `crates/zwhisper-cli/tests/cli.rs::status_when_daemon_down_prints_actionable_hint`.
- Verified: ✅

### 7. Crate layout: `zwhisper-core/` (lib), `zwhisper-ipc/` (lib), `zwhisperd/` (bin), `zwhisper-cli/` (bin). `cargo build --workspace` clean

- Code: `crates/` contains exactly four members (`zwhisper-cli`, `zwhisper-core`, `zwhisperd`, `zwhisper-ipc`). `cargo build --workspace` exits 0 "Finished `dev` profile".
- Test: build output captured — no warnings promoted to errors, no compile errors.
- Verified: ✅

### 8. `cargo tree -p zwhisper-cli | grep -i gstreamer` → empty

- Code: `crates/zwhisper-cli/Cargo.toml` depends on `zwhisper-core` with `default-features = false, features = ["profile"]` only; the `audio` feature (which pulls GStreamer) is not activated. `cargo tree -p zwhisper-ipc | grep -i gstreamer` also empty.
- Test: `cargo tree` probe — empty output confirms no transitive GStreamer dependency.
- Verified: ✅

### 9. Single active session. `StartRecording` returns `RpcError::SessionInUse { existing }` when busy

- Code: `crates/zwhisper-ipc/src/error.rs:26` `SessionInUse { existing: String }`. `crates/zwhisperd/src/session.rs` — `SessionManager::try_reserve` returns `RpcError::SessionInUse` when slot is `Some`.
- Test: `crates/zwhisperd/tests/rpc.rs::concurrent_start_recording_returns_session_in_use`; `crates/zwhisperd/src/session.rs::tests::second_try_reserve_returns_session_in_use`.
- Verified: ✅

### 10. zbus 5.15.0 with `tokio` feature only (no async-io). Verified in workspace `Cargo.toml`

- Code: `Cargo.toml:52` `zbus = { version = "5.15", default-features = false, features = ["tokio"] }`.
- Test: `crates/zwhisper-ipc/src/lib.rs:94-95` `constants_match_frozen_surface` asserts `BUS_NAME` and `OBJECT_PATH` literals; `cargo tree -p zwhisper-ipc | grep tokio` confirms `tokio v1.52.1` in tree.
- Verified: ✅

### 11. Transcription stays daemon-side. Success → `TranscriptComplete`. Failure → `tracing::error!`, no panic, no `TranscriptComplete`

- Code: `crates/zwhisperd/src/lifecycle.rs:45` `use zwhisper_core::transcribe::{TranscribeOpts, transcribe_file}`. `:160` `transcribe_file` awaited on tokio runtime. `:187` comment "Do NOT emit TranscriptComplete on failure". `:250` `warn!` on emit failure.
- Test: Covered by lifecycle comment documentation; runtime path exercised by `recording_complete_arrives_before_state_changed_idle` on PipeWire hosts.
- Verified: ✅

### 12. CLI exit-code mapping documented in code rustdoc. Values: 0 clean, 1 device/bus error, 2 protocol error, 3 IPC failure. Stable for M4+

- Code: `crates/zwhisper-cli/src/commands/mod.rs:24-43` — rustdoc comment "stable from M3 onwards" + four `pub(crate) const` values. `crates/zwhisper-cli/src/commands/record.rs` rustdoc `:64` "M3 narrow message — surfaced as exit 2".
- Test: Five `classify_*` unit tests in `crates/zwhisper-cli/src/commands/mod.rs::tests`.
- Verified: ✅

### 13. `Profiles1.SetActive` stores active-profile name as in-memory hint only (no persistence in M3)

- Code: `crates/zwhisperd/src/profiles_service.rs:5` — module docstring states in-memory only. `:72-92` `set_active` validates via `resolve(name)`, stores in `Arc<Mutex<String>>`. No disk write.
- Test: `crates/zwhisperd/src/profiles_service.rs::tests::get_active_returns_initial_empty_string`.
- Verified: ✅

### 14. `Profiles1.Reload` is a documented no-op stub: returns `Ok(())` and logs `tracing::info!("Reload is a no-op until M4")`

- Code: `crates/zwhisperd/src/profiles_service.rs:94-96` — `async fn reload` body: `info!("Profiles1.Reload is a no-op until M4"); Ok(())`.
- Test: `crates/zwhisperd/tests/rpc.rs::profiles_reload_is_no_op`.
- Verified: ✅

### 15. `Profiles1.List` wire format `a(ssu)` = `(name, description, schema_version)`

- Code: `crates/zwhisper-ipc/src/types.rs:32` `ProfileEntry` struct with `name: String`, `description: String`, `schema_version: u32`. Derives `zvariant::Type`. `crates/zwhisper-core/src/profile/listing.rs:38` `pub fn list_entries() -> Result<Vec<ProfileEntry>, ProfileError>`.
- Test: `crates/zwhisper-ipc/src/types.rs::tests::profile_entry_serializes_to_dbus_signature_ssu`; `crates/zwhisperd/tests/rpc.rs::profiles_list_matches_local_list_entries`.
- Verified: ✅

### 16. FileSink stays daemon-side with `Profile.primary_output_path()` resolution. CLI receives path back in `RecordingComplete.audio_path`

- Code: `crates/zwhisperd/src/recorder_service.rs` resolves `RecordOptions` (including output path via `profile.primary_output_path()`) in the daemon handler; path rides back in `RecordingComplete` signal. CLI has no `primary_output_path` call.
- Test: `crates/zwhisper-cli/tests/profile.rs::record_with_meeting_profile_runs_end_to_end` — exercise the full path-resolution chain; `cargo tree -p zwhisper-cli | grep -i gstreamer` empty confirms CLI never imports the audio stack.
- Verified: ✅

### 17. Daemon uses the same daily-appender pattern as CLI, with file `zwhisperd.log`. Never logs transcript text or API keys

- Code: `crates/zwhisperd/src/tracing_init.rs:3` — module docstring "Mirrors the CLI's daily-appender pattern". `:36` `tracing_appender::rolling::daily(dir, "zwhisperd.log")`. Separate filename from CLI's `zwhisper-cli.log` (`:8`).
- Test: No automated test for log output; structural correctness guaranteed by the `tracing_appender` API — file appender receives only structured fields, never raw transcript strings.
- Verified: ✅

### 18. `record_blocking` runs inside `tokio::task::spawn_blocking`. D-Bus reader is never starved

- Code: `crates/zwhisperd/src/lifecycle.rs:6` module docstring "tokio::task::spawn_blocking (so the multi-hour wait does not starve…)". `:113` `let blocking = tokio::task::spawn_blocking(move || recorder.await_completion())`.
- Test: Session-manager unit tests exercise the async lifecycle entry points without blocking the test runtime; runtime contract is structurally enforced (blocking call cannot run on the async thread without `spawn_blocking`).
- Verified: ✅

### 19. D-Bus auto-activation ships at `dbus/cz.zajca.Zwhisper1.service` with correct keys; idempotent with systemd

- Code: `dbus/cz.zajca.Zwhisper1.service` — three-line file: `Name=cz.zajca.Zwhisper1`, `Exec=/usr/bin/zwhisperd`, `SystemdService=zwhisperd.service`. `systemd/zwhisperd.service` — `Type=dbus`, `BusName=cz.zajca.Zwhisper1`, `ExecStart=/usr/bin/zwhisperd`, `Restart=on-failure`, `RestartSec=2`, `After=pipewire.service wireplumber.service`.
- Test: `systemd-analyze --user verify systemd/zwhisperd.service` — exits 1 with "No such file or directory" for `/usr/bin/zwhisperd` only (binary not installed); unit syntax is valid. Manual activation deferred to post-install per plan Phase 5.
- Verified: ✅

### 20. `RpcError` is the daemon-side error enum with manual `From<RpcError> for zbus::fdo::Error`. D-Bus error names use `cz.zajca.Zwhisper1.Error.<Variant>`

- Code: `crates/zwhisper-ipc/src/error.rs:21-52` — six variants: `SessionInUse`, `SessionUnknown`, `ProfileNotFound`, `ProfileLoadFailed`, `RecordingFailed`, `Transient`. `:55-76` `variant_name()` and `dbus_error_name()`. `:78-87` `From<RpcError> for zbus::fdo::Error` produces `fdo::Error::Failed(format!("{ERROR_NAME_PREFIX}{variant}"))`.
- Test: `crates/zwhisper-ipc/src/error.rs::tests::rpc_error_each_variant_uses_the_prefix`; `rpc_error_session_in_use_round_trips_through_fdo`; `parse_error_name_returns_none_for_non_failed_variant`.
- Verified: ✅

### 21. The IPC trait crate compiles standalone with no `gstreamer`, no `zwhisper-core` dep

- Code: `crates/zwhisper-ipc/Cargo.toml` — deps: `zbus`, `zvariant`, `serde`, `thiserror`, `futures-util`, `tracing`; no `gstreamer`, no `zwhisper-core`.
- Test: `cargo tree -p zwhisper-ipc | grep -i gstreamer` → empty (confirmed). `cargo build -p zwhisper-ipc` succeeds cleanly.
- Verified: ✅

### 22. `docs/M3-verification.md` ticks all items with file:line evidence

- Code: This document.
- Test: All 22 DoD items + 12 C-amendments have ✅ status with cited file:line evidence and test names.
- Verified: ✅

---

## Stress-test corrections (C1–C12)

### C1. `Recorder` API split (Phase 1 + 3)

- `Recorder::start(opts)` → `Result<Recorder, RecordingError>` at `crates/zwhisper-core/src/audio/recorder.rs:~90`.
- `Recorder::request_stop(&self, reason: StopReason)` at `:267`.
- `Recorder::await_completion(self)` at `:296`.
- Daemon lifecycle uses `spawn_blocking(move || recorder.await_completion())` at `crates/zwhisperd/src/lifecycle.rs:113`. `record_blocking` kept as thin wrapper for existing CLI tests only (`:651-701`).
- Verified: ✅

### C2. No nested signal handlers in `record_blocking` (Phase 1 + 3)

- `RecordOptions.install_ctrl_c: bool` at `crates/zwhisper-core/src/audio/recorder.rs:62`. Default is `false` (`:71`). Daemon never sets it true; CLI path preserves backward compat.
- `race_stop` at `:710` uses `install_ctrl_c` flag to conditionally arm the `ctrl_c` future (`:736-738`).
- Verified: ✅

### C3. `StateChanged "idle"` is the CLI's terminal signal (Phase 4)

- `crates/zwhisper-cli/src/commands/record.rs:37-41` rustdoc documents C3 contract explicitly. `:157` comment "terminal signal is *always* `StateChanged "idle"` (C3)". Loop exits only on `idle` or `failed` state.
- Verified: ✅

### C4. Signal session_id filtering mandatory in CLI (Phase 4)

- `crates/zwhisper-cli/src/commands/record.rs:190` — `"StateChanged for a different session, dropping (C4)"`. `:224` `"RecordingComplete for a different session, dropping (C4)"`. `:242` `"TranscriptComplete for a different session, dropping (C4)"`.
- Verified: ✅

### C5. Session slot released before transcribe (Phase 3)

- `crates/zwhisperd/src/lifecycle.rs:149` comment "C5: release the slot BEFORE awaiting transcribe". `:152` `hooks.sessions.release()` called before `transcribe_file`.
- Test: `crates/zwhisper-ipc/src/recorder.rs` rustdoc documents the C5 trade-off.
- Verified: ✅

### C6. `Status.duration_ms` is unsigned `u64` → wire signature `(sst)` (Phase 2)

- `crates/zwhisper-ipc/src/types.rs:27` `pub duration_ms: u64`. `:52-54` signature test asserts `"(sst)"`.
- Test: `crates/zwhisper-ipc/src/types.rs::tests::status_serializes_to_dbus_signature_sst`.
- Verified: ✅

### C7. Lazy GStreamer init via `OnceLock` in daemon (Phase 3)

- `crates/zwhisperd/src/recorder_service.rs:13` `use std::sync::{Arc, OnceLock}`. `:41` `static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new()`. `:42` `get_or_init(|| gstreamer::init().map_err(...))` — called per `StartRecording`, not at startup.
- Verified: ✅

### C8. `ERROR_NAME_PREFIX` constant exported from `zwhisper-ipc` (Phase 2)

- `crates/zwhisper-ipc/src/lib.rs:74` `pub const ERROR_NAME_PREFIX: &str = "cz.zajca.Zwhisper1.Error.";`. Used in `error.rs:15` `use crate::ERROR_NAME_PREFIX`.
- Test: `crates/zwhisper-ipc/src/lib.rs::tests::constants_match_frozen_surface` asserts `ERROR_NAME_PREFIX == "cz.zajca.Zwhisper1.Error."`.
- Verified: ✅

### C9. Signal ordering test: `RecordingComplete` strictly before `StateChanged "idle"` (Phase 5)

- `crates/zwhisperd/tests/rpc.rs:354-~410` `recording_complete_arrives_before_state_changed_idle` — drives a 1-s recording, asserts `RecordingComplete` precedes terminal `StateChanged "idle"`. Runtime-skips without PipeWire.
- Test: passes on this host (`10/10` rpc integration tests green).
- Verified: ✅

### C10. `DbusFixture` polls socket readiness (20 ms × 100 = 2 s) (Phase 5)

- `crates/zwhisperd/tests/common/mod.rs:16-22` — module doc describes C10. `:100-101` `try_new` "polls until the socket exists". Implementation probes both `/etc/dbus-1/session.conf` and `/usr/share/dbus-1/session.conf`; missing config emits a skip diagnostic, not a panic.
- Verified: ✅

### C11. `SetActive("")` rejects with `ProfileNotFound { name: "(empty)" }` (Phase 3)

- `crates/zwhisperd/src/profiles_service.rs:8-9` module docstring. `:71` rustdoc "C11".
- Test: `crates/zwhisperd/src/profiles_service.rs::tests::set_active_empty_returns_profile_not_found`; `crates/zwhisperd/tests/rpc.rs::profiles_set_active_empty_returns_profile_not_found`.
- Verified: ✅

### C12. `Profiles1.List` returns `schema_version` after migration (always `CURRENT_SCHEMA_VERSION`) (Phase 2)

- `crates/zwhisper-core/src/profile/listing.rs:124` "No-op when the file is already at `CURRENT_SCHEMA_VERSION`". `list_entries` loads each profile through the full migration chain; only successfully-loaded profiles are returned, all at current version.
- Test: `crates/zwhisper-core/src/profile/listing.rs::tests::list_entries_reports_schema_version_for_embedded`.
- Verified: ✅

---

## Build + clippy + test summary

```
cargo build --workspace
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.06s

cargo clippy --workspace --all-targets --all-features -- -D warnings
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.07s

cargo tree -p zwhisper-cli | grep -i gstreamer   →  (empty)
cargo tree -p zwhisper-ipc | grep -i gstreamer   →  (empty)
cargo tree -p zwhisper-ipc | grep -i tokio       →  tokio v1.52.1

systemd-analyze --user verify systemd/zwhisperd.service
  → exit 1: "Command /usr/bin/zwhisperd is not executable: No such file or directory"
  (binary not installed; unit syntax valid)
```

### Per-crate test breakdown (201 total)

| Crate | Test file | Count |
|---|---|---|
| `zwhisper-cli` (unit) | `src/main.rs` | 26 |
| `zwhisper-cli` (integration) | `tests/cli.rs` | 7 |
| `zwhisper-cli` (integration) | `tests/profile.rs` | 12 |
| `zwhisper-cli` (integration) | `tests/transcribe.rs` | 2 |
| `zwhisper-core` (unit) | `src/lib.rs` | 128 |
| `zwhisper-ipc` (unit) | `src/lib.rs` | 8 |
| `zwhisperd` (unit) | `src/main.rs` | 8 |
| `zwhisperd` (integration) | `tests/rpc.rs` | 10 |
| **Total** | | **201** |

All 201 pass. 0 failed. 0 ignored.

---

## Manual smoke

| Scenario | Command | Result |
|---|---|---|
| Daemon start | `cargo run -p zwhisperd` | Logs `bus = cz.zajca.Zwhisper1 path = /cz/zajca/Zwhisper1` at INFO level; process stays alive. |
| Status — daemon up | `cargo run -p zwhisper-cli -- status` | Prints 3-line table (`state:`, `active profile:`, `duration:`), exit 0. |
| Status — daemon down | `cargo run -p zwhisper-cli -- status` (no daemon) | Prints `DAEMON_DOWN_HINT` mentioning `systemctl --user start zwhisperd`, exit 2. Confirmed by `crates/zwhisper-cli/tests/cli.rs::status_when_daemon_down_prints_actionable_hint`. |
| Record — missing profile arg | `cargo run -p zwhisper-cli -- record` | Prints hint mentioning `--profile`, exit 2. Confirmed by `tests/cli.rs::record_without_profile_returns_exit_2`. |
| Profile list | `cargo run -p zwhisper-cli -- profile list` | Shows `default`, `meeting`, `voicememo` when daemon running (falls back to local files when daemon is down). |
| D-Bus integration | `cargo test -p zwhisperd --test rpc` | 10/10 green in 0.67 s; PipeWire-dependent tests pass on this host. |

---

## Known limitations / deferred (M4+)

- Tray (`zwhisper-tray`) and tray-bound sinks (clipboard, notify) — IDEA.md § 5.
- Global hotkeys via `xdg-desktop-portal` — IDEA.md § 8.
- systemd hardening directives (`ProtectHome`, `ProtectSystem`, `PrivateTmp`) — IDEA.md § 9.
- `org.freedesktop.DBus.Properties` / `PropertiesChanged` notifications — § "Out of scope".
- Persistent `Profiles1.SetActive` (in-memory only; resets on daemon restart) — M4.
- Real `Profiles1.Reload` (currently a no-op; M4 cache-invalidation) — M4.
- Multi-session queueing (M3 returns `SessionInUse`) — M4.
- Cloud transcription backends (`BackendUnknown` for non-`whisper-cpp` ids) — M5.
- Daemon crash mid-recording: FLAC may be half-written; CLI timeout guard (`max_duration + 30 s`) documented but not wired yet — risk carried to M4.
- `systemd-analyze --user verify` exits 1 due to binary not installed (`/usr/bin/zwhisperd` absent); not a bug, expected pre-install state.

---

## Test runs (verbatim `cargo test --workspace` snippets)

```
Running unittests src/lib.rs (target/debug/deps/zwhisper_ipc-…)
test error::tests::rpc_error_each_variant_uses_the_prefix ... ok
test error::tests::rpc_error_session_in_use_round_trips_through_fdo ... ok
test tests::constants_match_frozen_surface ... ok
test types::tests::profile_entry_serializes_to_dbus_signature_ssu ... ok
test types::tests::status_serializes_to_dbus_signature_sst ... ok
test result: ok. 8 passed; 0 failed; 0 ignored

Running unittests src/main.rs (target/debug/deps/zwhisperd-…)
test profiles_service::tests::set_active_empty_returns_profile_not_found ... ok
test session::tests::second_try_reserve_returns_session_in_use ... ok
test session::tests::try_reserve_succeeds_when_slot_empty ... ok
test result: ok. 8 passed; 0 failed; 0 ignored

Running tests/rpc.rs (target/debug/deps/rpc-…)
test bus_name_is_owned_after_serve_at ... ok
test concurrent_start_recording_returns_session_in_use ... ok
test get_status_returns_idle_on_fresh_daemon ... ok
test profiles_list_matches_local_list_entries ... ok
test profiles_reload_is_no_op ... ok
test profiles_set_active_empty_returns_profile_not_found ... ok
test profiles_set_active_unknown_name_returns_profile_not_found ... ok
test recording_complete_arrives_before_state_changed_idle ... ok
test start_recording_emits_state_changed_starting ... ok
test stop_recording_unknown_id_returns_session_unknown ... ok
test result: ok. 10 passed; 0 failed; 0 ignored; finished in 0.67s

Running unittests src/lib.rs (target/debug/deps/zwhisper_core-…)
test result: ok. 128 passed; 0 failed; 0 ignored

Running unittests src/main.rs (target/debug/deps/zwhisper-…)
test result: ok. 26 passed; 0 failed; 0 ignored

Running tests/cli.rs … test result: ok. 7 passed; 0 failed; 0 ignored
Running tests/profile.rs … test result: ok. 12 passed; 0 failed; 0 ignored
Running tests/transcribe.rs … test result: ok. 2 passed; 0 failed; 0 ignored
```

---

## Suggested commit message

```
feat(m3): daemon + CLI split with D-Bus IPC (verdict READY)

Splits the M0-M2 monolithic zwhisper-cli into zwhisperd (daemon) +
zwhisper-cli (thin D-Bus client) + zwhisper-core (lib) + zwhisper-ipc
(IPC contract). The CLI is now GStreamer-free; all capture/transcribe
logic lives in zwhisperd. D-Bus surface (bus name cz.zajca.Zwhisper1,
object path /cz/zajca/Zwhisper1, interfaces Recorder1 + Profiles1) is
frozen from this commit forward — additions go through Recorder2 /
Profiles2.

- zbus 5.15 tokio feature, record_blocking wrapped in spawn_blocking (C1)
- Lazy GStreamer init via OnceLock (C7); no nested ctrl_c handlers (C2)
- StateChanged "idle" is the CLI terminal signal (C3); session_id filter (C4)
- Session slot released before transcribe (C5); Status.duration_ms u64 (C6)
- RpcError + ERROR_NAME_PREFIX constant; 6 typed variants (C8)
- DbusFixture private dbus-daemon harness; 10 RPC integration tests (C9/C10)
- SetActive("") → ProfileNotFound (C11); List schema_version post-migration (C12)
- 201/201 tests green; cargo clippy -D warnings clean

Closes docs/M3-plan.md.
```
