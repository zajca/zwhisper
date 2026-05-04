# M3 — Daemon + CLI split: implementation plan

> Target milestone from [IDEA.md § 11](../IDEA.md#11-roadmap). Splits
> the M0–M2 monolithic `zwhisper-cli` binary into a `zwhisperd`
> daemon + thin `zwhisper-cli` client + shared `zwhisper-ipc` crate
> over a relocated `zwhisper-core` library, per IDEA.md § 2 (D-Bus
> interface), § 5 (FileSink stays daemon-side), § 9 (systemd
> integration), and § 11 (M3 DoD verbatim: "Rozdělit na `zwhisperd`
> + `zwhisper-cli`, D-Bus IPC. Start přes CLI, daemon nahrává
> nezávisle, signal `TranscriptComplete` doručí cestu; CLI funguje
> bez tray.").

## Status snapshot (2026-05-01)

| Area | State | Evidence |
|---|---|---|
| Crate `zwhisper-core` exists | not done | `crates/` only contains `zwhisper-cli/` (cf. `Cargo.toml:3` `members = ["crates/*"]`) |
| Crate `zwhisper-ipc` exists | not done | no `crates/zwhisper-ipc/` |
| Crate `zwhisperd` (daemon bin) exists | not done | no `crates/zwhisperd/` |
| `zbus` workspace dep declared | not done | `Cargo.toml:33` lists `tokio` features but no `zbus` (IDEA.md § 15 marks zbus as M3) |
| `record_blocking` callable from a non-CLI crate | not done | declared `pub(crate) fn` at `crates/zwhisper-cli/src/audio/recorder.rs:577` |
| `RecordOptions` / `RecordingReport` reachable cross-crate | not done | both `pub(crate)` (`audio/recorder.rs:46` and `:65`); `audio/mod.rs:9-11` re-exports `pub(crate)` only |
| `transcribe::transcribe_file` reachable cross-crate | not done | `pub(crate) async fn` at `crates/zwhisper-cli/src/transcribe/mod.rs:84` |
| `profile::load(name)` reachable cross-crate | not done | `pub(crate) fn` at `crates/zwhisper-cli/src/profile/mod.rs:71` |
| `RecorderState::Display` (idle/starting/recording/stopping/failed) | exists | `crates/zwhisper-cli/src/audio/state.rs:36-46` (will be reused as the wire format for `GetStatus`) |
| `SessionId` type | exists | `crates/zwhisper-cli/src/audio/state.rs:9-15` (uuid v4); state.rs:5 docstring already says "M3 will surface this on the D-Bus `StartRecording` reply" |
| `StopReason::is_error()` exit-code helper | exists | `crates/zwhisper-cli/src/audio/state.rs:65-69` (M3 plan locks this as the CLI exit-code mapper) |
| Object path / bus name registered anywhere | not done | no daemon process exists |
| systemd unit file `systemd/zwhisperd.service` | not done | no `systemd/` dir at repo root (IDEA.md § 14 marks "M3+") |
| D-Bus activation file `dbus/cz.zajca.Zwhisper1.service` | not done | no `dbus/` dir at repo root |
| `Status` subcommand prints daemon state | placeholder | `crates/zwhisper-cli/src/main.rs:62-64` prints a static `"M2 profile system; daemon split lands in M3"` string |
| `init_gstreamer()` lives in CLI (must move) | yes | `crates/zwhisper-cli/src/main.rs:106-114` (must move to daemon; CLI becomes GStreamer-free in M3) |
| `record_blocking` builds its own current-thread runtime | yes (must change) | `audio/recorder.rs:580-585` — daemon must wrap this in `spawn_blocking` instead of nesting a runtime |

**Verdict.** M3 is greenfield wrt the daemon + IPC, but reuses
M0–M2 capture/transcribe/profile code unchanged. The audio /
profile / transcribe modules already carry M3-aware doc comments
at `audio/recorder.rs:42-44`, `audio/state.rs:5-6, 25-28, 55-58`,
and `transcribe/mod.rs:46-48` — the public surface they imply *is*
the M3 contract. No business logic moves; visibility widens from
`pub(crate)` to `pub`, files relocate to a new crate, and a thin
RPC adapter wraps them.

## Definition of done

Each item below is a testable assertion. Items 1–6 are the IDEA.md
§ 11 verbatim DoD; items 7–22 lock in the architectural decisions.

1. `zwhisperd` registers `cz.zajca.Zwhisper1` on the user session
   bus and serves `Recorder1` + `Profiles1` at object path
   `/cz/zajca/Zwhisper1`.
2. `zwhisper record --profile meeting` calls
   `Recorder1.StartRecording("meeting")`, subscribes to
   `StateChanged` / `RecordingComplete` / `TranscriptComplete`,
   prints the artefact paths, exits 0 on a clean stop.
3. The daemon owns the entire recording lifecycle (GStreamer init,
   `record_blocking`, EOS finalisation, post-record transcribe).
   The CLI never imports `gstreamer` or `zwhisper_core::audio`.
4. `RecordingComplete(s session_id, s audio_path)` fires after the
   FLAC is closed and validated. `TranscriptComplete(s session_id,
   s transcript_path, x bytes, s backend)` fires after the
   `.flac.txt` is written, only when `transcription.auto = true`.
   Transcribe failure logs the typed error and emits **no**
   `TranscriptComplete`; `RecordingComplete` still fires so the
   user has a recoverable state.
5. CLI exit codes: `record` exits 0 when `RecordingComplete`
   arrives (and `TranscriptComplete` if profile auto = true); 1
   when `StateChanged "failed"` or any `StopReason::is_error()`
   surfaces (`audio/state.rs:65-69`).
6. `zwhisper status` prints `GetStatus` output when the daemon is
   live, and a one-line actionable hint mentioning `systemctl
   --user start zwhisperd` when the daemon is unreachable.
7. Crate layout: `zwhisper-core/` (lib), `zwhisper-ipc/` (lib),
   `zwhisperd/` (bin), `zwhisper-cli/` (bin). `cargo build
   --workspace` clean.
8. **Daemon = single source of truth for capture.** Verified by
   `cargo tree -p zwhisper-cli | grep -i gstreamer` → empty.
9. **Single active session in M3.** `StartRecording` returns
   `RpcError::SessionInUse { existing }` when busy; the existing
   id rides in the error message.
10. zbus 5.15.0 with the `tokio` feature only (no async-io).
    Verified in workspace `Cargo.toml`: `zbus = { version = "5.15",
    default-features = false, features = ["tokio"] }`.
11. **Transcription stays daemon-side.** After `record_blocking`
    returns, the daemon awaits `transcribe::transcribe_file`
    on the tokio runtime. Success → `TranscriptComplete`. Failure
    → `tracing::error!`, no panic, no `TranscriptComplete`.
12. **CLI exit-code mapping** documented in code rustdoc and in
    Phase 4 below. Values: `0` clean stop, `1` device/bus error,
    `2` user-facing protocol error (daemon down, profile not
    found), `3` IPC failure (bus disconnect mid-call). Stable for
    M4+.
13. **`Profiles1.SetActive`** stores active-profile name as
    in-memory hint only (no persistence in M3).
14. **`Profiles1.Reload`** is a documented no-op stub: returns
    `Ok(())` and logs `tracing::info!("Reload is a no-op until M4")`.
    Profile loader resolves on every `StartRecording`, so a TOML
    edit between sessions still takes effect.
15. **`Profiles1.List`** wire format `a(ssu)` = `(name, description,
    schema_version)` — sourced from existing
    `profile::commands::list` enumeration logic
    (`crates/zwhisper-cli/src/profile/commands.rs:170-192`).
16. **FileSink stays daemon-side** with the same
    `Profile.primary_output_path()` resolution
    (`profile/schema.rs:213`). CLI never picks the path; it
    receives it back in `RecordingComplete.audio_path`.
17. **Logs**: daemon uses the same daily-appender pattern as
    `crates/zwhisper-cli/src/main.rs:71-104`, with file
    `zwhisperd.log`. Never log transcript text or API keys.
18. **GStreamer + tokio**: `record_blocking` runs inside
    `tokio::task::spawn_blocking`. Gating concurrency contract:
    the daemon's main worker pool never sees the multi-hour
    blocking call, so the D-Bus reader is never starved
    (researcher § 8).
19. **D-Bus auto-activation** ships at
    `dbus/cz.zajca.Zwhisper1.service` with `Name=cz.zajca.Zwhisper1`,
    `Exec=/usr/bin/zwhisperd`, `SystemdService=zwhisperd.service`.
    Idempotent with `systemctl --user start zwhisperd`.
20. **`RpcError`** is the daemon-side error enum with manual
    `From<RpcError> for zbus::fdo::Error` (researcher gotcha § 6 —
    `#[derive(zbus::DBusError)]` is undocumented in 5.15.0).
    Variants: `SessionInUse`, `SessionUnknown { id }`,
    `ProfileNotFound { name }`, `ProfileLoadFailed { name, reason
    }`, `RecordingFailed { reason }`, `Transient { reason }`.
    D-Bus error names use `cz.zajca.Zwhisper1.Error.<Variant>`.
21. The IPC trait crate compiles standalone with no `gstreamer`,
    no `zwhisper-core` dep. It declares the `#[interface]` /
    `#[proxy]` traits, the wire-format structs, and nothing else.
22. `docs/M3-verification.md` ticks all of the above with file:line
    evidence (test name, log line, `dbus-monitor` capture). Verdict
    line "M3 closes …" only after all 22 are ticked.

## Out of scope (deferred to M4+)

- Tray (`zwhisper-tray`) and tray-bound clipboard / notification
  sinks — M4.
- Cloud transcription backends — M5; the dispatcher at
  `crates/zwhisper-cli/src/transcribe/mod.rs:88-99` still surfaces
  `BackendUnknown` for non-`whisper-cpp` ids in M3.
- Hotkey registration (`xdg-desktop-portal`) — M6.
- Persistent `Profiles1.SetActive`, real `Profiles1.Reload` — M4.
- Stereo-split capture pipeline — `Profile::validate` still
  rejects (M2 carry-over).
- Multi-session queueing — M3 returns `SessionInUse`.
- D-Bus property notifications (`PropertiesChanged`) — M3 uses
  bare signals only.
- systemd hardening directives (`ProtectHome`, `ProtectSystem`,
  `PrivateTmp`) — IDEA.md § 9 explicitly defers as last-mile.

## Architecture for M3

Cargo workspace, four crates:

```
zwhisper/
├── Cargo.toml                       # adds zbus, zvariant, futures-util, signal-hook-tokio
├── crates/
│   ├── zwhisper-core/               # NEW lib: audio + profile + transcribe relocated
│   │   ├── Cargo.toml               # features = ["audio", "profile"]; CLI uses no-default-features + ["profile"]
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── audio/               # moved from zwhisper-cli/src/audio/ (gated by feature `audio`)
│   │       ├── profile/             # moved (always on)
│   │       │   └── profiles/        # embedded TOML moves with the module (CARGO_MANIFEST_DIR-relative)
│   │       └── transcribe/          # moved (gated by feature `audio` because it pulls async runtime)
│   ├── zwhisper-ipc/                # NEW lib: zbus traits + wire types + RpcError
│   │   └── src/{lib,recorder,profiles,types,error}.rs
│   ├── zwhisperd/                   # NEW bin: the daemon
│   │   └── src/{main,recorder_service,profiles_service,session,signals}.rs
│   └── zwhisper-cli/                # rewritten: thin client
│       └── src/{main,cli,commands/{record,transcribe,profile,status}}.rs
├── systemd/zwhisperd.service        # NEW; user unit, Type=dbus, BusName=cz.zajca.Zwhisper1
└── dbus/cz.zajca.Zwhisper1.service  # NEW; D-Bus session activation
```

Rationale: `zwhisper-core` is library-only. `zwhisper-ipc` is
deliberately tiny (only the wire shape and zbus traits) so the
build is fast and a future tray can depend on it without pulling
GStreamer. `zwhisperd` and `zwhisper-cli` are siblings; only the
daemon depends on `zwhisper-core` with default features.

### Public API rules (M3 lock-ins)

Reversing any of these in M4+ breaks the D-Bus surface, which is
irrevocable once the activation file ships.

1. **Interface names frozen**: `cz.zajca.Zwhisper1.Recorder1` and
   `cz.zajca.Zwhisper1.Profiles1`, verbatim from IDEA.md § 2. The
   `1` suffix follows the `org.freedesktop.DBus1` convention; M4
   widening goes through `Recorder2`, never an incompatible
   mutation of `Recorder1`.
2. **Object path is `/cz/zajca/Zwhisper1`**, single object instance.
   No sub-paths in M3.
3. **Wire-format structs** implement `zvariant::Type +
   serde::Serialize + serde::Deserialize` (researcher § 9):
   `Status { state: String, active_profile: String, duration_ms:
   i64 }` (signature `(ssx)`) and `ProfileEntry { name: String,
   description: String, schema_version: u32 }` (signature
   `(ssu)`). `Status.state` matches `RecorderState::Display` at
   `audio/state.rs:36-46` exactly.
4. **`SessionId` crosses the wire as `String`** (uuid hyphenated).
   The daemon stringifies on egress; the CLI keeps it as
   `String`. IDEA.md § 2 reads `(s session_id)` — human-readable
   boundary, no `Uuid` derive on the wire.
5. **Signal payloads are bare tuples**, no nested struct.
   `RecordingComplete(s session_id, s audio_path)` is `(String,
   String)`, matching the zbus `#[zbus(signal)]` convention
   (researcher § 1).
6. **`RpcError` lives in `zwhisper-ipc::error`** with manual
   `From<RpcError> for zbus::fdo::Error`; D-Bus error name format
   `cz.zajca.Zwhisper1.Error.<Variant>`. The mapping is the only
   place the prefix is hardcoded.
7. **`record_blocking` runs in `spawn_blocking`.** No exceptions.
   Documented in the daemon's
   `recorder_service::start_recording` rustdoc.
8. **Profile lookups happen on every `StartRecording`.** The
   daemon does not cache. A user editing the TOML between sessions
   sees the change without `Reload` — making lookups uncached is
   the cheap path that makes `Reload` a true no-op.
9. **Active-profile hint** in `Profiles1.GetActive` / `SetActive`
   is in-memory only. Daemon restart resets to `""`.
10. **Bus name registration** uses `RequestNameFlags::DO_NOT_QUEUE`
    + `REPLACE_EXISTING = false`. If two daemons race, the loser
    aborts with a clear log line.

### D-Bus contract (frozen, IDEA.md § 2 verbatim)

```
interface cz.zajca.Zwhisper1.Recorder1 {
    StartRecording(s profile_name) -> (s session_id);
    StopRecording(s session_id)    -> (s session_id);
    GetStatus()                    -> (s state, s active_profile, x duration_ms);

    StateChanged(s new_state, s session_id);
    RecordingComplete(s session_id, s audio_path);
    TranscriptComplete(s session_id, s transcript_path, x bytes, s backend);
}

interface cz.zajca.Zwhisper1.Profiles1 {
    List() -> (a(ssu));            // [(name, description, schema_version)]
    GetActive() -> (s);
    SetActive(s name);
    Reload();
}
```

State string mapping (`s new_state`): `idle | starting | recording
| stopping | failed`, sourced from `RecorderState::Display` at
`audio/state.rs:36-46`. The `Display` impl is the canonical wire
format; do not introduce a parallel mapping in `zwhisper-ipc`.

### Concurrency model

The daemon runs a single tokio current-thread runtime. The D-Bus
reader is a zbus task on that runtime. RPC handlers are async.
The recording lifecycle uses two task layers:

- The handler takes a `tokio::sync::Mutex<Option<ActiveSession>>`
  briefly, slots a session struct (id, profile name,
  `oneshot::Sender<()>` cancel), drops the mutex, then spawns a
  lifecycle task with `tokio::spawn`.
- The lifecycle task runs `tokio::task::spawn_blocking(move ||
  record_blocking(opts, dur))`, watches the existing
  `tokio::sync::watch<StopReason>` channel
  (`audio/recorder.rs:103-110`), forwards state transitions as
  D-Bus signals, then on completion awaits `transcribe_file` (if
  `auto`) and emits `RecordingComplete` /
  `TranscriptComplete` accordingly.

The mutex is **never** held across a `spawn_blocking` await.
`StopRecording` acquires the mutex, reads the cancel channel out
of the `ActiveSession`, sends the cancel, drops the mutex.

### Single-session policy

`SessionManager::reserve(profile_name) -> Result<(SessionId,
oneshot::Receiver<()>), RpcError>`. If
`Mutex<Option<ActiveSession>>` already holds `Some`, return
`SessionInUse { existing: id.to_string() }`. Otherwise build a
fresh `SessionId`, wire the cancel channel, slot the
`ActiveSession`, return.

### CLI exit-code map

| Exit | Trigger | Source |
|---|---|---|
| `0` | clean stop, optional transcript delivered | `StateChanged "idle"` after `RecordingComplete` (and `TranscriptComplete` if `auto`) |
| `1` | device or bus error | `StopReason::is_error()` true (`audio/state.rs:65-69`); surfaces as `RecordingFailed` + `StateChanged "failed"` |
| `2` | user-facing protocol error | `ProfileNotFound`, `SessionInUse`, `BackendUnknown`, daemon-down hint |
| `3` | IPC failure | bus disconnect mid-call; `zbus::Error::*` other than method-call rejection |

Documented in `crates/zwhisper-cli/src/commands/record.rs` rustdoc
("`record` exit codes — stable from M3 onwards").

## Phased breakdown

Each phase is a single PR-sized commit set; phases run sequentially.
Each phase block is **self-contained** so an implementation
sub-agent can act on it without re-reading IDEA.md.

### Phase 0 — Workspace bootstrap (~1 h)

**Scope.** Add the three new crates as empty skeletons. Update the
workspace dependency table. No business logic moves yet.

**Files to touch.** `Cargo.toml` (workspace deps);
`crates/zwhisper-core/Cargo.toml` + `src/lib.rs` (placeholder);
`crates/zwhisper-ipc/Cargo.toml` + `src/lib.rs` (placeholder);
`crates/zwhisperd/Cargo.toml` + `src/main.rs` (placeholder).

**Workspace dep additions** (to `Cargo.toml:15-54`):
- `zbus = { version = "5.15", default-features = false, features = ["tokio"] }`
- `zvariant = "5.7"` (matched zbus minor; pin one version
  explicitly so consumer crates don't pull two copies)
- `futures-util = "0.3"` (signal stream `next().await` on the CLI)
- `signal-hook-tokio = { version = "0.3", features = ["futures-v0_3"] }`
  (daemon SIGTERM; `tokio::signal::ctrl_c` only handles SIGINT)
- `tempfile = "3"` already present (`Cargo.toml:53`); reused in
  Phase 5 dbus fixture.

**Success criteria.** `cargo build --workspace` clean on the
maintainer's box; `cargo test --workspace` still 155/155 green
(M2 baseline, `docs/M2-verification.md` "Test runs"); `cargo
metadata --format-version 1 | jq '.packages[].name'` lists the
four crates; `Cargo.lock` resolves `zbus` to exactly `5.15.x`
with no second copy from a transitive dep.

**Test strategy.** No new tests. Existing suite is the regression
net.

**Estimated risk.** Low. The only risky piece is the
`zbus`/`zvariant` minor pin; if `zvariant` 5.7 disagrees with zbus
5.15 over the `Type` derive, Phase 2 catches it and we adjust
here.

### Phase 1 — `zwhisper-core` extraction (~4 h)

**Scope.** Move `audio/`, `profile/`, `transcribe/` from
`crates/zwhisper-cli/src/` into `crates/zwhisper-core/src/`.
Widen `pub(crate)` to `pub` only on symbols `zwhisperd` will
need. **Behavioural change is zero.** Every existing test moves
alongside the code.

**Files to touch.** `crates/zwhisper-core/Cargo.toml` (declare
deps: `gstreamer`, `tokio`, `tracing`, `serde`, `serde_json`,
`toml_edit`, `chrono`, `dirs`, `uuid`, `which`, `libc`,
`async-trait`, `include_dir`, `shellexpand`, `thiserror`); split
features `audio` / `profile` so the CLI can take core
without GStreamer (`default-features = false, features = ["profile"]`).
`crates/zwhisper-core/src/lib.rs` (`pub mod audio; pub mod profile;
pub mod transcribe;`). Move `crates/zwhisper-cli/src/{audio,profile,transcribe}/*`
to the corresponding paths under `zwhisper-core/src/`. Move
`crates/zwhisper-cli/profiles/` → `crates/zwhisper-core/profiles/`
(`include_dir!` is `CARGO_MANIFEST_DIR`-relative so the embedded
macro path rebases automatically). `crates/zwhisper-cli/Cargo.toml`
drops the relocated deps and adds
`zwhisper-core = { path = "...", default-features = false, features = ["profile"] }`.
`crates/zwhisper-cli/src/main.rs` replaces `mod audio; mod profile;
mod transcribe;` with `use zwhisper_core::{profile};` (audio /
transcribe references now go through Phase 4 RPC paths).

**Visibility widening checklist** (`pub(crate)` → `pub`):
`audio/recorder.rs:46` `RecordOptions`, `:65` `RecordingReport`,
`:577` `record_blocking`; `audio/state.rs:9` `SessionId`, `:30`
`RecorderState`, `:55` `StopReason`, `:65` `StopReason::is_error`;
`audio/error.rs::{RecordingError, DeviceError}` (entire enums);
`profile/mod.rs:24` `ProfileSource`, `:42` `resolve`, `:71`
`load`; `profile/error.rs::ProfileError`; `profile/schema.rs::*`
(`Profile`, `Sources`, `Recording`, `Transcription`, `Hotkey`,
`OutputDest`, `Mode`, `Codec`, `Backend`); `profile/schema.rs:213`
`Profile::primary_output_path`;
`profile/loader.rs::CURRENT_SCHEMA_VERSION`;
`transcribe/mod.rs:36` `TranscribeOpts`, `:55`
`TranscriptArtifacts`, `:84` `transcribe_file`;
`transcribe/error.rs::TranscribeError`.

**Visibility NOT widened** (stays `pub(crate)` inside core):
`audio::{pipeline, devices, watchdog}`,
`profile::{loader::load_from_path, migrations, paths, embedded}`
(the `load(name)` façade is the only public entry point),
`transcribe::{discovery, models, whisper_cpp}`.

**`profile::commands` carve-out.** `commands::list()` currently
prints to stdout (`profile/commands.rs:22`). This phase introduces
a sibling `pub fn list_entries() -> Result<Vec<ProfileEntry>,
ProfileError>` returning the data structure (`name`, `source`,
`schema_version`, `description`); the printing helper stays in
the CLI. Daemon's `Profiles1.List` calls `list_entries`. CLI
`profile list` also calls `list_entries` and renders the same
table.

**Success criteria.** `cargo build --workspace` clean; `cargo
test --workspace` still 155/155 green (no test count delta —
tests move, not added); `cargo clippy --workspace --all-targets
--all-features -- -D warnings` clean; `grep -rn "pub(crate) fn
record_blocking" crates/zwhisper-core/src/audio/` returns nothing
(now `pub`); `zwhisper record --output /tmp/x.flac --duration 2`
still records on the maintainer's box (manual smoke).

**Test strategy.** Refactor — existing tests are the regression
net. Add **one** new test in
`crates/zwhisper-cli/tests/cross_crate_smoke.rs` that imports
`zwhisper_core::profile::Profile` and serialises it, proving the
public surface is reachable from a sibling crate.

**Estimated risk.** Medium. `include_dir!` is
`CARGO_MANIFEST_DIR`-relative; if the move is botched the
embedded profiles silently disappear and `profile list` returns
`(no profiles found)`. The M2 test
`embedded::tests::every_embedded_profile_loads_and_validates`
must stay green after the move — run it first before declaring
Phase 1 done.

### Phase 2 — `zwhisper-ipc` contract (~3 h)

**Scope.** Define the zbus proxy traits and wire types in
`zwhisper-ipc`. The trait definition lives on the proxy side; the
daemon's `impl Recorder1Service` mirrors the same method set
verbatim. Per researcher § 2, zbus 5.15.0 expects `#[interface]`
on a real `impl` block that owns its own state — keeping the two
sides byte-identical is the phase deliverable.

**Files to touch.** `crates/zwhisper-ipc/Cargo.toml` (deps:
`zbus`, `zvariant`, `serde`, `thiserror`, `futures-util`,
`tracing`); `src/lib.rs` (re-exports);
`src/recorder.rs` (`#[proxy] trait Recorder1` with three methods +
three signals); `src/profiles.rs` (`#[proxy] trait Profiles1`);
`src/types.rs` (`Status`, `ProfileEntry`, `BUS_NAME`,
`OBJECT_PATH`, `INTERFACE_NAME_RECORDER1`,
`INTERFACE_NAME_PROFILES1` consts);
`src/error.rs` (`RpcError` enum + `From<RpcError> for
zbus::fdo::Error`).

**Method signatures (proxy side).** `Recorder1` declares
`async fn start_recording(&self, profile_name: &str) ->
zbus::Result<String>`, `stop_recording(&self, session_id: &str)`,
`get_status(&self) -> zbus::Result<Status>`, plus
`#[zbus(signal)]` for `state_changed(new_state: &str, session_id:
&str)`, `recording_complete(session_id: &str, audio_path: &str)`,
`transcript_complete(session_id: &str, transcript_path: &str,
bytes: i64, backend: &str)`. Wire signature for `get_status` is
`(ssx)` exactly. `Profiles1` declares `list(&self) ->
zbus::Result<Vec<ProfileEntry>>` (signature `a(ssu)`),
`get_active`, `set_active`, `reload`.

**`RpcError` mapping** (manual, gotcha § 6):
`From<RpcError> for fdo::Error` produces
`fdo::Error::Failed(format!("{name}: {err}"))` where `name` is
`cz.zajca.Zwhisper1.Error.<Variant>`. We use `Failed` because
`fdo::Error` does not expose an "arbitrary error name" variant
in 5.x; the CLI parses the prefix back when surfacing exit-code
2.

**Success criteria.** `cargo build -p zwhisper-ipc` clean;
`cargo tree -p zwhisper-ipc | grep -i gstreamer` empty;
`cargo test -p zwhisper-ipc` runs at least three unit tests:
`RpcError` round-trip through `From<…> for fdo::Error` preserves
the variant name; `Status` and `ProfileEntry`
`zvariant::signature()` strings match `(ssx)` and `(ssu)`; the
proxy traits compile when wrapped in `RecorderProxy<'_>` against
a `Connection` placeholder (compile-fence test).

**Test strategy.** Pure unit tests; no bus yet (Phase 5 brings the
fixture). The signature tests are the canary that catches a
wire-format drift.

**Estimated risk.** Medium. zbus's macro errors for
`zvariant::Type` mismatches are obscure. If `Status` fails to
match `(ssx)`, double-check that `duration_ms: i64` (not `u64`) —
`x` is signed.

### Phase 3 — `zwhisperd` binary (~6 h)

**Scope.** Implement the daemon. Tokio current-thread runtime,
GStreamer init at startup, register `cz.zajca.Zwhisper1` and
serve both interfaces at `/cz/zajca/Zwhisper1`, single-session
manager, `record_blocking` via `spawn_blocking`, signal emission
at the right transitions, post-record transcribe on the tokio
runtime, SIGINT/SIGTERM ⇒ graceful drain.

**Files to touch.** `crates/zwhisperd/Cargo.toml` (deps:
`zwhisper-core` with default features, `zwhisper-ipc`, `zbus`,
`tokio`, `tracing`, `tracing-subscriber`, `tracing-appender`,
`signal-hook-tokio`, `color-eyre`, `gstreamer`, `dirs`, `chrono`,
`futures-util`); `src/main.rs` (entrypoint, logging mirroring
`crates/zwhisper-cli/src/main.rs:71-104` but with file
`zwhisperd.log`, GStreamer init, runtime, bus registration);
`src/recorder_service.rs` (`pub struct Recorder1Service { session:
Arc<SessionManager> }` + `impl` annotated `#[zbus::interface(name
= "cz.zajca.Zwhisper1.Recorder1")]`); `src/profiles_service.rs`
(`Profiles1Service` + `impl`); `src/session.rs` (`ActiveSession`,
`SessionManager`); `src/signals.rs` (SIGINT + SIGTERM handler).

**Daemon `main` shape.** `#[tokio::main(flavor =
"current_thread")]`. Steps: `color_eyre::install()?`,
`init_tracing()`, `gstreamer::init()`, build `Recorder1Service` /
`Profiles1Service`, `zbus::connection::Builder::session()?
.name(BUS_NAME)?.serve_at(OBJECT_PATH, recorder)?
.serve_at(OBJECT_PATH, profiles)?.build().await?`. Race
`signals::wait_for_termination()` against connection lifetime; on
termination call `session.abort_active()` then drop the connection.

**`Recorder1Service::start_recording` shape.** (1) Load profile
via `zwhisper_core::profile::load(profile_name)`; map errors to
`RpcError::ProfileLoadFailed`. (2) Resolve `RecordOptions` the
same way `cli::run_record_with_profile` does today
(`crates/zwhisper-cli/src/cli.rs:121-178`): take
`profile.primary_output_path()`, `profile.sources.{mic,
system_output}`, `profile.recording.max_duration_minutes`. (3)
`session.reserve(profile.name).await?` → `SessionInUse` if busy,
otherwise returns `(SessionId, cancel_rx)`. (4) Emit
`StateChanged "starting"` via the captured `SignalContext`. (5)
Spawn lifecycle task: `tokio::task::spawn_blocking(move ||
record_blocking(opts, effective_duration))`, await it, then on
`Ok(report)` emit `RecordingComplete(session_id,
report.audio_path)`, then if `profile.transcription.auto`
call `transcribe_file(&report.audio_path, &opts).await` (already
async — no `spawn_blocking`), and on `Ok(art)` emit
`TranscriptComplete(session_id, art.txt_path,
fs::metadata(&art.txt_path)?.len() as i64, profile.transcription.backend.as_str())`.
On any error path log via `tracing::error!`, emit `StateChanged
"failed"` with the session id, free the session slot. Always
finish with `StateChanged "idle"` if not `failed`. (6) Return the
session id string from the handler.

**`Profiles1Service` shape.** `list` calls
`zwhisper_core::profile::list_entries()`, maps the `Vec` directly
to `Vec<ProfileEntry>`. `get_active` returns the in-memory hint
(empty string by default). `set_active` calls
`zwhisper_core::profile::resolve(name)` to validate that the
profile exists, then stores the name; non-existent names return
`ProfileNotFound`. `reload` is a no-op that logs once and returns
`Ok(())`.

**Success criteria.** `zwhisperd` starts up clean; logs `bus =
cz.zajca.Zwhisper1 path = /cz/zajca/Zwhisper1`. `busctl --user
introspect cz.zajca.Zwhisper1 /cz/zajca/Zwhisper1` shows both
interfaces with the expected method/signal sets. `busctl --user
call cz.zajca.Zwhisper1 /cz/zajca/Zwhisper1
cz.zajca.Zwhisper1.Profiles1 List` returns `a(ssu) 3 "default" "…"
1 "meeting" "…" 1 "voicememo" "…" 1`. `busctl ... GetStatus`
returns `(ssx) "idle" "" 0` on a freshly-started daemon. Two
concurrent `StartRecording` calls: first returns a session id,
second returns `cz.zajca.Zwhisper1.Error.SessionInUse: …`. SIGINT
mid-recording produces a clean `RecordingComplete` then
`StateChanged "idle"`; the FLAC on disk is `flac -t` valid.

**Test strategy.** Unit tests for `SessionManager` (state machine:
reserve → cancel → reserve again, double-cancel, abort_active).
Real D-Bus integration tests land in Phase 5 with the private bus
fixture. Manual verification via `busctl` and `dbus-monitor` here.

**Estimated risk.** High. This is the bulk of M3.
- `SignalContext` lifetime: `ctx: zbus::SignalContext<'_>` borrows
  the connection; passing it into `tokio::spawn` needs
  `to_owned()` (researcher § 1 / § 10).
- The forwarder task (`watch::Receiver<StopReason>` → signal
  emission) must drain the channel even after the recorder is
  done, otherwise the final `RecordingComplete` may be missed.
- `From<RpcError> for fdo::Error` lives in `zwhisper-ipc`, not in
  the daemon — avoids each handler duplicating the mapping.

### Phase 4 — `zwhisper-cli` rewrite (~5 h)

**Scope.** Replace local-execution paths with D-Bus calls. Each
command opens a `zbus::Connection::session()`, builds the proxy,
calls methods, awaits signals. No GStreamer dep. `profile clone`
and `profile migrate` stay local (they touch user TOML files; a
daemon RPC for them is M4).

**Files to touch.** `crates/zwhisper-cli/Cargo.toml` (drop
`gstreamer`; add `zbus`, `zwhisper-ipc`, `futures-util`; keep
`zwhisper-core` with `default-features = false, features =
["profile"]` for the local-only profile commands — closes DoD #8
because the `profile`-only feature does not pull GStreamer);
`src/main.rs` (drop `init_gstreamer`, delete `mod audio;`, the
`Status` arm dispatches to `commands::status::run`); `src/cli.rs`
(clap surface unchanged from M2; dispatch moves to `commands/`);
`src/commands/{mod,record,transcribe,profile,status}.rs` (new).

**Record command flow.** `commands/record.rs`: open session
connection; build `Recorder1Proxy`; **subscribe first** to all
three signals (`receive_state_changed`, `receive_recording_complete`,
`receive_transcript_complete`) — installing the `MatchRule`
before `start_recording` is the M3 fix for the missed-signal race
(risk #2); call `proxy.start_recording(name).await`; load profile
locally with `zwhisper_core::profile::load(name)` to know whether
`transcription.auto` is true (so the CLI knows whether to wait
for `TranscriptComplete`); race tokio `ctrl_c` against signal
arrivals: on Ctrl+C call `proxy.stop_recording(&session_id).await`
and continue waiting for the terminal signal; map the final
`StateChanged` to an exit code via the table in DoD #12.

**Profile command flow.** `profile list` and `profile show
<name>` proxy to the daemon (so the active daemon is the source
of truth for "what profiles exist"). `profile clone <src> <dst>`
and `profile migrate <name>` stay local — `clone` writes a
brand-new file under `${XDG_CONFIG_HOME}/zwhisper/profiles/`
(daemon has no business there in M3); `migrate` rewrites a
user-owned TOML in-place using the M2 pipeline. Local execution
preserves the M2 semantics exactly.

**Status command flow.** `commands/status.rs` opens the proxy,
calls `Recorder1.GetStatus`. On `zbus::Error::MethodError(name,
…)` where `name == "org.freedesktop.DBus.Error.ServiceUnknown"`
or `"org.freedesktop.DBus.Error.NameHasNoOwner"`, print
"daemon not running. To start it manually: `systemctl --user
start zwhisperd`. Or send any zwhisper command — the D-Bus
activation file at `/usr/share/dbus-1/services/cz.zajca.Zwhisper1.service`
will spawn it on first call." Exit code 2. On other zbus errors
exit 3. On success print a 3-line table (`state:`, `active
profile:`, `duration:`).

**Behaviour change vs M2.** M3 narrows the CLI to require
`--profile` for `record`. The bare-flag path (`--output --mic
--monitor --transcribe --model --lang`) stays in clap but the
runtime returns exit 2 with a hint pointing at `--profile
default`. The embedded `default` profile preserves the same
effective values for users who relied on the M0/M1 invocation
shape. Documented in code rustdoc + the verification doc.

**Success criteria.** `cargo tree -p zwhisper-cli | grep -i
gstreamer` → empty (DoD #8). `zwhisper record --profile meeting`
against a running daemon records to disk and exits 0 with the
FLAC + `.flac.txt` at the daemon-reported paths. `zwhisper
status` against a running daemon prints the three-line table; no
daemon → "daemon not running" hint, exit 2. `zwhisper profile
list` against a running daemon prints the same three-row table
M2 verification captured. `cargo test -p zwhisper-cli` clap
tests stay green.

**Test strategy.** Mostly manual + clap unit tests; D-Bus
integration tests are Phase 5. Add one
`commands::record::tests::record_without_profile_returns_exit_2`
asserting the M3 contract.

**Estimated risk.** Medium. The biggest gotcha is the
missed-signal race — the subscription must be installed before
the `StartRecording` reply arrives. Test explicitly with a fast
profile (`max_duration_minutes` low) and confirm the CLI sees
`starting → recording → stopping → idle` in order.

### Phase 5 — Tests + systemd + activation file (~5 h)

**Scope.** Build the D-Bus integration test harness (private
`dbus-daemon` fixture per researcher § 7), the systemd user
unit, and the D-Bus activation file. This phase puts the
end-to-end-via-bus guarantee behind a regression net.

**Files to touch.** `crates/zwhisperd/tests/rpc.rs` (new
integration tests); `crates/zwhisperd/tests/common/mod.rs` (new
`DbusFixture`); `systemd/zwhisperd.service` (new);
`dbus/cz.zajca.Zwhisper1.service` (new).

**`DbusFixture` shape** (researcher § 7): `pub struct DbusFixture
{ daemon: Child, address: String, _tmp: tempfile::TempDir }`. On
`new`, `tempfile::tempdir`, build socket path
`tmp/bus.sock`, spawn `dbus-daemon --config-file=/etc/dbus-1/session.conf
--address=unix:path=… --nofork`, wait briefly for the socket to
appear (50 × 20 ms poll), set `DBUS_SESSION_BUS_ADDRESS` for the
test process. `Drop` kills the daemon. Tests are
`#[tokio::test(flavor = "current_thread")]`. Runtime-skip on
hosts without `dbus-daemon` (M0/M1 PipeWire skip pattern).

**Integration tests** (each pinned to a DoD item):
- `bus_name_is_owned_after_serve_at` (DoD #1).
- `get_status_returns_idle_on_fresh_daemon` (DoD #6).
- `start_recording_emits_state_changed_starting` — subscribe,
  call, assert signal arrives within 200 ms.
- `concurrent_start_recording_returns_session_in_use` (DoD #9).
- `stop_recording_unknown_id_returns_session_unknown`.
- `profiles_list_matches_local_list_entries` — daemon's `List()`
  returns the same set as `zwhisper_core::profile::list_entries()`.
- `profiles_set_active_unknown_name_returns_profile_not_found`.
- `profiles_reload_is_no_op` — call twice, no error, no
  observable mutation.
- `recording_lifecycle_emits_state_recording_then_idle` — drive a
  short recording end-to-end.

The recording lifecycle test runtime-skips on hosts without
PipeWire. The non-recording tests run unconditionally.

**`systemd/zwhisperd.service`.** `[Unit] Description = zwhisper
recording daemon; After = pipewire.service wireplumber.service;
Requires = pipewire.service. [Service] Type = dbus; BusName =
cz.zajca.Zwhisper1; ExecStart = /usr/bin/zwhisperd; Restart =
on-failure; RestartSec = 2. [Install] WantedBy = default.target.`
`Type=dbus` is correct for a session-bus service: systemd waits
for `RequestName` before considering the unit active. No hardening
directives in M3 (IDEA.md § 9 explicitly defers).

**`dbus/cz.zajca.Zwhisper1.service`.** `[D-BUS Service] Name =
cz.zajca.Zwhisper1; Exec = /usr/bin/zwhisperd; SystemdService =
zwhisperd.service`. The `SystemdService=` line makes dbus-daemon
delegate startup to systemd when both are configured (avoids two
competing daemons; researcher § 4 + IDEA.md § 9). Without it,
dbus-daemon would direct-spawn `/usr/bin/zwhisperd`, bypassing
the unit's `After=pipewire.service` dependency.

**Success criteria.** `cargo test -p zwhisperd --test rpc` green;
recording lifecycle test prints `[SKIP]` on hosts without
PipeWire (no silent gaps). Suite runs in <30 s. `systemd-analyze
--user verify systemd/zwhisperd.service` green. After a manual
install of the activation file to `/usr/share/dbus-1/services/`,
a `dbus-send` against the bus name spawns the daemon and the RPC
succeeds.

**Test strategy.** Integration tests via `DbusFixture`. systemd
unit verified by `systemd-analyze`; D-Bus activation by manual
install + a one-shot `busctl` call (logged in
`M3-verification.md`). No automated test for activation because
it requires a live session bus + root install paths.

**Estimated risk.** Medium-high. `dbus-daemon` config file
location varies by distro (`/etc/dbus-1/session.conf` on Arch /
Fedora; `/usr/share/dbus-1/session.conf` on Debian-likes); the
fixture must probe both, failure mode is a clean skip with a
diagnostic. The recording lifecycle test is the flakiest because
it spans GStreamer + tokio + zbus; if signal timing is racy,
fall back to a 2 s polling assertion instead of a tight
`next().await`.

### Phase 6 — Verification doc + commit (~2 h)

**Scope.** Mirror `docs/M2-verification.md` exactly. Walk all 22
DoD items, each linked to evidence (file:line, test name, or a
`busctl` / `dbus-monitor` capture). Manual end-to-end run on the
maintainer's host: `zwhisper record --profile meeting`,
`tail -f $XDG_STATE_HOME/zwhisper/zwhisperd.log` confirms state
transitions, FLAC + `.flac.txt` on disk match the daemon-reported
paths, CLI exit code 0.

**Files to touch.** `docs/M3-verification.md` (new; mirror M2
structure). `docs/M0-plan.md`, `docs/M1-plan.md`,
`docs/M2-plan.md` (status snapshot tables get an "M3 done"
cross-link, same pattern M2 used).

**Verification doc shape** (mirrors M2-verification): one-paragraph
blurb + verdict line; 22-item DoD checklist with file:line
evidence; frozen snapshots — `busctl --user introspect`,
`dbus-monitor` capture for a 60 s `--profile meeting` lifecycle,
`zwhisper status` against live + dead daemon, `cargo test
--workspace` count (M2 baseline 155 + Phase-5 tests; expected
~165–170), clippy clean line; deviations + risks-carried sections.

**Sign-off.** M3 closes only when `docs/M3-verification.md` is
committed with all 22 DoD items ticked and the `dbus-monitor`
capture in the doc. Verdict line `M3 closes 2026-…` matches the
M2 sign-off style. Commit message: `feat(m3): daemon + CLI split
+ D-Bus IPC (verdict READY)`, mirroring the M2 commit (`151c703
feat(m2): TOML profile system with schema versioning + migrations
(verdict READY)` from `git log`).

**Success criteria.** `docs/M3-verification.md` exists and ticks
all 22 items; a representative recording artefact pair
(`<basename>.flac` + `<basename>.flac.txt` +
`<basename>.flac.json`) is referenced from the doc; cross-links
from earlier plans updated.

**Test strategy.** Documentation phase; success is "every DoD
item resolves to evidence". Reviewer trace is the test.

**Estimated risk.** Low. If anything is missing it bounces back
into the relevant earlier phase, not this one.

## Risks / open questions

### 1. GStreamer + tokio interaction

`record_blocking` builds and tears down a tokio current-thread
runtime internally (`audio/recorder.rs:580-585`). The daemon
already has a tokio runtime; calling `record_blocking` from
inside one **panics** with "Cannot start a runtime from within a
runtime". Mitigation: wrap in `tokio::task::spawn_blocking`,
which moves it to the dedicated blocking thread pool — the inner
runtime sees a fresh, runtime-less thread (researcher § 8). If
M5+ promotes `record_blocking`'s inner runtime to multi-threaded,
the daemon may want to share its outer runtime instead; M5 review
trigger.

### 2. D-Bus signal lossiness if CLI subscribes after `StartRecording`

Real failure: the CLI calls `start_recording`, the reply arrives,
the daemon emits `StateChanged "starting"` from the same handler,
then the CLI installs `receive_state_changed()` — the "starting"
signal is gone. Mitigation in `commands/record.rs`: install all
three signal subscriptions **before** the `start_recording` call.
The proxy queues messages on the connection's read side; signals
arriving between `MatchRule` registration and the user's
`next().await` are buffered, not dropped (researcher § 5). For
extra safety the daemon defers the signal emission into the
spawned lifecycle task (so it fires *after* the reply has been
written) — Phase 3 sketch already does this. The Phase 5
lifecycle test is the regression net.

### 3. Daemon crash mid-recording — partial FLAC + no signal

If `zwhisperd` segfaults during a 30-minute recording, the FLAC
on disk is half-written (no STREAMINFO total samples), no
`RecordingComplete` fires, and the CLI hangs on `next().await`.
Mitigation: the CLI wraps signal waits in a wall-clock timeout
of `max_duration_minutes + 30 s`; on timeout, print "daemon went
away during recording; check
$XDG_STATE_HOME/zwhisper/zwhisperd.log" and exit 1. The `Drop`
impl on `Recorder` (`audio/recorder.rs`) already drains EOS and
finalises the FLAC for clean panics, but a *segfault* skips
drops. Document; do not pretend to solve in M3.

### 4. Auto-activation env var hand-off

When dbus-daemon spawns `zwhisperd` via the activation file, the
spawned process inherits dbus-daemon's environment, not the
user's session env. `XDG_RUNTIME_DIR` is usually inherited
correctly; `WAYLAND_DISPLAY` / `PIPEWIRE_RUNTIME_DIR` may be
missing. Mitigation: the activation file's
`SystemdService=zwhisperd.service` delegates startup to systemd,
which receives the full `graphical-session.target` env. Phase 5
verifies by killing the daemon and watching the next RPC
trigger `systemctl --user status zwhisperd` showing "Active:
active (running)". Hosts where `SystemdService=` is not honoured
(extremely old dbus-daemon < 1.10 — won't ship on Arch in 2026)
direct-spawn the daemon; document the manual `systemctl --user
start zwhisperd` workaround.

### 5. Concurrent CLI processes

Two `zwhisper record --profile meeting` calls land on the same
daemon. The second sees `RpcError::SessionInUse { existing:
<uuid> }`, prints "another session is active: <uuid>; stop it
with `zwhisper stop <id>` (M3.5/M4) or wait", exits 2. Mitigation
in M3: the error message includes the existing session id so the
user can `zwhisper stop <id>` once that command lands (the wire
shape already supports it via `Recorder1.StopRecording(s
session_id)`). Threat model: session bus is single-user, so any
local user process is trusted; M3 does not authenticate callers.

### 6. Profile reload semantics if user edits a TOML mid-session

User starts a 60-minute recording, then edits
`~/.config/zwhisper/profiles/meeting.toml` to a different model.
The in-flight session is **unaffected**: the daemon resolved
`meeting` at `StartRecording` time and stored a clone of the
`Profile` struct. The next `StartRecording` picks up the new
TOML (DoD #14 — no daemon-side cache). `Profiles1.Reload` is a
no-op: nothing to invalidate. M4 will switch to a daemon-side
cache for performance and `Reload` will mean "drop the cache";
wire format is already correct. Add a `tracing::info!` line on
every `StartRecording` that logs the resolved profile's path and
schema_version, so post-mortem of "why did my recording use the
old model" is straightforward.

### 7. systemd user unit + dbus auto-activation race on first install

Fresh install: `sudo install -m 644 dbus/cz.zajca.Zwhisper1.service
/usr/share/dbus-1/services/` and `sudo install -m 644
systemd/zwhisperd.service /usr/lib/systemd/user/`, then
`systemctl --user daemon-reload`. The first `zwhisper record`
RPC may race against the systemd unit's
`BusName=cz.zajca.Zwhisper1` gate: dbus-daemon sees the
activation file, asks systemd to start the unit, systemd starts
`zwhisperd`, which calls `RequestName`, which dbus-daemon was
already waiting for to satisfy the original RPC. This handshake
works on Arch / Fedora / Debian. Mitigation: Phase 5 manual
verification on a clean Arch VM (or the maintainer's host after
`systemctl --user stop zwhisperd && systemctl --user reset-failed
zwhisperd`); document the exact sequence in
`M3-verification.md`. If the race manifests as a hang on the
first RPC, the user-facing fix is `systemctl --user start
zwhisperd` once.

## Validation strategy

| Test layer | Coverage | Scope |
|---|---|---|
| Unit (per-crate) | `zwhisper-core`: existing M0/M1/M2 tests stay 155/155 green after Phase 1 move. `zwhisper-ipc`: 3+ tests (signature round-trip, `RpcError` mapping, proxy compile-fence). `zwhisperd`: `SessionManager` state machine (reserve/cancel/reserve, double-cancel, abort_active). `zwhisper-cli`: clap-parsing tests (M2 carry-over) + status-mapping helpers. | Pure logic; no bus, no GStreamer. |
| Integration (private bus) | `crates/zwhisperd/tests/rpc.rs` against `DbusFixture` private dbus-daemon: 9 tests in Phase 5 (bus name ownership, `GetStatus`, `Profiles1.List`, single-session policy, recording lifecycle signal sequence). | Real D-Bus, real daemon, no PipeWire (or runtime-skip on no-PipeWire hosts). |
| Live (PipeWire on maintainer's host) | `zwhisper record --profile meeting` 60-second run; FLAC validity (`flac -t`), transcript existence, exit code 0. Captured in `M3-verification.md`. | End-to-end. The "actually works" gate. |
| Manual D-Bus introspection | `busctl --user introspect cz.zajca.Zwhisper1 /cz/zajca/Zwhisper1` snapshot + `dbus-monitor` capture during a recording. | The wire format the IDEA contract describes is what shows up on the bus. |
| systemd unit verification | `systemd-analyze --user verify systemd/zwhisperd.service`. | Catches typos in `After=` / `BusName=` / `Type=` before install. |
| Lints | `cargo clippy --workspace --all-targets --all-features -- -D warnings`. | Style + correctness floor across all four crates. |
| `cargo audit` (informational) | Workspace deps after the zbus pin. | Heads-up on RUSTSEC, not a gate. |

What stays manual: D-Bus auto-activation through
`/usr/share/dbus-1/services/` (root install; one-shot test in the
verification doc); daemon SIGINT/SIGTERM mid-recording (`kill
-INT $(pidof zwhisperd)`, assert FLAC valid post-hoc); concurrent
CLI race (two terminals, second exits 2 with `SessionInUse`);
status UX hint when daemon is down.

## Stress-test corrections (devils-advocate, 2026-05-01)

After the initial draft, a stress-test pass surfaced 17 failure modes
(3 critical, 6 serious, 8 minor). The fixes below are **binding for
implementation** — they amend earlier sections.

### C1. `Recorder` API split — mandatory

`record_blocking` cannot be the daemon's entry point. Phase 3 must
expose a split API in `zwhisper-core::audio::recorder`:

- `pub fn Recorder::start(opts: RecordOptions) -> Result<Recorder, RecordingError>`
  — initialises the GStreamer pipeline, returns a handle owning
  `watch::Sender<StopReason>` and `RecordingReport` accumulator state.
- `pub fn Recorder::request_stop(&self, reason: StopReason)` — non-blocking
  signal into the `watch::Sender`. Idempotent.
- `pub fn Recorder::await_completion(self) -> Result<RecordingReport, RecordingError>`
  — blocking, drains EOS, returns the report.

The daemon's lifecycle task runs `let rec = Recorder::start(opts)?;` on
the tokio runtime, then calls
`spawn_blocking(move || rec.await_completion())`. The `oneshot`
receiver in `ActiveSession` is replaced with **direct ownership of
`Recorder` by the lifecycle task**; `StopRecording` calls
`session.recorder.request_stop(StopReason::UserRequested)` and then
joins the blocking handle. `record_blocking` is kept as a thin wrapper
for the existing CLI tests but is **not used by the daemon**.

### C2. No nested signal handlers in `record_blocking`

When `Recorder::start` runs inside `zwhisperd`, it must not register
`tokio::signal::ctrl_c()` internally. Phase 1 must:

- Add a `Recorder` constructor variant (or a config flag on
  `RecordOptions`) `install_ctrl_c: bool`. CLI code paths set it
  `true` for backward compat; daemon sets it `false`.
- Remove the inner `tokio::signal::ctrl_c` from `Recorder::start` /
  `await_completion` when `install_ctrl_c == false`. All stop signals
  flow exclusively through `request_stop`.
- Daemon's process-level SIGTERM/SIGINT handler (Phase 3) calls
  `session.recorder.request_stop(StopReason::UserRequested)` then
  awaits the lifecycle task's join.

POSIX allows only one signal handler per signal per process. With
this fix, only the daemon's outer handler registers SIGINT/SIGTERM.

### C3. `StateChanged "idle"` is the CLI's terminal signal

CLI's `record` command must NOT wait specifically for
`TranscriptComplete`. Phase 4 contract:

- After `StartRecording` returns, CLI loops on the merged signal
  stream (`StateChanged` ∪ `RecordingComplete` ∪ `TranscriptComplete`).
- On `RecordingComplete{session_id, audio_path}` matching our
  `session_id`: stash the audio path.
- On `TranscriptComplete{session_id, ...}` matching our `session_id`:
  stash the transcript path.
- On `StateChanged{new_state, session_id}` matching our `session_id`
  with `new_state == "idle"`: print stashed paths, exit `0`.
- On `StateChanged{new_state, session_id}` matching our `session_id`
  with `new_state == "failed"`: print whatever path arrived, exit `1`.

This eliminates the "transcribe failed silently → CLI hangs forever"
hazard. Daemon must always emit `StateChanged "idle"` after a
session terminates (success or transcribe-failure), before releasing
the session slot.

### C4. Signal session_id filtering — mandatory in CLI

Every signal handler in the CLI **must** compare
`signal.args()?.session_id` against the `session_id` returned by
`StartRecording`. Mismatched signals are dropped. This guards against
stale signals from an immediately-prior session leaking into a new
CLI process subscribing on the same bus.

### C5. Session slot release semantics

Phase 3 `SessionManager` releases the session slot **immediately
after `RecordingComplete` is emitted**, before the transcribe step
starts. Transcription runs without holding the slot; a second
`StartRecording` during transcription is allowed. Trade-off:
two concurrent `whisper-cli` subprocesses on slow machines, mitigated
by the OS scheduler. Documented in `zwhisper-ipc::Recorder1` rustdoc.

### C6. `Status.duration_ms` becomes unsigned

DoD #11 wire format changes from `(ssx)` → `(sst)`. `t` is unsigned
64-bit. Duration cannot be negative; use the unsigned type so the
client never has to defend against negative values. The signal
`StateChanged` and `Recorder1.GetStatus` both update.

### C7. Lazy GStreamer init in daemon

Phase 3 daemon does **not** call `gstreamer::init()` at startup.
Each `Recorder::start` call invokes a `OnceLock`-guarded
`gstreamer::init()`. Rationale: D-Bus auto-activation may start the
daemon before PipeWire is fully up; per-request init turns a
permanent-broken-daemon failure into a transient `RecordingFailed`
error. The CLI surface user-facing message reads "audio subsystem
unavailable; retry once PipeWire is ready".

### C8. RPC error name constant

`zwhisper-ipc` exports `pub const ERROR_NAME_PREFIX: &str =
"cz.zajca.Zwhisper1.Error.";` Both the daemon's `From<RpcError> for
zbus::fdo::Error` and the CLI's error-name extractor use this
constant. A unit test in `zwhisper-ipc` asserts `RpcError::SessionInUse`
round-trips through `fdo::Error` and back.

### C9. Signal ordering test

Phase 5 adds `recording_complete_arrives_before_state_changed_idle`:
spawns daemon against `DbusFixture`, drives a 1-s recording, asserts
the recorded signal sequence has `RecordingComplete` strictly before
the terminal `StateChanged "idle"`. Locks the lifecycle ordering in
the test suite so a future refactor cannot reorder them silently.

### C10. `DbusFixture` socket-readiness poll

Phase 5 fixture **must** poll `std::fs::metadata(socket_path)` after
spawning `dbus-daemon`, sleeping 20 ms between attempts, for up to
2 s before returning. Failure to find the socket within 2 s is a
hard test failure with a diagnostic message. Probes both
`/etc/dbus-1/session.conf` and `/usr/share/dbus-1/session.conf` for
the config; missing config emits a `cargo test` skip with a
diagnostic, not a panic. (Same skip discipline as M0/M1.)

### C11. `SetActive("")` rejects with explicit error

`SetActive` returns `RpcError::ProfileNotFound { name: "(empty)" }`
when the input is empty string. Documented in `zwhisper-ipc`
rustdoc and unit-tested.

### C12. List schema_version semantics

`Profiles1.List` returns `schema_version` **after migration** —
always equal to `CURRENT_SCHEMA_VERSION` for any successfully-loaded
profile. Documented in `zwhisper-ipc::ProfileEntry` rustdoc.
(M4-shaped property changes / signal notifications stay deferred.)

### Summary of binding amendments

| ID | Affects | Section originally affected |
|---|---|---|
| C1 | Phase 1 + 3 | "Concurrency model" / "Phase 3 lifecycle task" |
| C2 | Phase 1 + 3 | "GStreamer + tokio" |
| C3 | Phase 4 | "CLI exit-code map" |
| C4 | Phase 4 | "CLI exit-code map" |
| C5 | Phase 3 | "Single-session policy" |
| C6 | Phase 2 | "Public API rules → Status" |
| C7 | Phase 3 | "Phase 3 lifecycle task" |
| C8 | Phase 2 | "RpcError" |
| C9 | Phase 5 | "Tests" |
| C10 | Phase 5 | "Tests" |
| C11 | Phase 3 | "Profiles1.SetActive" |
| C12 | Phase 2 | "ProfileEntry" |

**The 17 failure modes are recorded in the session memory; the 12
binding fixes above are the actionable subset for the implementer.**

## Definition-of-done sign-off

M3 is closed only when `docs/M3-verification.md` is committed with
all 22 DoD items ticked, the `dbus-monitor` capture for one full
recording lifecycle is in the doc, and the maintainer's end-to-end
run (`zwhisper record --profile meeting`, 60 s mic + sink monitor)
produces a valid FLAC + transcript pair with daemon-emitted
signals visible in `$XDG_STATE_HOME/zwhisper/zwhisperd.log`. Bus
name `cz.zajca.Zwhisper1` and object path `/cz/zajca/Zwhisper1`
are the first locked D-Bus surface; subsequent additions go
through `Recorder2` / `Profiles2` rather than ad-hoc breaking
changes.
