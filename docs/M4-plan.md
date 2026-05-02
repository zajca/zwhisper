# M4 — Tray indicator (`zwhisper-tray`): implementation plan

> Target milestone from [IDEA.md § 11](../IDEA.md#11-roadmap). Adds a
> third process — `zwhisper-tray` — alongside the M3 daemon + CLI. The
> tray drives the daemon over the **frozen** M3 D-Bus contract and
> delivers session-bound sinks (clipboard + notifications) per
> IDEA.md § 5. M4 DoD verbatim from IDEA.md § 11: *"menu funkční,
> recording indicator viditelný; clipboard a notify deliver z tray,
> FileSink nezávislý."*
>
> Anchors: IDEA.md § 2 (architecture, IPC), § 5 (output destinations
> and delivery model), § 8 (only the M4-relevant *toggle* hotkey row —
> hotkey *binding* is M6, **not** M4), § 9 (tray systemd unit), § 11
> (M4 row).
>
> **Frozen wire surface (do NOT mutate in M4):**
> `crates/zwhisper-ipc/src/{recorder.rs,profiles.rs,types.rs,error.rs}`
> and the constants in `crates/zwhisper-ipc/src/lib.rs`. Any future
> contract widening goes through `Recorder2` / `Profiles2`.
>
> **Daemon-side behavior added (not wire-breaking):** the daemon gains
> a `~/.local/state/zwhisper/last-session.json` write step on
> `RecordingComplete` and `TranscriptComplete` (see § "C2 binding
> amendment"). Wire format is unchanged.

## Status snapshot (2026-05-02)

| Area | State | Evidence |
|---|---|---|
| `crates/zwhisper-tray/` exists | not done | `crates/` has `zwhisper-cli`, `zwhisper-core`, `zwhisper-ipc`, `zwhisperd` only |
| `ksni` workspace dep declared | not done | absent from `Cargo.toml:15-61` |
| `notify-rust` workspace dep declared | not done | absent from `Cargo.toml:15-61` |
| `arboard` workspace dep declared | not done | absent from `Cargo.toml:15-61` |
| `systemd/zwhisper-tray.service` | not done | only `systemd/zwhisperd.service` exists |
| Daemon writes `last-session.json` after lifecycle events | not done | `crates/zwhisperd/src/lifecycle.rs` emits signals only |
| `Recorder1.GetStatus()` returns `(state, active_profile, duration_ms)` | done | `crates/zwhisper-ipc/src/types.rs` |
| `Recorder1.StateChanged` / `RecordingComplete` / `TranscriptComplete` signal streams usable from a second client | done | proven by `crates/zwhisperd/tests/rpc.rs` (M3) |
| Daemon FileSink decoupled from tray | done | M3 lifecycle task in `crates/zwhisperd/src/recorder_service.rs` emits `RecordingComplete` after FLAC close; tray is not in the daemon code path |

**Verdict.** M4 is a greenfield client crate that consumes the
already-frozen `Recorder1` + `Profiles1` proxies. The daemon stays
behaviorally compatible — the only addition is a state-file write
step that uses the same metadata the daemon already has in memory.
No business logic moves out of `zwhisper-core`.

## Definition of done

Each item below is a testable assertion. Items 1–6 mirror the IDEA.md
§ 11 verbatim DoD; items 7–24 lock in the architectural decisions
distilled from `m4-architecture.md` + the C1–C3 stress-test
corrections.

1. `crates/zwhisper-tray/` builds clean as part of `cargo build
   --workspace`, ships a binary `zwhisper-tray`, and `cargo clippy
   --workspace --all-targets -- -D warnings` is clean.
2. SNI tray icon is visible on KDE Plasma 6 with **state-driven
   appearance**: `Idle | Recording | Stopping | Failed` map to four
   distinct icons (`Starting` may share an icon with `Stopping`).
3. Right-click menu shows: state header (disabled label), Start
   recording, Stop recording, Profiles submenu (radio list of
   profiles, active highlighted), Open last recording, Open last
   transcript, Quit. Items enabled/disabled per state per § "Menu
   structure".
4. Tooltip text: `"zwhisper — {state} · profile: {active_profile}"`,
   appended with `" · MM:SS"` only while recording (1 Hz tick).
5. On `TranscriptComplete`: clipboard receives transcript text;
   notification fires with action mapping to `xdg-open
   <transcript_path>`.
6. **Daemon FileSink keeps working when tray is not running** —
   `systemctl --user stop zwhisper-tray && zwhisper-cli record
   --profile default …` produces FLAC + `.txt` + `.json` on disk and
   exits 0.
7. **Late-start invariant**: kill the tray, run a recording to
   completion, restart the tray; no clipboard write, no notification;
   menu shows correct "Open last recording" / "Open last transcript"
   entries pointing to the just-completed files. Implemented via
   `last-session.json` (see C2).
8. **`RecordingComplete` does NOT trigger sinks.** Only
   `TranscriptComplete` does. Verified by a test that records a
   profile with `transcription.auto = false`: tray stays silent, no
   clipboard, no notify.
9. **Wayland clipboard persists across the user paste action.** The
   `arboard::Clipboard` object is held in
   `Arc<Mutex<Option<arboard::Clipboard>>>` for the tray's lifetime
   (see C1). Verified by an integration test (or manual: paste 5 s
   after notification, content is intact).
10. **`systemd/zwhisper-tray.service`** ships, `Type=simple`,
    `After=graphical-session.target`, `Restart=on-failure`,
    `RestartSec=2`. Not enabled by default; install instructions tell
    the user to run `systemctl --user enable zwhisper-tray`.
11. **D-Bus auto-activation** of `zwhisperd` from a tray-side method
    call is verified to work end-to-end on a clean session. The
    existing `dbus/cz.zajca.Zwhisper1.service` covers this; no change
    required.
12. **Single-instance enforcement** via `cz.zajca.Zwhisper1.Tray` bus
    name claim. Second invocation logs a clear error and exits 0.
13. **M3 `Recorder1` / `Profiles1` wire format unchanged.** Out-of-band
    proposals (`Recorder2.GetLastCompletedSession`,
    `Profiles2.ProfilesChanged`) live in § "Open contract asks" and do
    NOT ship in M4.
14. **Crate dependency graph**: `zwhisper-tray` → `zwhisper-ipc` +
    `zwhisper-core` (read-only access to `RecorderState::Display` and
    `SessionId`; no `gstreamer`, no `transcribe`, no profile loader I/O
    on the tray hot path) + `ksni` + `notify-rust` + `arboard` +
    `tokio` + `zbus` + `futures-util` + `tracing*` + `color-eyre` +
    `thiserror`. No dep on `zwhisperd`. No dep on `zwhisper-cli`.
15. **Threading model**: single tokio runtime owns the D-Bus
    connection, signal pump, sink dispatcher, and tray-command
    dispatcher. ksni runs on its own thread (per `ksni::TrayService::spawn`)
    and communicates with tokio via `tokio::sync::watch<TrayState>`
    (state out) and `tokio::sync::mpsc<TrayCmd>` (menu actions in).
    ksni handlers MUST never block on RPC.
16. **ksni thread panic ⇒ process exit 1** so systemd
    `Restart=on-failure` recovers (see C3). A supervisor task awaits
    the `TrayService::spawn` handle.
17. **Daemon liveness watch**: tray subscribes to
    `org.freedesktop.DBus.NameOwnerChanged` for `cz.zajca.Zwhisper1`.
    On owner-disappeared the tray transitions to a synthetic
    `DaemonOffline` state (icon: error), and triggers reconnect on
    next owner-appeared. No periodic `GetStatus` poll.
18. **`Sink` trait** with two implementations (`ClipboardSink`,
    `NotificationSink`). Sink dispatcher runs both per
    `TranscriptComplete`, with clipboard-first ordering. Failure in
    one sink does NOT abort the other; if clipboard fails, the
    notification body changes to communicate the artefact path.
19. **Clipboard size guard**: if `bytes > ZWHISPER_TRAY_CLIPBOARD_MAX`
    (default 512 KB, configurable via env var), skip clipboard write
    and surface a notification "Transcript too large for clipboard
    (N MB). Open file to read." with `xdg-open` action.
20. **Profile submenu disabled when `state != Idle`**: prevents the
    confusing "SetActive succeeds, current recording unaffected"
    UX (M3 lock-in: `SetActive` is a hint for *future* sessions). The
    submenu re-enables on `StateChanged "idle"`.
21. **Optimistic action lock**: on menu Start/Stop click the tray sets
    a `pending_cmd` flag in `TrayState` that disables the action menu
    items until the next `StateChanged`. Prevents double-click double
    RPC (see L6).
22. **Daemon-side `last-session.json`**: written under
    `$XDG_STATE_HOME/zwhisper/last-session.json` (default
    `~/.local/state/zwhisper/`) with `File::sync_all()` BEFORE the
    daemon emits the corresponding signal. Two-phase write: after
    `RecordingComplete` with `transcript_path: null`, again after
    `TranscriptComplete` with both paths populated. Permissions
    `0600` (mirrors FileSink invariant in IDEA.md § 5).
23. **`NotificationSink` is non-blocking**: uses
    `notify-rust::show()` (non-blocking) and registers a
    `org.freedesktop.Notifications.ActionInvoked` D-Bus signal
    listener for the returned notification id. No accumulating
    `spawn_blocking` per notification (see M3-stress-test).
24. `docs/M4-verification.md` ticks all of the above with file:line
    evidence (test name, log line, manual screenshot, `dbus-monitor`
    capture). Verdict line "M4 closes …" only after all 24 are
    ticked.

## Out of scope (deferred to M5+)

- **`Recorder2.GetLastCompletedSession()`** — race-free server-side
  query. M4 ships state-file (b) per OQ-1; (c) is the long-term
  answer in M5+.
- **`Profiles2.ProfilesChanged` signal** — would replace timer-driven
  profile-list refresh. Deferred to M5+ contract bump.
- **Type-at-cursor sink** — IDEA.md § 5 R&D queue.
- **Cloud backends** — M5.
- **Hotkey binding** (xdg-desktop-portal GlobalShortcuts) — M6. M4
  ships only the *toggle* daemon side; the binding mechanism is M6.
- **Settings GUI** (FLTK profile editor, model downloader) — M7.
- **systemd unit hardening directives** (`ProtectSystem=strict`,
  `PrivateNetwork=yes`, etc.) — must be tested incrementally
  alongside packaging in M8.
- **`Tray1` D-Bus interface** (`ShowMenu`, `Reset`) — M4 registers the
  bus name as a presence claim only; full server interface deferred
  to M7.
- **GNOME AppIndicator-extension auto-detection / hint notification** —
  documented in `docs/M4-verification.md` as a known caveat. M4 does
  NOT ship the hint-notification code path; it is a follow-up.
- **`Profiles1.Reload` server-side reload behavior** — M3 stub stays
  in place (M3-plan DoD item 14). Tray refreshes its own profile
  cache on a 60 s timer + after every `SetActive`.
- **`zwhisper-cli output last --to clipboard|notify`** — IDEA.md § 5
  CLI sink proxies. The `Sink` trait designed in M4 is reusable but
  the CLI wiring is M5+.

## Architecture for M4

### ASCII diagram

```
                      ┌───────────────────────────────────────┐
                      │       D-Bus session bus               │
                      │   cz.zajca.Zwhisper1                  │
                      │   cz.zajca.Zwhisper1.Tray  (M4 NEW)   │
                      └───────────────────────────────────────┘
                            ▲                       ▲
                            │ Recorder1Proxy        │ #[interface]
                            │ Profiles1Proxy        │ Recorder1 / Profiles1
                            │ + signal streams      │ + signal emit
                            │ + NameOwnerChanged    │ + last-session.json write
                            │                       │
   ┌────────────────────────┴─────────┐   ┌─────────┴──────────────────────────┐
   │  zwhisper-tray (M4, NEW)         │   │  zwhisperd (M3 + tiny M4 add)      │
   │                                  │   │                                    │
   │  ┌────────────┐  ┌────────────┐  │   │  GStreamer capture                 │
   │  │ ksni event │  │ async tokio│  │   │  Profile manager                   │
   │  │ thread     │◄─┤ runtime    │  │   │  Transcribe orchestrator           │
   │  │ (Tray impl)│  │ (zbus,     │  │   │  FileSink (audio + .txt + .json)   │
   │  └─────┬──────┘  │ signals,   │  │   │  last-session.json writer (NEW M4) │
   │        │ menu    │ sinks)     │  │   └────────────────────────────────────┘
   │        ▼ events  └─────┬──────┘  │
   │  Tokio command channel │         │   ┌────────────────────────────────────┐
   │  (mpsc<TrayCmd>)       │         │   │  zwhisper-cli (M3, unchanged)      │
   │        │               │         │   │  start / stop / status / record    │
   │        ▼               ▼         │   └────────────────────────────────────┘
   │  RPC dispatch      Sink layer    │
   │  Start/Stop/SetA   ClipboardSink │   ┌────────────────────────────────────┐
   │  (with pending_cmd)NotifySink    │──►│  org.freedesktop.Notifications     │
   └──────────────────────────────────┘   │  (notification daemon)             │
                            │             └────────────────────────────────────┘
                            ▼
                      arboard library
                      (Wayland or X11, autodetect)
```

### Public API rules (M4 lock-ins)

1. **Wire format frozen.** `crates/zwhisper-ipc/` is read-only. Tray
   uses the M3 proxies as-is.
2. **Daemon-side state file: contract is internal but stable.**
   `~/.local/state/zwhisper/last-session.json` schema:
   ```json
   {
     "schema_version": 1,
     "session_id": "uuid-v4",
     "audio_path": "/abs/path/to/recording.flac",
     "transcript_path": null,           // or absolute path
     "backend": "whisper-cli",          // empty when transcript null
     "completed_at_unix_ms": 1714660800000
   }
   ```
   Tray reads this on startup. Schema versioning matches the M2
   profile schema-versioning approach: future bumps go through
   migrations, not silent format changes.
3. **Sink trait stays inside `zwhisper-tray` for M4.** Lifting it to a
   shared crate (e.g. `zwhisper-sinks`) is M5+.
4. **No tray-side `Tray1` D-Bus server interface** in M4. The bus
   name `cz.zajca.Zwhisper1.Tray` is a presence claim only.
5. **CLI ↔ tray isolation.** Both processes are independent D-Bus
   clients of the same daemon (IDEA.md § 5: *"Žádné CLI volání
   nesahá do tray procesu."*). Verified by `cargo tree -p
   zwhisper-tray | grep zwhisper-cli` → empty and `cargo tree -p
   zwhisper-cli | grep zwhisper-tray` → empty.

### Threading model (locked in)

```
┌──────────── tokio runtime (multi-thread) ────────────┐
│                                                      │
│  Task A: D-Bus connection bootstrap                  │
│          - ConnectionBuilder → claim                 │
│            cz.zajca.Zwhisper1.Tray (single-instance) │
│          - Construct Recorder1Proxy + Profiles1Proxy │
│                                                      │
│  Task B: Signal pump (reconnect-aware)               │
│          - Subscribe FIRST: StateChanged,            │
│            RecordingComplete, TranscriptComplete,    │
│            NameOwnerChanged.                         │
│          - THEN snapshot: GetStatus + List + GetActive│
│            + read last-session.json.                 │
│          - On NameOwnerChanged(new_owner == ""):     │
│            transition state → DaemonOffline, await   │
│            owner-appeared, re-snapshot.              │
│          - On stream end / connection error: backoff │
│            (250 ms, 500 ms, 1 s, 2 s, 5 s cap),      │
│            reconnect, re-subscribe, re-snapshot.     │
│                                                      │
│  Task C: Sink dispatcher                             │
│          - Consumes TranscriptComplete events        │
│          - Reads transcript file ONCE                │
│          - Calls ClipboardSink, NotificationSink     │
│          - Holds Arc<Mutex<Option<arboard::Clipboard>>>│
│            for the tray's entire lifetime (C1)       │
│                                                      │
│  Task D: TrayCmd dispatcher                          │
│          - Consumes mpsc<TrayCmd> from ksni handlers │
│          - Executes RPC on Recorder1Proxy /          │
│            Profiles1Proxy                            │
│          - Sets/clears pending_cmd in TrayState      │
│                                                      │
│  Task E: ksni-thread supervisor (C3)                 │
│          - Awaits TrayService::spawn handle          │
│          - Err → process::exit(1); Ok → exit(0)      │
│                                                      │
│  Task F: Tooltip ticker                              │
│          - Single tokio::time::interval(1 s)         │
│          - On each tick: if state == Recording,      │
│            push current TrayState to watch tx        │
│          - Never cancelled / re-created (L1 fix)     │
│                                                      │
└──────────────────────────────────────────────────────┘
                         ▲
                         │ mpsc<TrayCmd>
                         │ + watch<TrayState>
                         │
┌──────── ksni service thread (owned by ksni) ────────┐
│                                                     │
│  - Renders icon + menu from latest watch<TrayState> │
│  - Menu activations: try_send(TrayCmd::*) into mpsc │
│  - Never blocks on RPC                              │
│                                                     │
└─────────────────────────────────────────────────────┘
```

### Sink trait (locked-in shape)

```rust
// crates/zwhisper-tray/src/sink/mod.rs (M4)
#[async_trait]
pub trait Sink: Send + Sync {
    fn id(&self) -> &'static str;

    /// Deliver one transcript. Caller (sink dispatcher) reads the
    /// file once and shares the buffer between sinks.
    async fn deliver(&self, ctx: &SinkContext<'_>) -> Result<(), SinkError>;
}

pub struct SinkContext<'a> {
    pub session_id: &'a str,
    pub transcript_path: &'a Path,
    pub transcript_text: &'a str,
    pub bytes: u64,
    pub backend: &'a str,
}
```

### State machine: `RecorderState` → icon + menu state

```
state           │ icon          │ Start │ Stop  │ Profiles ► │ Open last │
────────────────┼───────────────┼───────┼───────┼────────────┼───────────┤
DaemonOffline   │ error         │  ✕    │  ✕    │     ✕      │     ✓     │
Idle            │ idle          │  ✓    │  ✕    │     ✓      │     ✓     │
Starting        │ busy          │  ✕    │  ✕    │     ✕      │     ✓     │
Recording       │ recording     │  ✕    │  ✓    │     ✕      │     ✓     │
Stopping        │ busy          │  ✕    │  ✕    │     ✕      │     ✓     │
Failed          │ error         │  ✓    │  ✕    │     ✓      │     ✓     │
```

### Late-start handling (per IDEA.md § 5 row 3)

Three cases the tray MUST handle:

| # | Sequence | Required behaviour |
|---|---|---|
| 1 | Daemon emits `TranscriptComplete`, **then** tray starts | No clipboard, no notify. "Open last recording" / "Open last transcript" point at the just-completed files (read from `last-session.json`). |
| 2 | Tray crashes mid-recording, daemon completes the session, then tray restarts | Same as #1. |
| 3 | Tray restarts **during** recording (state == `Recording`) | Tooltip + icon reflect Recording. When the in-progress session completes after restart, **clipboard + notify fire** (we are alive when the signal lands). |

The state file approach (b) per OQ-1 is the M4 mechanism. The
race-free server-side query (`Recorder2.GetLastCompletedSession`) is
deferred to M5+.

## Stress-test corrections (devils-advocate, 2026-05-02)

C1–C3 are binding amendments distilled from `m4-stress-test.md`.
They are reflected in the Definition of done and in the Phased
breakdown below.

### C1 (M4). `arboard` `Clipboard` object owned for the tray process lifetime — mandatory

**Trigger.** Wayland clipboard ownership is held by the writing
client. If `arboard::Clipboard` is dropped after `set_text`, the
selection dies and any subsequent paste yields empty content. The
naive `spawn_blocking { let cb = Clipboard::new(); cb.set_text(…) }`
pattern silently breaks the M4 primary user story (paste transcript
into editor).

**Lock-in.** `ClipboardSink` MUST hold one
`Arc<tokio::sync::Mutex<Option<arboard::Clipboard>>>` initialised on
first use and kept alive until process exit. `set_text` is called on
the existing object. If `Clipboard::new()` fails at first use, log
WARN and surface a fallback notification ("Clipboard unavailable,
transcript at <path>") — do NOT panic, do NOT propagate as fatal.

**Crate decision.** `arboard` is the binding choice (over
`wl-clipboard-rs` and over the subprocess `wl-copy`/`xclip` route).
Reasoning: `arboard` autodetects Wayland vs X11; no subprocess env
hand-off concerns under systemd-user; one crate covers both
backends.

**Test.** Manual on KDE Plasma 6: trigger a transcribe, switch to a
text editor, sleep 5 s, paste — content present. Automated unit test:
mock `Clipboard` trait inside `ClipboardSink`, assert `set_text` is
called on the persistent handle (not on a fresh handle per call).

### C2 (M4). Daemon `sync_all()` `last-session.json` BEFORE emitting the signal — mandatory

**Trigger.** If the daemon emits `TranscriptComplete` on D-Bus before
the `last-session.json` write hits disk (kernel page cache flush
delay, NFS home, btrfs nodatacow), a freshly-started tray that does
its bootstrap snapshot in the signal-delivery window reads stale or
empty data. DoD item 7 (late-start) cannot be guaranteed without this
ordering.

**Lock-in.** In the daemon lifecycle task, the order is:
1. Write `last-session.json` (atomic-replace via `tempfile + rename`
   pattern; mode 0600).
2. `File::sync_all()` on the new file handle.
3. Then emit `RecordingComplete` (or `TranscriptComplete`).

Two-phase write: after `RecordingComplete` with `transcript_path:
null`, again after `TranscriptComplete` with both paths populated.
The audio file is therefore discoverable from "Open last recording"
even when transcription crashes (covers L4).

**Test.** Integration test in `crates/zwhisperd/tests/`: subscribe to
`TranscriptComplete`, on receipt immediately read and parse
`last-session.json`, assert `session_id` matches and `transcript_path`
is populated.

### C3 (M4). ksni thread panic MUST exit the process — mandatory

**Trigger.** `ksni::TrayService::spawn()` returns a handle to an
internal SNI watcher thread. If that thread panics (D-Bus SNI watcher
disconnected, malformed icon, path collision), the thread dies but
the tokio runtime keeps running. systemd reports `active (running)`,
the icon disappears from the panel, and clipboard + notify silently
break. No recovery without manual restart.

**Lock-in.** A tokio supervisor task awaits the `TrayService::spawn`
handle. On `Err(_)`: log error and call `std::process::exit(1)` so
systemd `Restart=on-failure` recovers. On `Ok(())` (clean Quit menu
action): `std::process::exit(0)`.

**Test.** Unit-level: inject a stub handle that immediately resolves
to `Err(...)`; assert process exits with code 1 within 500 ms (run
under a dedicated test process via `assert_cmd`).

### Summary of binding amendments

| ID | Area | Lock-in |
|---|---|---|
| C1 | Clipboard | `arboard` long-lived handle; `wl-clipboard-rs` and subprocess routes rejected |
| C2 | Daemon state file | atomic-write + `sync_all()` before signal emission, two-phase write |
| C3 | ksni supervision | supervisor task awaits handle; panic ⇒ `exit(1)` |

## Phased breakdown

Phase 0 lands first; Phase 7 ships M4. Each phase is one PR-sized
commit set, sequential. Phase numbering matches the granularity of
the M3 plan to make review parallels obvious.

### Phase 0 — Workspace bootstrap (~1 h)

**Files touched.**
- `Cargo.toml` (workspace) — add deps: `ksni = "0.3"`,
  `notify-rust = "4.11"`, `arboard = { version = "3.4", features =
  ["wayland-data-control"] }`, `xdg = "2.5"` (state-file path
  resolution).
- `crates/zwhisper-tray/Cargo.toml` — new crate manifest.
- `crates/zwhisper-tray/src/main.rs` — minimal binary that prints
  version + git-sha and exits 0.

**Test.** `cargo build --workspace`, `cargo run -p zwhisper-tray --
--version` prints something.

**Risk.** Low. ksni 0.3 is documented to compile on Linux only — the
crate is `cfg(target_os = "linux")` gated, mirroring `zwhisperd`.

### Phase 1 — Daemon-side `last-session.json` writer (~3 h)

**Files touched.**
- `crates/zwhisperd/src/lifecycle.rs` — add a `last_session::write_*`
  helper invoked after `RecordingComplete` and `TranscriptComplete`.
- `crates/zwhisperd/src/last_session.rs` (new) — schema struct,
  atomic-write (`tempfile + rename`), `sync_all` on the file handle.
- `crates/zwhisperd/tests/last_session.rs` (new) — integration test
  per C2.

**Tests.**
- Unit test: schema serialization round-trip.
- Integration test (with `DbusFixture`): record + transcribe a
  fixture wav; subscribe to `TranscriptComplete`; on receipt parse
  `last-session.json`, assert paths match.
- Test: kill daemon between `RecordingComplete` and
  `TranscriptComplete`; restart daemon; `last-session.json` shows the
  audio path with `transcript_path: null` (covers L4).

**Risk.** Medium. The lifecycle.rs ordering is delicate (M3 already
spent two post-ship fixes there). Atomic-write must NOT race with
the in-flight signal emit code path. The pattern: write to tempfile
in same dir → `sync_all` → atomic rename → THEN emit signal.

**Definition of done.** All three tests pass. `clippy
-D warnings` clean.

### Phase 2 — `zwhisper-tray` D-Bus signal pump (~5 h)

**Files touched.**
- `crates/zwhisper-tray/src/dbus/mod.rs` (new) — connection bootstrap,
  `Recorder1Proxy` + `Profiles1Proxy` + `NameOwnerChanged` signal.
- `crates/zwhisper-tray/src/state.rs` (new) — `TrayState`,
  `LastCompleted`, `Sink` types (no impls yet).
- `crates/zwhisper-tray/src/pump.rs` (new) — Task B implementation:
  subscribe-then-snapshot, reconnect-with-backoff, NameOwnerChanged
  handling.
- `crates/zwhisper-tray/tests/pump.rs` (new) — integration tests
  reusing the daemon's `DbusFixture` (lift via dev-dependency or
  promote to a shared `zwhisperd-test-support` crate — see § "Open
  questions resolved").

**Tests.**
- Subscribe-then-snapshot races: signal fired during snapshot is not
  lost (`TrayState` reflects it).
- Daemon SIGKILL → `TrayState` transitions to `DaemonOffline` within
  3 s.
- Daemon restart → tray re-subscribes, `TrayState` re-seeds.
- `last-session.json` populated by Phase 1: tray bootstrap reads it
  and sets `last_session` on `TrayState`.

**Risk.** Medium. zbus signal-stream semantics around dead services
(H5) need careful test coverage.

### Phase 3 — ksni tray skeleton (~4 h)

**Files touched.**
- `crates/zwhisper-tray/src/tray.rs` (new) — `Tray` impl driving icon
  + tooltip + dummy menu from `watch<TrayState>`.
- `crates/zwhisper-tray/src/icon.rs` (new) — `RecorderState` → icon
  enum mapper.
- `crates/zwhisper-tray/data/icons/` (new) — `zwhisper-{idle,
  recording, busy, error}.svg` (or PNG fallback).
- `crates/zwhisper-tray/src/main.rs` — wire pump (Phase 2) + tray.
- `crates/zwhisper-tray/src/supervisor.rs` (new) — Task E (C3).

**Tests.**
- Pure function: `RecorderState` → icon enum mapping, table test.
- Pure function: tooltip formatter (state + profile + duration), table test.
- C3 supervisor unit test (assert process exit on injected ksni
  panic).
- Manual: launch `cargo run -p zwhisper-tray`, observe icon on KDE
  Plasma 6 status area; observe tooltip updates as daemon state
  changes.

**Risk.** Medium. ksni quirks (icon update timing, menu rendering)
require manual verification on the maintainer's machine.

### Phase 4 — Menu structure + RPC dispatch (~4 h)

**Files touched.**
- `crates/zwhisper-tray/src/menu.rs` (new) — menu builder consuming
  `TrayState`, emitting ksni `MenuItem` tree.
- `crates/zwhisper-tray/src/cmd.rs` (new) — `TrayCmd` enum + Task D
  RPC dispatcher.
- `crates/zwhisper-tray/src/state.rs` — extend `TrayState` with
  `pending_cmd: Option<PendingCmd>` (locks Start/Stop while RPC
  in-flight, per L6).

**Tests.**
- Menu builder: given `TrayState`, assert menu tree (per state matrix).
- Profiles submenu disabled when `state != Idle` (M1 fix).
- pending_cmd lock: simulate Start click → `try_send` succeeds, menu
  re-renders with Start disabled until next `StateChanged`.
- Manual: click Start in menu → daemon starts recording; click Stop →
  daemon stops; switch profile → `SetActive` fires.

**Risk.** Low.

### Phase 5 — Sink layer (~5 h)

**Files touched.**
- `crates/zwhisper-tray/src/sink/mod.rs` (new) — `Sink` trait,
  `SinkContext`, `SinkError`, dispatcher.
- `crates/zwhisper-tray/src/sink/clipboard.rs` (new) —
  `ClipboardSink` with persistent `arboard::Clipboard` handle (C1).
- `crates/zwhisper-tray/src/sink/notification.rs` (new) —
  `NotificationSink` using non-blocking `notify-rust::show()` +
  `ActionInvoked` D-Bus subscription (DoD #23).
- `crates/zwhisper-tray/src/sink/dispatch.rs` (new) — Task C; on
  `TranscriptComplete` reads file, applies size guard (DoD #19),
  calls sinks in order.

**Tests.**
- C1 unit test: stub `Clipboard` trait, verify `set_text` is called
  on the same handle across multiple deliveries.
- Size guard: 1 MB transcript → clipboard skipped, notification body
  contains "too large".
- TOCTOU: file deleted between signal and read → notification body
  contains "deleted before…", clipboard skipped.
- Clipboard fail → notification body changes to convey artefact path.
- Manual on KDE Plasma 6: trigger a real transcribe; paste 5 s after
  notification (C1 verification); click "Open in editor" notification
  action.

**Risk.** Medium. Wayland/X11 detection edge cases (mitigated by
relying on `arboard`'s autodetection per L5).

### Phase 6 — Single-instance + systemd unit (~3 h)

**Files touched.**
- `crates/zwhisper-tray/src/single_instance.rs` (new) — claim
  `cz.zajca.Zwhisper1.Tray`; on collision, log + `exit(0)`.
- `systemd/zwhisper-tray.service` (new) — `Type=simple`,
  `After=graphical-session.target` (NOT `Requisite=`, per M4 fix),
  `Restart=on-failure`, `RestartSec=2`.
- `crates/zwhisper-tray/src/main.rs` — startup check: verify
  `WAYLAND_DISPLAY` or `DISPLAY` is set, `exit(1)` with clear error
  if neither.

**Tests.**
- Two tray processes started simultaneously → second exits 0 with
  log line "tray already running".
- Tray launched without `WAYLAND_DISPLAY` and `DISPLAY` → exits 1
  with clear stderr.

**Risk.** Low. systemd unit verification is manual on a real session
(documented in `docs/M4-verification.md`).

### Phase 7 — Verification doc + commit (~2 h)

**Files touched.**
- `docs/M4-verification.md` (new) — walks all 24 DoD items with
  file:line evidence (test name, log line, manual screenshot).
- `README.md` — add an M4 paragraph and the manual-enable command.
- `Cargo.lock` — committed.

**Test.** All `cargo build`, `cargo test --workspace`, `cargo clippy
--workspace --all-targets -- -D warnings`, `cargo fmt --check` clean.

**Definition of done for the milestone.** All 24 DoD items ticked in
`docs/M4-verification.md`; product-engineer issues READY verdict.

## Risks / open questions

### 1. KDE Plasma 6 SNI behavior under fractional-scaling

ksni icons may render blurry on fractional HiDPI (1.5×, 1.75×). M4
documents this as a known caveat in `docs/M4-verification.md` if
observed. Mitigation: ship vector SVG icons; ksni's `icon_pixmap`
path supports multiple resolutions.

### 2. GNOME requires AppIndicator extension

GNOME 47+ does not natively show SNI items; users must install the
`appindicator-support` extension. M4 documents this in
`README.md` and `docs/M4-verification.md` but does NOT auto-detect or
warn at runtime (deferred per "Out of scope").

### 3. wlroots `graphical-session.target` activation

Sway/Hyprland users launching from TTY without a display manager
have to manually start `graphical-session.target`. M4 unit uses
`After=graphical-session.target` (not `Requisite=`) so the unit runs
even without the target active — at the cost of being startable on
non-graphical sessions. The startup `WAYLAND_DISPLAY` / `DISPLAY`
check (DoD-adjacent) prevents that anti-case.

### 4. zbus `RequestName` error variant naming (M5 stress-test M5)

The exact error variant for "name already taken" in zbus 5.15.0
needs runtime verification. Phase 6 implementation tests this
empirically; if no typed variant exists, fall back to error-string
match.

### 5. Last-session.json daemon contract change

Adding `last-session.json` is an internal contract change in
`zwhisperd` but does NOT touch the wire format. M5+ may extend the
schema (add fields); migration is straightforward via
`schema_version`.

### 6. `Profiles1.Reload` is still a stub

M3-plan locked `Reload` as a no-op stub. M4 does not change that —
the tray refreshes its own cached profile list every 60 s and after
every successful `SetActive`. A "ProfilesChanged" signal is the
right long-term answer (OQ-4) but stays out of M4 scope.

### 7. notify-rust `ActionInvoked` D-Bus signal subscription

The architecture document mentions the non-blocking pattern; concrete
implementation needs to subscribe to
`org.freedesktop.Notifications.ActionInvoked` filtered by the
notification id returned from `show()`. This is a Phase 5 detail;
budgeted within the 5 h estimate.

### 8. TOCTOU on transcript file

Phase 5 sink dispatcher reads the file after signal receipt. If the
user deletes the file in between, the dispatcher emits a fallback
notification with the path (M6 fix). Same path for "transcript file
moved" (FileSink retention purge race).

## Validation strategy

| Layer | Approach |
|---|---|
| Unit tests | `RecorderState→icon` mapper, tooltip formatter, menu builder, sink dispatch ordering, `last-session.json` round-trip — all pure functions, table tests |
| Integration tests | `DbusFixture` (M3) reused for: signal pump reconnect, late-start, daemon-offline transition. Lift `crates/zwhisperd/tests/common/mod.rs` to a `zwhisperd-test-support` crate so `zwhisper-tray` can dev-depend on it without circular deps |
| Manual verification | Real KDE Plasma 6: icon visibility, menu interactions, clipboard paste 5 s after notification (C1), notification action invocation. Documented in `docs/M4-verification.md` with screenshots |
| Daemon-FileSink-without-tray | `systemctl --user stop zwhisper-tray && zwhisper-cli record …` produces FLAC + transcript on disk, exits 0 |
| Single-instance | Two `zwhisper-tray` processes launched in parallel → second exits 0 cleanly |
| systemd | `systemctl --user enable && start zwhisper-tray.service` runs cleanly on a fresh session; `systemctl --user status` shows `active (running)`; killing the binary causes `Restart=on-failure` to bring it back within `RestartSec=2` |

## Open contract asks (logged for M5+)

1. **`Recorder2.GetLastCompletedSession()`** — race-free server-side
   query. Replaces the state-file approach (b) with the long-term
   answer (c).
2. **`Profiles2.ProfilesChanged` signal** — replaces timer-driven
   profile-list refresh in the tray and CLI.
3. **`Tray1` server interface** — `ShowMenu`, `Reset` actions for
   external triggers (e.g., a future settings GUI calling into the
   running tray).

All three are pure additions through new versioned interfaces — no
breakage of the M3 `Recorder1` / `Profiles1` surface.
