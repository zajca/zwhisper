# M6 — Hotkey toggle (xdg-desktop-portal GlobalShortcuts): implementation plan

> Target milestone from [IDEA.md § 11](../IDEA.md#11-roadmap) row M6
> *"Hotkey toggle (portal) — KDE Plasma 6 funguje; GNOME a wlroots
> empirically tested, dokumentovaný stav"*. Adds a system-wide
> chord-to-toggle path on top of the M3 `Recorder1` D-Bus surface
> (frozen) by linking a new lib crate (`zwhisper-hotkey`) into the
> tray and the CLI.
>
> Anchors: IDEA.md § 11 (roadmap row M6), § 12 ("xdg-desktop-portal
> GlobalShortcuts (M6)") and the IDEA.md § 1 risk row "Global hotkeys
> přes xdg-desktop-portal" (line 25). Builds on the frozen contracts
> of M3 (`Recorder1` / `Profiles1` wire format), M4 (tray + ksni +
> single-instance bus claim), and M5 (`☁` marker rendering hook on
> menu rows — unrelated, kept for context).
>
> **Frozen wire surface (do NOT mutate in M6):**
> `crates/zwhisper-ipc/src/{recorder.rs,types.rs,error.rs}` —
> verbatim. M6 ships **no new D-Bus methods, no new D-Bus signals,
> no new D-Bus signature changes**. The only daemon-visible effect
> of M6 is that **more callers** dial `Recorder1.StartRecording` /
> `StopRecording` — the daemon does not even know it is being
> toggled by a hotkey. See § "Wire-surface contract" for the locked
> "frozen" / "additive" split.
>
> **Internal additive surface (allowed in M6):**
> - new lib crate `crates/zwhisper-hotkey/` (Linux-only),
> - new tray field `TrayState::hotkey: HotkeyMenuState`,
> - new tray task `run_hotkey`,
> - new CLI subcommands `zwhisper toggle` and `zwhisper hotkey {…}`,
> - new optional config file `~/.config/zwhisper/hotkey.toml`
>   (debounce, post-stop cooldown, auto-bind-on-startup),
> - new `packaging/zwhisper.desktop` for the portal app-id.

## Status snapshot (2026-05-02)

| Area | State | Evidence |
|---|---|---|
| `Recorder1.StartRecording` / `StopRecording` / `GetStatus` wire format frozen | done (M3) | `crates/zwhisper-ipc/src/recorder.rs:60-86`, `types.rs` (Status `(sst)`) |
| `GetStatus` returns only `"idle"` or `"recording"` (never `"starting"` / `"stopping"` / `"failed"`) | live — A1 blind spot | `crates/zwhisperd/src/recorder_service.rs:326-340` (the only state-string assignment) |
| `StateChanged` emits five wire strings (`starting`, `recording`, `stopping`, `idle`, `failed`) — distinct from GetStatus snapshot | done | `recorder_service.rs:184,232`; `lifecycle.rs:158-247` (`emit_state_changed` / `emit_terminal_state`) |
| Slot released **before** transcription (C5) — `GetStatus` reports `"idle"` for the entire 5–30 s transcription window | live (intentional, M3 lock-in) | `lifecycle.rs:170` (`hooks.sessions.release()` precedes `transcribe_file`) |
| `active-session.json` written before `StateChanged "recording"` and cleared at terminal state | done (M4 fix, post-2026-05-02 review) | `recorder_service.rs:209-228`; `lifecycle.rs:282` (`active_session::clear` inside `emit_terminal_state`) |
| Tray single-instance via `cz.zajca.Zwhisper1.Tray` D-Bus name | done (M4) | `crates/zwhisper-tray/src/single_instance.rs:42` (`TRAY_BUS_NAME`) |
| Tray dispatcher owns one `Recorder1Proxy` + `Profiles1Proxy`, optimistic `pending_cmd` lock | done (M4) | `crates/zwhisper-tray/src/cmd.rs:57-189` |
| `TrayState` carries `icon`, `active_profile`, `active_session_id`, `last_session`, `profiles`, `pending_cmd` | done (M4 + M5) | `crates/zwhisper-tray/src/state.rs:200-220` |
| CLI top-level commands: `record`, `transcribe`, `profile`, `backend`, `status` | done (M5) | `crates/zwhisper-cli/src/main.rs:48-68` |
| CLI exit-code chart (`EXIT_OK=0`, `EXIT_RECORDING_FAILED=1`, `EXIT_PROTOCOL_ERROR=2`, `EXIT_IPC_FAILURE=3`) | done | `crates/zwhisper-cli/src/commands/mod.rs:35-44` |
| `DAEMON_DOWN_HINT` constant | done (M3) | `crates/zwhisper-cli/src/commands/mod.rs:54` |
| `ashpd` workspace dep | absent | `Cargo.toml:15-90` (no `ashpd` in `[workspace.dependencies]`) |
| `crates/zwhisper-hotkey/` crate | absent | does not exist |
| `packaging/` directory | absent | `ls /home/zajca/Code/me/zwhisper` shows no `packaging/` |
| `~/.config/zwhisper/hotkey.toml` | absent | does not exist |
| Existing tray `Config::from_env` precedent for optional config | done (M4) | `crates/zwhisper-tray/src/config.rs` (`COMMAND_CHANNEL_CAPACITY`, `clipboard_max_bytes`) |
| `IDEA.md` § 11 row M6 | not yet shipped | `IDEA.md:588` |

**Verdict.** M6 is a **client-side-only** milestone. The daemon
is not modified at all; the only producer of new RPCs is the
hotkey listener task (in tray) and the CLI's new `toggle`
subcommand. The work splits into four independent batches:
(a) new `zwhisper-hotkey` lib (portal adapter trait + toggle
decision logic + debouncer + post-stop cooldown), (b) tray task
`run_hotkey` + menu entry + state field, (c) CLI subcommands
`toggle` and `hotkey {status,bind,unbind,probe}`, (d)
`packaging/zwhisper.desktop`. Tests live in (a) via `FakePortal`
fakes; the live portal path is gated behind a manual smoke test
in `docs/M6-verification.md` (per H1).

**M6 unlocks.** M7 (settings GUI) gains a real "rebind hotkey"
button on the existing portal session. M8 (packaging) ships the
`.desktop` file under `/usr/share/applications/`. Neither
depends on M5 cloud work.

## Definition of done

Each item below is a testable assertion. Items 1–6 lock the
toggle decision (the A1 fix); 7–10 the portal layer; 11–14 the
CLI surface; 15–18 the tray integration; 19–22 packaging,
config, and docs.

1. `zwhisper toggle` against a daemon in `idle` state, with at
   least one valid profile set as active, calls
   `Recorder1.StartRecording(active_profile)`, prints
   `toggle: STARTED (session=<id>, profile="<name>")` to stdout,
   exits 0. Test: `zwhisper_hotkey::toggle::tests::idle_then_toggle_starts_recording`.
2. `zwhisper toggle` against a daemon in `recording` state calls
   `Recorder1.StopRecording(active_session_id)`, prints
   `toggle: STOPPING (session=<id>)`, exits 0. The
   `active_session_id` is read from `Recorder1.GetStatus` only
   in M6.1 — for M6 the session id comes from `active-session.json`
   on disk (`zwhisper_core::active_session_path()` analogue
   exposed via a new `zwhisper-hotkey::active_session::read`
   helper). Test:
   `zwhisper_hotkey::toggle::tests::recording_then_toggle_stops`.
3. **A1 fix — toggle never starts a second recording during the
   transcription drain window.** `zwhisper toggle` issued while
   the daemon is in the post-`RecordingComplete` /
   pre-`StateChanged "idle"` window (slot already released, so
   `GetStatus` returns `"idle"`, but `active-session.json`
   still exists on disk) returns
   `ToggleOutcome::NoOp { reason: PostStopCooldown }` and prints
   `toggle: NOOP (state=draining; transcription in progress)`,
   exits 0. Verified by `zwhisper_hotkey::toggle::tests::draining_window_emits_noop_not_start`
   — the test seeds an `active-session.json` fixture file in a
   tmpdir and points the helper at it via dependency injection.
4. **Post-stop cooldown window** of `cooldown_ms = 1500` (default,
   override via `hotkey.toml`) suppresses a second toggle within
   that window after the listener observed any of:
   `StateChanged "stopping"`, `StateChanged "idle"`, or
   `StateChanged "failed"`. Test:
   `zwhisper_hotkey::toggle::tests::cooldown_blocks_rapid_restart`.
5. **Debounce window** of `debounce_ms = 250` (default, override
   via `hotkey.toml`) collapses two `Activated` events that
   arrive within 250 ms into one toggle attempt. Test:
   `zwhisper_hotkey::toggle::tests::debouncer_collapses_double_press`.
6. `zwhisper toggle` with no active profile (daemon's
   `Profiles1.GetActive` returns empty string) prints
   `toggle: FAIL (no active profile; run \`zwhisper profile set <name>\`)`
   to stderr and exits 2. Does NOT call `StartRecording` (verified
   by a mock proxy that records calls). Resolves devils-advocate
   E1. Test: `zwhisper_hotkey::toggle::tests::empty_active_profile_fails_with_hint`.
7. `PortalAdapter` trait declares the four operations
   (`create_session`, `bind`, `list_shortcuts`, `unbind`,
   `events`, `close`) and is `Send + Sync + dyn-compat`. Real
   impl `AshpdAdapter` lives behind `#[cfg(feature = "portal")]`.
   Tests use `FakePortal` for everything. Test:
   `zwhisper_hotkey::portal::tests::fake_portal_round_trip`.
8. **Portal sender validation (G1).** The `Activated` signal
   subscription only delivers events whose D-Bus sender matches
   the well-known name `org.freedesktop.portal.Desktop`. The
   subscription path uses ashpd 0.13's typed
   `GlobalShortcuts::receive_activated` (which validates sender
   internally). For paranoia, the `run_hotkey` task asserts the
   sender match in a debug-assertion plus a unit-test that feeds
   a forged `Activated` from a non-portal sender into a custom
   `FakePortal` that emits raw zbus events; the test asserts the
   forged event is dropped. Test:
   `zwhisper_hotkey::portal::tests::activated_from_non_portal_sender_dropped`.
9. **Portal backend crash recovery (B1).** When the listener's
   `HotkeySession` returns `zbus::Error::ServiceUnknown` on any
   call, the listener:
   (a) drops the stale session,
   (b) waits 500 ms (debounces a flapping backend),
   (c) attempts `HotkeySession::create` again,
   (d) on success calls `list_shortcuts` to verify the binding
       persisted; if not, surfaces a `notify-rust` desktop
       notification "Hotkey unbound — open tray to rebind",
   (e) on failure logs `warn` and sets
       `TrayState::hotkey = HotkeyMenuState::Unavailable { reason }`.
   Test: `zwhisper_hotkey::portal::tests::reconnect_on_service_unknown`.
10. **`zwhisper hotkey probe`** prints one of three single-line
    outputs and exits with the matching code (mirrors `backend
    health` style, `commands/backend.rs:213-215`):
    - `hotkey: portal=kde GlobalShortcuts=available version=2` → exit 0
    - `hotkey: portal=NONE (no GlobalShortcuts portal — bind via your WM)` → exit 2
    - `hotkey: portal=<n> GlobalShortcuts=unavailable (<reason>)` → exit 2
    Backend detection runs over the portal's `Introspect` D-Bus
    method; the human-readable backend name comes from the
    `org.freedesktop.portal.Desktop` bus name's owner PID
    (`/proc/<pid>/comm`). Test:
    `zwhisper_hotkey::probe::tests::probe_truth_table`.
11. `zwhisper hotkey status` walks the portal's `ListShortcuts`,
    prints `hotkey: BOUND (<chord>, portal=<backend>, session=<id>)`,
    exits 0. With no binding: `hotkey: NOT_BOUND (portal=<backend>)`,
    exits 0. With no portal: same as `probe` UNAVAILABLE line,
    exits 2. Test: `zwhisper_cli::commands::hotkey::tests::status_truth_table`.
12. `zwhisper hotkey bind` opens the portal `BindShortcuts`
    dialog; on user accept, prints `hotkey: BOUND (<chord>)` and
    exits 0; on user cancel, prints `hotkey: bind cancelled by
    user` and exits 2; on a 30 s timeout (H1 mitigation), prints
    `hotkey: bind timed out — no portal response in 30s` and
    exits 2. The 30 s value is in `hotkey.toml`'s
    `bind_timeout_s` (default 30) and is the upper bound on the
    `tokio::time::timeout` wrapping the `BindShortcuts` call.
    Test: `zwhisper_cli::commands::hotkey::tests::bind_timeout_returns_2`.
13. `zwhisper hotkey unbind` calls `UnbindShortcuts` and prints
    `hotkey: unbound`, exits 0 (idempotent — re-running prints
    the same line and exits 0 even when no binding existed).
    Test: `zwhisper_cli::commands::hotkey::tests::unbind_is_idempotent`.
14. **CLI `zwhisper toggle` daemon-down fallback (E2).** When
    the daemon is unreachable (`zbus::Error::ServiceUnknown`
    or no name owner), `zwhisper toggle`:
    (a) attempts to spawn `notify-send "zwhisper" "Cannot toggle:
        daemon not running. Run \`systemctl --user start zwhisperd\`."`
        with `urgency=critical`; if `notify-send` is missing or
        fails, falls back to libnotify via `notify-rust` (already
        a workspace dep, see `Cargo.toml:79`),
    (b) prints `toggle: FAIL (daemon not running)` to stderr
        plus the `DAEMON_DOWN_HINT` body,
    (c) exits 2.
    Test: `zwhisper_cli::commands::toggle::tests::daemon_down_fires_notify_send_then_exits_2`.
15. The tray `run_hotkey` task spawns immediately after the
    command dispatcher (so the listener's `Recorder1Proxy` is
    on a separate `zbus::Connection` from the dispatcher's). On
    receiving `HotkeyEvent::Activated`, it reads
    `state_rx.borrow().active_profile` AND issues a live
    `Profiles1.GetActive` call; the live call wins on
    disagreement (resolves D3). Test:
    `zwhisper_tray::hotkey::tests::live_get_active_overrides_cached_profile`.
16. **A4 mitigation — hotkey before tray ready.** `run_hotkey`
    blocks on a `watch::Receiver<bool>` flipped by the
    dispatcher once its `Recorder1Proxy::new` returns. Toggle
    `Activated` events received before that flip are buffered in
    a `tokio::sync::mpsc<HotkeyEvent>(capacity=1)` (size 1 — one
    pending press is the most user expects to recover); a second
    pending press while one is buffered is logged and dropped.
    Test: `zwhisper_tray::hotkey::tests::activated_before_proxy_ready_is_buffered_once`.
17. The tray menu shows a hotkey entry with one of four labels:
    - `Hotkey: <chord>` (bound)
    - `Hotkey: not bound — click to bind` (not bound, portal
       available)
    - `Hotkey: unavailable (no portal)` (probe NONE)
    - `Hotkey: probing…` (transient, before first probe completes)
    Click on the first three opens the portal bind dialog; click
    on `unavailable` is a no-op + tooltip "Bind via your WM
    (i3 / Hyprland) using `zwhisper toggle`". Test:
    `zwhisper_tray::tray::tests::hotkey_menu_label_truth_table`.
18. **F1 mitigation — recording-start audible/visible cue.** When
    the hotkey listener task observes a `StateChanged "recording"`
    that it itself triggered (matched by session id seen in the
    response of its own `StartRecording` call), the listener
    fires `notify-rust::Notification` with title "zwhisper" and
    body `Recording started ({active_profile})`, urgency
    Normal, timeout 2000 ms. Suppressed when `hotkey.toml` has
    `notify_on_start = false`. Test:
    `zwhisper_tray::hotkey::tests::recording_start_emits_notification_for_hotkey_path_only`.
19. `packaging/zwhisper.desktop` exists, ships
    `Type=Application`, `Exec=zwhisper-tray`,
    `X-Flatpak-Tags=`, and `StartupWMClass=zwhisper-tray`.
    The plan documents a single `make install-desktop` target
    (or a `scripts/install-desktop.sh`) that installs it under
    `~/.local/share/applications/cz.zajca.Zwhisper1.Tray.desktop`
    and runs `update-desktop-database`. Test:
    `tests/desktop_file.rs::file_parses_via_freedesktop_desktop_file_crate`
    (lightweight string-match suite — no new dep needed; or
    fall back to `xdg-utils` `desktop-file-validate` invoked
    via `assert_cmd` and skipped if absent).
20. **`hotkey.toml` parse failure must not kill the tray (D2).**
    A corrupt `~/.config/zwhisper/hotkey.toml` produces a
    `tracing::warn!` at startup ("hotkey.toml parse failed,
    using defaults") and the tray boots with the documented
    defaults (`auto_bind_on_startup = false`,
    `debounce_ms = 250`, `cooldown_ms = 1500`,
    `bind_timeout_s = 30`, `notify_on_start = true`). Defaults
    are constants in `crates/zwhisper-hotkey/src/config.rs`,
    not silent invented values; missing file is normal, parse
    failure is logged. Test:
    `zwhisper_hotkey::config::tests::corrupt_toml_falls_back_with_warn`.
21. **Wire-surface contract test.** A new
    `crates/zwhisper-ipc/tests/wire_freeze.rs` (or addendum to
    the existing signature tests in `zwhisper-ipc/src/types.rs`)
    asserts that the `Recorder1` interface signature set is
    byte-identical to the M3 lock-in: methods
    `start_recording(s) -> s`, `stop_recording(s) -> s`,
    `get_status() -> (sst)`; signals `StateChanged(ss)`,
    `RecordingComplete(ss)`, `TranscriptComplete(ssts)`. M6
    must add NO entries. Test:
    `zwhisper_ipc::types::tests::recorder_wire_format_unchanged_in_m6`.
22. `docs/M6-verification.md` ticks all of the above with
    file:line evidence (test name, log line excerpt, manual
    command output). Includes the manual smoke matrix from §
    "Manual verification gate" with date-stamped pass/fail per
    desktop. Verdict line "M6 closes …" only after all 21
    automated items are ticked AND at least KDE Plasma 6 + i3
    rows of the manual matrix are PASS.

## Architectural decisions

Each decision below is a locked choice for the M6 plan. Where
there were credible alternatives, the rejection rationale is
recorded so a later milestone does not silently re-open it.

### D1 — A1 resolution: post-stop cooldown + active-session.json fallback (Option γ + β-light)

**Decision.** The toggle decision logic in
`zwhisper-hotkey::toggle::toggle_once` consults two sources to
detect "session is winding down even though `GetStatus` says
`idle`":

1. **`active-session.json` on disk.** Already written by the
   daemon at `recorder_service.rs:209-228`, cleared at
   `lifecycle.rs:282` (inside `emit_terminal_state`). Present
   for the entire span between `StateChanged "recording"` (slot
   reserved) and the terminal `StateChanged "idle"` /
   `"failed"` (slot released, transcription complete). If this
   file exists, the daemon is mid-session.
2. **Post-stop cooldown timer in the toggle helper.** The
   listener tracks the timestamp of the most recent
   `StateChanged "stopping"`, `"idle"`, or `"failed"`. Inside a
   `cooldown_ms` window (default 1500 ms) `toggle_once` returns
   `ToggleOutcome::NoOp { reason: PostStopCooldown }` instead of
   firing `StartRecording`.

**Rationale.** Option α (extending `GetStatus` to return a
richer `state` string `"stopping"` / `"draining"`) is rejected
because **`Recorder1` is M3-frozen** (this milestone's banner
constraint). Option β (purely internal: a separate
`RecorderState` enum returned by GetStatus that includes
transcription drain) implies the same wire change and is
rejected for the same reason. Option γ (cooldown only) handles
the in-process case but does NOT help the **cross-process** case
where `zwhisper toggle` is run from i3 with no tray running —
the CLI has no `StateChanged` history to populate a cooldown
clock. Hence the disk-file fallback (β-light: side channel
already on the daemon contract) plus the cooldown clock for the
in-process tray. Both must agree before `StartRecording` fires.

**Alternatives rejected.**
- *α: extend GetStatus* — breaks the M3 wire freeze. Killed.
- *Pure γ: cooldown only* — fails the cross-process case (CLI
  toggle from i3 has no signal stream).
- *Hand-written sentinel file separate from `active-session.json`*
  — duplicates state. The existing file is sufficient.
- *Read `last-session.json` modification time* — cleared too
  late (after transcript), would over-suppress a legitimate
  start.

**What this gives up.** A user who *wants* to abort a
transcription and start a fresh recording immediately has to
wait `cooldown_ms` (1.5 s) after pressing the hotkey twice in
a row. Documented, configurable. Not a regression vs M5 (where
no toggle existed).

### D2 — One shared crate, link from both tray and CLI

**Decision.** New crate `crates/zwhisper-hotkey/` (Linux-only,
`#![cfg(target_os = "linux")]` at crate root). Tray links it
with the `portal` feature on; CLI links it with `portal` feature
on for `hotkey {bind,unbind,probe,status}` subcommands and with
default features for the standalone `toggle` subcommand.

**Rationale.** The toggle decision logic is identical between
the in-tray hotkey path and the WM-bound `zwhisper toggle` path.
Putting it in `zwhisper-tray::cmd` would force the CLI to depend
on the tray crate (it does not, by design — see
`zwhisper-tray/src/main.rs:3-5`). Inlining a copy in the CLI
would split the post-stop cooldown logic across two files,
which is the exact place a future bug could hide. One crate,
one place.

**Alternatives rejected.**
- *Module inside `zwhisper-tray`* — CLI cannot link tray.
- *Module inside `zwhisper-cli`* — tray cannot link CLI.
- *Module inside `zwhisper-ipc`* — leaks ashpd into a wire-
  surface crate that is intentionally tiny.
- *Module inside `zwhisper-core`* — `zwhisper-core` is the
  recording / transcription engine; portal is presentation
  glue. Wrong layer.

### D3 — Portal session lifecycle: lazy create with auto_bind_on_startup soft attempt

**Decision.** The tray's `run_hotkey` task starts in the
`Idle { session: None }` state. It attempts
`HotkeySession::create(APP_ID)` once at startup ONLY IF
`hotkey.toml`'s `auto_bind_on_startup = true` (default). On
failure (no portal, ServiceUnknown, etc.) it logs `warn`, sets
`TrayState::hotkey = Unavailable { reason }`, and **does not
retry**. The user re-attempts via the menu's "Bind hotkey…"
entry. On a portal-backend crash mid-session (B1) the listener
follows DoD #9 (recreate-with-debounce).

**Rationale.** Eager-at-startup wastes a portal session on i3
(no backend). Pure-on-demand surprises KDE/GNOME users for
whom "the hotkey just works after reboot" is the point. The
soft-startup attempt with notification fall-through is the
sweet spot.

**Alternatives rejected.**
- *Eager at startup, fail-fast on portal-missing* — breaks i3.
- *Pure on-demand* — KDE users have to re-bind every reboot.

### D4 — App-id is `cz.zajca.Zwhisper1.Tray` for both tray and CLI

**Decision.** The `WindowIdentifier::None` / app-id passed to
`HotkeySession::create` is `cz.zajca.Zwhisper1.Tray` regardless
of which binary calls. The `.desktop` file ships with the same
basename: `cz.zajca.Zwhisper1.Tray.desktop`. The tray's
single-instance D-Bus claim already uses this name
(`single_instance.rs:42`).

**Rationale.** xdg-desktop-portal scopes shortcut ownership by
app-id (R4 unresolved — verify post-ship; if it scopes by app-id
only, this gives one shared binding visible to both tray and
CLI; if it scopes by app-id + unique D-Bus name, the binding
moves with whichever process most recently claimed it, which
mirrors the user's intent: "rebinding from the CLI updates the
shortcut for the tray too").

**Alternatives rejected.**
- *Separate `.Cli` app-id* — fragments shortcut ownership; user
  would see two separate entries in KDE System Settings.
- *Reverse-DNS reuse of daemon name `cz.zajca.Zwhisper1`* — the
  daemon does not request global shortcuts; conflating the two
  app-ids breaks the portal's permission UX (the dialog "Allow
  zwhisper to bind shortcut?" would name the daemon, not the
  tray).

### D5 — Portal binding state is portal-owned; we persist nothing about chord identity

**Decision.** The chord (e.g. `Ctrl+Alt+R`) is **not** stored in
`hotkey.toml` or anywhere else in our config. The portal owns
that. We persist only behaviour knobs (debounce, cooldown,
auto-bind, bind timeout, notify-on-start) — the keys exist to
let users tune timing without recompiling. Defaults are documented
constants, not invented at parse time. Reading the bound chord
back is via `Portal.ListShortcuts`.

**Rationale.** Storing the chord ourselves creates a sync hazard
(user re-binds via KDE settings → our config goes stale → next
launch fights the user's choice). Avoid.

### D6 — `Recorder1` wire surface stays untouched

**Decision.** No method or signal added or modified on
`Recorder1` or `Profiles1`. The proxy traits in
`crates/zwhisper-ipc/src/recorder.rs:60-86` are byte-identical
between M5 ship and M6 ship. DoD #21 enforces this with a
regression test.

**Rationale.** Hotkey is a presentation concern. The daemon
should not even know it is being toggled by a key. Keeps the
wire surface lean for M7 and M8.

### D7 — Tests rely on `FakePortal` trait fakes; live portal is manual-only

**Decision.** All unit and integration tests use a `FakePortal`
implementation of the `PortalAdapter` trait. No `dbusmock`, no
spawned `xdg-desktop-portal` process, no GNOME/KDE-specific
fixture. The live portal path is exercised by the manual smoke
matrix in `docs/M6-verification.md` § "Manual verification
gate".

**Rationale.** dbusmock cannot model the portal's
Request/Response object pattern (see H1 / R7). A FakePortal
that follows the same `PortalAdapter` trait covers every
in-process branch (debounce, cooldown, reconnect, timeout) and
the manual matrix covers what the trait abstraction cannot.

## Risks

Pulled from `.cache/M6-devils-advocate.md`, ordered by severity.
Each row points to where it is addressed (DoD item or
architectural decision).

| ID | Severity | Summary | Addressed by |
|---|---|---|---|
| A1 | Critical | `GetStatus` blind spot during transcription drain → spurious `StartRecording` | DoD #3, decision D1 |
| E1 | High | `zwhisper toggle` with no active profile → cryptic ProfileNotFound | DoD #6 |
| A2 | High | TOCTOU: daemon drops between GetStatus and Start/Stop | DoD #14 (daemon-down notify-send fallback handles every "ServiceUnknown" arrival, regardless of which method call surfaced it). Plus: `toggle_once` issues both `GetStatus` and the action call inside one helper; on `zbus::Error::ServiceUnknown` from the action call, the typed error path runs the same notification logic. |
| E2 | High | Daemon down in i3 `bindsym` → complete silence | DoD #14 |
| G1 | High | Spoofed `Activated` signal | DoD #8 |
| B1 | High | Portal backend crash → stale `HotkeySession` | DoD #9 |
| A4 | Medium | Hotkey fires before tray proxy ready | DoD #16 |
| A3 | Medium | CLI + tray race on `idle` → second StartRecording errors | Risk-only mitigation: `zwhisper toggle` treats `RpcError::SessionInUse` as "already recording" and exits 0 with `toggle: NOOP (state=already-recording)`. Documented in DoD #1 commentary; covered by `zwhisper_cli::commands::toggle::tests::session_in_use_classified_as_already_recording`. |
| D2 | Medium | Corrupt `hotkey.toml` → tray startup failure | DoD #20 |
| D3 | Medium | Stale `active_profile` cache at toggle time | DoD #15 |
| C1 | Medium | Same app-id from CLI + tray → double portal listener | Decision D4 (single shared app-id). The tray's single-instance D-Bus claim already prevents two trays from running. The CLI's `hotkey bind` is a one-shot — when it returns, no listener is left running. |
| B2 | Medium | User revokes binding via System Settings → silent stale indicator | The listener subscribes to `ShortcutsChanged` (ashpd 0.13 typed signal); on receipt it refreshes `TrayState::hotkey` from `list_shortcuts`. If the signal is not delivered by a backend (R3 unverified), the tray menu's "Hotkey: …" label may go stale until the next `auto_bind_on_startup` cycle. Documented as known limitation; verification matrix gates this on KDE. |
| B3 | Medium | Portal not installed → startup error or silent skip | DoD #20 (graceful fallback; `Unavailable` menu state instead of panic). |
| F1 | Medium | No audible/visible cue on hotkey-triggered start | DoD #18 |
| H1 | Medium | `FakePortal` misses KDE async Request/Response pattern | Decision D7 + manual verification matrix |
| D1 | Low | `duration_ms` inflated by pre-capture setup | Out of scope for M6. Documented in M6-verification.md as "known M3 quirk; M7 may move `started_at` to post-`Recorder::start`". |
| C2 | Low | Flatpak double-instance bypass | Out of scope (M8 packaging milestone). Flagged in M6-verification.md. |
| F2 | Low | Bind dialog blocks tray UI thread | Avoided by structure: the menu callback `try_send`s a `HotkeyControl::Bind` message into the listener's mpsc; the actual `BindShortcuts` await happens on the listener task, never on the ksni callback thread. |

## Open questions for ship (research-deferred — verify during implementation)

These items map to R3–R7 from `.cache/M6-research.md`. They are
**not blockers** — the design tolerates either outcome — but
each must be answered empirically and recorded in
`docs/M6-verification.md`.

1. **R4 — app-id ownership scope.** Does the portal scope
   shortcut ownership by app-id alone, or by app-id + unique
   D-Bus name? Test: bind via tray, kill tray, run
   `zwhisper hotkey status` from CLI — same app-id, fresh
   connection. If `BOUND` row appears, ownership is by app-id
   alone (good). If `NOT_BOUND`, the CLI cannot inspect the
   tray's binding; record this and reframe `hotkey status`
   docs.
2. **R5 — binding persistence across portal restart.** Test:
   bind, `systemctl --user restart xdg-desktop-portal-kde`,
   call `ListShortcuts`. If preserved, B1's "rebind on
   reconnect" is rarely needed; if not, the tray's
   `auto_bind_on_startup` may need to default `true` on KDE
   only.
3. **R5b — binding persistence across reboot.** Same test
   after reboot. If lost, document the implication for users.
4. **R6 — KDE without `.desktop` file.** Test: run
   `zwhisper-tray` straight from `cargo run` (no install) and
   attempt `BindShortcuts`. Record whether KDE refuses,
   accepts with the binary basename, or accepts with a generic
   placeholder.
5. **R3 — backend matrix per-desktop.** Per-line outcomes for
   KDE Plasma 6 (primary), GNOME 47+, Sway, and i3 (X11 — must
   produce `UNAVAILABLE` from `probe`). Recorded in
   `docs/M6-verification.md` § "Manual verification gate".
6. **R7 — `dbusmock` for portal.** Confirmed deferred. The
   manual matrix is the contract.

## Implementation tasks

Tech-lead style decomposition. Each task is owner-agnostic and
carries (a) a one-line goal, (b) the files it owns, (c) its
dependency on previous tasks. Group letters (A/B/C/D/E/F/G/H)
correspond to "parallel groups" — letters can run concurrently;
within a letter, ordering matters.

### A. Workspace + new crate skeleton

- **A1**. Add `ashpd = { version = "0.13", default-features = true }`
  and `async-trait = "0.1"` (already present) to
  `Cargo.toml [workspace.dependencies]`. Verify default features
  pull in `tokio` (per R2 research). Owns: `Cargo.toml`. Dep: —.
- **A2**. Create `crates/zwhisper-hotkey/Cargo.toml` with
  `[features] default = ["portal"]; portal = ["dep:ashpd"]`.
  Owns: `crates/zwhisper-hotkey/Cargo.toml`. Dep: A1.
- **A3**. Create `crates/zwhisper-hotkey/src/lib.rs` with the
  `#![cfg(target_os = "linux")]` gate, module declarations
  (`config`, `toggle`, `portal`, `probe`, `active_session`),
  and re-exports per architecture proposal § 1. Owns:
  `crates/zwhisper-hotkey/src/lib.rs`. Dep: A2.
- **A4**. Add the new crate to the implicit workspace member
  glob (already `members = ["crates/*"]` per `Cargo.toml:3`).
  Verify `cargo metadata` lists it. Dep: A3.

### B. Toggle decision logic + Debouncer + cooldown

- **B1**. `crates/zwhisper-hotkey/src/config.rs` — `HotkeyConfig`
  struct (debounce_ms, cooldown_ms, bind_timeout_s,
  auto_bind_on_startup, notify_on_start) with documented constants
  for defaults and `Config::from_path(&Path)` + a `Default` impl.
  Tests for: defaults, parse OK, parse failure → defaults +
  warn. Dep: A3.
- **B2**. `crates/zwhisper-hotkey/src/active_session.rs` — read-only
  helper `read_active_session() -> Option<ActiveSessionRef>`
  resolving `$XDG_STATE_HOME/zwhisper/active-session.json`
  (matching the daemon's writer at `active_session.rs:89`). Tests:
  fixture file present → Some; absent → None; corrupt → None +
  warn. Dep: A3.
- **B3**. `crates/zwhisper-hotkey/src/toggle.rs` — `Debouncer`
  struct + `toggle_once` function + `ToggleOutcome` /
  `ToggleError` types. Decision table per architecture proposal §
  3. Tests for: debounce, cooldown, draining-window NoOp, empty
  active profile fail, daemon down classification. Mock the
  `Recorder1Proxy` via a small trait so tests can inject.
  Dep: B1, B2.
- **B4**. Wire `toggle_once` to call the proxy via an injectable
  `RecorderClient` trait so the function is unit-testable without
  a live D-Bus. The production impl wraps a real
  `Recorder1Proxy<'_>`; the test impl is a struct with recorded
  call history. Owns:
  `crates/zwhisper-hotkey/src/toggle.rs` (extension). Dep: B3.

### C. Portal layer (PortalAdapter trait + AshpdAdapter + FakePortal)

- **C1**. `crates/zwhisper-hotkey/src/portal.rs` — `PortalAdapter`
  trait (`async fn create_session`, `bind`, `list_shortcuts`,
  `unbind`, `events: BoxStream`, `close`); `BindRequest`,
  `BoundShortcut`, `HotkeyEvent`, `PortalError` types. Dep: A3.
- **C2**. `AshpdAdapter` real impl behind
  `#[cfg(feature = "portal")]`. Uses ashpd 0.13's
  `desktop::global_shortcuts::GlobalShortcuts` proxy and
  `WindowIdentifier::None` for headless callers (per R2). Dep: C1.
- **C3**. `FakePortal` test fake: in-memory `Vec<BoundShortcut>`,
  injectable failure modes (`fail_next_call_with`,
  `emit_activated_from_sender`). Dep: C1.
- **C4**. `HotkeySession` wrapper struct exposing the listener-
  facing API (`create`, `bind`, `list_shortcuts`, `next_event`,
  `close`); generic over `PortalAdapter` so tests use FakePortal
  and prod uses AshpdAdapter. Dep: C2, C3.
- **C5**. Reconnect-on-`ServiceUnknown` logic per DoD #9. Tests
  with `FakePortal::fail_next_with_service_unknown`. Dep: C4.
- **C6**. Sender-validation test for the `Activated` stream per
  DoD #8. The FakePortal allows emitting events from arbitrary
  sender names; the production `HotkeySession::next_event`
  filters them. Dep: C4.

### D. Probe (portal availability detection)

- **D1**. `crates/zwhisper-hotkey/src/probe.rs` — `detect_backend()`
  → `BackendDetected::{Kde, Gnome, Wlr, Other(String), None}`.
  Walks the session bus for `org.freedesktop.portal.Desktop`
  owner PID, reads `/proc/<pid>/comm`. Falls back to "Other" /
  "None" cleanly. Tests: fixture-driven via a small `BusInspector`
  trait. Dep: A3.
- **D2**. `probe()` top-level function returning a `ProbeReport`
  (backend + GlobalShortcuts version property + reason string).
  Tests for the truth table per DoD #10. Dep: D1.

### E. Tray integration

- **E1**. Add `pub hotkey: HotkeyMenuState` field +
  `HotkeyMenuState` enum to
  `crates/zwhisper-tray/src/state.rs`. Default `Unknown`. Update
  every constructor site (one site:
  `TrayState::default` at `state.rs:225`). Dep: A3.
- **E2**. New file
  `crates/zwhisper-tray/src/hotkey.rs` — `run_hotkey` task,
  the listener loop, the `HotkeyControl` mpsc command enum
  (`Bind`, `Unbind`, `Probe`). Owns the active `HotkeySession`
  option. Dep: B4, C5, D2, E1.
- **E3**. Wire `run_hotkey` into `zwhisper-tray/src/main.rs`
  immediately after the dispatcher spawn (between line 188 and
  the ksni `tray.spawn().await` at line 195). New
  `tokio::sync::watch::Sender<bool>` flips from `false` to
  `true` once the dispatcher's `Recorder1Proxy::new` returns
  Ok (DoD #16). Dep: E2.
- **E4**. Add `Hotkey: …` menu entry to
  `crates/zwhisper-tray/src/tray.rs`. Reads
  `state.hotkey: HotkeyMenuState`. Click handler `try_send`s
  `HotkeyControl::Bind` to the listener via a new mpsc held by
  the tray. Dep: E1, E2.
- **E5**. Hotkey-path notification on `StateChanged "recording"`
  per DoD #18 — extend the listener to subscribe to the same
  `Recorder1.StateChanged` signal stream as the pump (or take a
  `watch::Receiver<TrayState>` clone) and fire `notify-rust`
  when the matched session id corresponds to a recently-fired
  toggle. Dep: E2.

### F. CLI surface

- **F1**. Extend `crates/zwhisper-cli/src/cli.rs` with
  `HotkeyCmd` enum (`Status`, `Bind`, `Unbind`, `Probe`) and
  add `Toggle` + `Hotkey(HotkeyCmd)` to the top-level `enum
  Command` at `crates/zwhisper-cli/src/main.rs:48-68`. Dep: A3.
- **F2**. New file
  `crates/zwhisper-cli/src/commands/toggle.rs` — single-screen
  `run` + `run_async`. Calls `zwhisper-hotkey::toggle::toggle_once`
  with a real `Recorder1Proxy`. Daemon-down path per DoD #14
  (notify-send fallback). Dep: B4, F1.
- **F3**. New file
  `crates/zwhisper-cli/src/commands/hotkey.rs` — match
  `HotkeyCmd` variants. `Status` calls `HotkeySession::list_shortcuts`;
  `Bind` opens portal dialog + 30 s timeout (DoD #12); `Unbind`
  is idempotent (DoD #13); `Probe` calls
  `zwhisper-hotkey::probe::probe()`. Dep: C4, D2, F1.
- **F4**. Wire both new modules into
  `crates/zwhisper-cli/src/commands/mod.rs` — `pub(crate) mod
  hotkey; pub(crate) mod toggle;`. Update the top-level dispatch
  in `main.rs:74-80`. Dep: F2, F3.

### G. Packaging

- **G1**. `mkdir packaging/`. Owns: `packaging/`. Dep: —.
- **G2**. `packaging/zwhisper.desktop` — the entry per
  architecture proposal § 5 (`Type=Application`,
  `Exec=zwhisper-tray`, `Icon=zwhisper`, `Categories=AudioVideo;
  Audio;Recorder;`, `StartupWMClass=zwhisper-tray`,
  `X-GNOME-UsesNotifications=true`). Dep: G1.
- **G3**. `scripts/install-desktop.sh` — `install -Dm644 …`
  to `~/.local/share/applications/cz.zajca.Zwhisper1.Tray.desktop`,
  then `update-desktop-database`. Dep: G2.

### H. Tests + docs

- **H1**. Unit tests for every public surface of
  `zwhisper-hotkey` (config, toggle, portal, probe). Run under
  `cargo test -p zwhisper-hotkey`. Dep: B-, C-, D- complete.
- **H2**. Integration test for the tray's `run_hotkey` task —
  spawn the task with a `FakePortal` and a stub
  `Recorder1Proxy` (existing `dbusmock` setup not needed; a
  thin trait fake suffices). Dep: E5.
- **H3**. CLI subcommand parser tests — extend
  `crates/zwhisper-cli/src/cli.rs::tests` to cover `toggle`,
  `hotkey status`, `hotkey bind`, `hotkey unbind`,
  `hotkey probe`. Dep: F1.
- **H4**. Wire-format regression test per DoD #21 — extend
  `crates/zwhisper-ipc/src/types.rs::tests` (or new file
  `tests/wire_freeze.rs`). Dep: A3.
- **H5**. `docs/M6-verification.md` skeleton mirroring
  `docs/M5-verification.md`. Dep: H1–H4 in flight.
- **H6**. README update — add a "Hotkey toggle" section to the
  top-level README pointing at `zwhisper hotkey bind` and i3
  `bindsym` examples. Dep: F4.
- **H7**. `IDEA.md` § 11 row M6: change "shipped" status only
  after manual verification matrix passes. Dep: H5.

### Suggested team sizing

- Group A (skeleton) — 1 teammate, ~3 tasks.
- Group B (toggle logic) — 1 teammate, ~4 tasks.
- Group C (portal) — 1 teammate, ~6 tasks.
- Group D (probe) — folded into the same teammate as Group C
  (small surface).
- Group E (tray) — 1 teammate, ~5 tasks.
- Group F (CLI) — 1 teammate, ~4 tasks.
- Group G (packaging) — folded into Group F (small).
- Group H (tests + docs) — folded into the matching feature
  teammate.

Total: 4 implementation teammates + 1 product-engineer for the
quality gate. Within the CLAUDE.md "Team Rules" cap of 5.

Dependency graph:
```
A → B, C, D, E (all four parallel groups depend on A's skeleton)
B → C (toggle feeds the listener flow that portal.rs spawns)
C → E (tray consumes HotkeySession)
D → E, F (both consume probe())
E → F (CLI's hotkey subcommands share types with tray, but only
       the trait surfaces; no run-loop coupling)
B, C, D, E, F → H (tests + docs trail every batch)
```

## Test matrix

New tests by file (name → assertion summary).

| File | Test | Asserts |
|---|---|---|
| `zwhisper-hotkey/src/config.rs` | `defaults_are_documented_constants` | All five fields equal documented constants. |
| `zwhisper-hotkey/src/config.rs` | `parse_ok_overrides_defaults` | TOML override wins; missing keys retain defaults. |
| `zwhisper-hotkey/src/config.rs` | `corrupt_toml_falls_back_with_warn` | DoD #20 — corrupt file → defaults + warn (no panic). |
| `zwhisper-hotkey/src/active_session.rs` | `reads_present_active_session_json` | Fixture file → `Some(ActiveSessionRef)`. |
| `zwhisper-hotkey/src/active_session.rs` | `absent_returns_none_no_warn` | Missing file is normal. |
| `zwhisper-hotkey/src/active_session.rs` | `corrupt_returns_none_with_warn` | Bad JSON → None + warn. |
| `zwhisper-hotkey/src/toggle.rs` | `idle_then_toggle_starts_recording` | DoD #1 — happy path. |
| `zwhisper-hotkey/src/toggle.rs` | `recording_then_toggle_stops` | DoD #2 — happy path. |
| `zwhisper-hotkey/src/toggle.rs` | `draining_window_emits_noop_not_start` | DoD #3 — A1 fix via active-session.json. |
| `zwhisper-hotkey/src/toggle.rs` | `cooldown_blocks_rapid_restart` | DoD #4. |
| `zwhisper-hotkey/src/toggle.rs` | `debouncer_collapses_double_press` | DoD #5. |
| `zwhisper-hotkey/src/toggle.rs` | `empty_active_profile_fails_with_hint` | DoD #6 — E1 fix. |
| `zwhisper-hotkey/src/toggle.rs` | `daemon_down_classifies_as_DaemonDown` | TOCTOU map (A2 mitigation half). |
| `zwhisper-hotkey/src/toggle.rs` | `session_in_use_classified_as_already_recording` | A3 mitigation — exit 0 with NOOP. |
| `zwhisper-hotkey/src/portal.rs` | `fake_portal_round_trip` | Bind → list → unbind via FakePortal. |
| `zwhisper-hotkey/src/portal.rs` | `activated_from_non_portal_sender_dropped` | DoD #8 — G1 fix. |
| `zwhisper-hotkey/src/portal.rs` | `reconnect_on_service_unknown` | DoD #9 — B1 fix. |
| `zwhisper-hotkey/src/portal.rs` | `bind_timeout_after_30s` | DoD #12 helper. |
| `zwhisper-hotkey/src/probe.rs` | `probe_truth_table` | DoD #10 — three exit-code outputs. |
| `zwhisper-tray/src/hotkey.rs` | `activated_before_proxy_ready_is_buffered_once` | DoD #16 — A4 fix. |
| `zwhisper-tray/src/hotkey.rs` | `live_get_active_overrides_cached_profile` | DoD #15 — D3 fix. |
| `zwhisper-tray/src/hotkey.rs` | `recording_start_emits_notification_for_hotkey_path_only` | DoD #18 — F1 fix. |
| `zwhisper-tray/src/tray.rs` | `hotkey_menu_label_truth_table` | DoD #17. |
| `zwhisper-cli/src/commands/toggle.rs` | `daemon_down_fires_notify_send_then_exits_2` | DoD #14 — E2 fix. |
| `zwhisper-cli/src/commands/toggle.rs` | `session_in_use_classified_as_already_recording` | A3 mitigation. |
| `zwhisper-cli/src/commands/hotkey.rs` | `status_truth_table` | DoD #11. |
| `zwhisper-cli/src/commands/hotkey.rs` | `bind_timeout_returns_2` | DoD #12. |
| `zwhisper-cli/src/commands/hotkey.rs` | `unbind_is_idempotent` | DoD #13. |
| `zwhisper-cli/src/cli.rs` (tests) | `parses_toggle_subcommand` | clap surface. |
| `zwhisper-cli/src/cli.rs` (tests) | `parses_hotkey_subcommand_variants` | clap surface — `status/bind/unbind/probe`. |
| `zwhisper-ipc/src/types.rs` (tests) | `recorder_wire_format_unchanged_in_m6` | DoD #21. |
| `tests/desktop_file.rs` | `file_parses_via_desktop_file_validate` | DoD #19. |

Approx. count: 30 new tests (24 unit, 4 integration, 2 CLI
parser). Workspace test total target after M6: 400 (M5) + ~30 =
≈ 430.

## Wire-surface contract

### Frozen — touched by NO code in M6

- `crates/zwhisper-ipc/src/recorder.rs` — `Recorder1Proxy`
  trait; methods (`start_recording`, `stop_recording`,
  `get_status`); signals (`StateChanged`, `RecordingComplete`,
  `TranscriptComplete`); D-Bus name `cz.zajca.Zwhisper1`;
  object path `/cz/zajca/Zwhisper1`; interface name
  `cz.zajca.Zwhisper1.Recorder1`.
- `crates/zwhisper-ipc/src/types.rs` — `Status` `(sst)`,
  `ProfileEntry` `(ssu)`, `ProfileEntryV2` `(ssus)`.
- `crates/zwhisper-ipc/src/error.rs` — `RpcError` variant set.
- `crates/zwhisperd/src/recorder_service.rs` —
  `RecorderInterface` impl is mod-private and unchanged;
  `get_status` continues to return `("idle"|"recording", ...)`.
- `crates/zwhisperd/src/lifecycle.rs` — signal ordering
  invariant unchanged.

### Additive — new in M6, evolution-safe

- New crate `zwhisper-hotkey` (Linux-only, lib).
- New CLI subcommands `zwhisper toggle` and
  `zwhisper hotkey {status,bind,unbind,probe}`.
- New tray field `TrayState::hotkey: HotkeyMenuState`.
- New tray task `run_hotkey` (sibling to
  `cmd::run_dispatcher`).
- New tray menu entry `Hotkey: …`.
- New optional config file
  `~/.config/zwhisper/hotkey.toml`.
- New packaging file
  `packaging/zwhisper.desktop`.
- New script `scripts/install-desktop.sh`.
- New workspace deps `ashpd = "0.13"`.

### Forbidden — must NOT change

- The `Recorder1` D-Bus signature set (DoD #21 enforces).
- The `Profiles1` D-Bus signature set (no changes from M5).
- The `Status.state` enum strings (`"idle"`, `"recording"` —
  the only two `GetStatus` returns; the additional wire
  strings in `StateChanged` are unchanged).
- The `active-session.json` schema_version (still `1`).
- The `last-session.json` schema_version (still `1`).
- The CLI `EXIT_*` constants (still 0/1/2/3).

## Manual verification gate

Before "M6 closes" can be ticked in `docs/M6-verification.md`,
the matrix below MUST be filled in by the maintainer on a real
desktop session. KDE Plasma 6 + i3 are mandatory (i3 because it
exercises the no-portal path); GNOME and Sway are best-effort
(record outcome, not pass/fail-blocking).

| Desktop | Test | Expected output | Pass? |
|---|---|---|---|
| KDE Plasma 6 | Launch tray, click "Bind hotkey…", pick `Ctrl+Alt+R`, release window | Tray shows `Hotkey: Ctrl+Alt+R`. KDE System Settings → Shortcuts lists `zwhisper` as owner. | __ |
| KDE Plasma 6 | Press `Ctrl+Alt+R` from a different app's focus (e.g. terminal) | Tray icon flips to recording state within 250 ms. Desktop notification "Recording started (<active_profile>)". `~/.local/share/zwhisper/recordings/<id>.flac` appears on disk. | __ |
| KDE Plasma 6 | Press `Ctrl+Alt+R` again | `StateChanged "stopping"` propagates; tray icon flips to stopping; transcription completes; tray icon returns to idle. | __ |
| KDE Plasma 6 | While transcription is running (5–30 s window), press `Ctrl+Alt+R` once | NoOp — no second recording starts. Tray label briefly shows "draining" or remains in stopping. | __ |
| KDE Plasma 6 | `systemctl --user restart xdg-desktop-portal-kde` while tray is bound | Tray re-establishes session; `hotkey status` from CLI still reports `BOUND`. (R5 verification.) | __ |
| KDE Plasma 6 | `zwhisper hotkey status` from a fresh terminal | `BOUND (Ctrl+Alt+R, portal=kde, session=...)`, exit 0. | __ |
| KDE Plasma 6 | `systemctl --user stop zwhisperd; zwhisper toggle` | `notify-send` notification appears (critical urgency). Stderr: `toggle: FAIL (daemon not running)`. Exit 2. | __ |
| GNOME 47+ | Launch tray, attempt bind | Document UX. KDE-comparable expected; GNOME may use a different chord-picker. | __ |
| GNOME 47+ | Press chord, observe toggle | Same as KDE; document any UX deviation. | __ |
| Sway / wlroots | `zwhisper hotkey probe` | If `xdg-desktop-portal-wlr` is installed: `available`; else `UNAVAILABLE`. | __ |
| i3 (X11) | `zwhisper hotkey probe` | `hotkey: portal=NONE (no GlobalShortcuts portal — bind via your WM)`, exit 2. | __ |
| i3 (X11) | `bindsym Mod4+Shift+r exec --no-startup-id /usr/bin/zwhisper toggle`, press chord | Recording starts via D-Bus; second press stops. | __ |
| i3 (X11) | Same `bindsym` while daemon stopped | `notify-send` notification appears (DoD #14 — E2). | __ |

Manual matrix runs go in `docs/M6-verification.md` with date,
distro, package versions, and Pass/Fail per row.

## Out of scope (deferred to M6.1+ / M7)

- **Push-to-talk semantics** (`key down = start`, `key up =
  stop`). The portal exposes only `Activated`; PTT cannot be
  modelled without a different protocol. Documented in IDEA.md
  § 12 and re-affirmed here.
- **Multiple chords** (e.g. `Ctrl+Alt+R` for recording profile
  A, `Ctrl+Alt+M` for profile B). M6 ships exactly one
  shortcut. M7 may add a per-profile binding.
- **`zwhisper toggle --profile <name>`**. Honour `--profile`
  when set; otherwise fall back to `Profiles1.GetActive`. Held
  for M6.1 — keeps DoD #6's "no active profile = exit 2" hint
  cleaner for now.
- **GUI to enter/edit hotkey chord without the portal dialog**.
  M7 settings GUI scope.
- **Hot-reload of `hotkey.toml`**. M6 reads once at startup;
  changes require tray restart.
- **Any `Recorder1` or `Profiles1` D-Bus changes**. Frozen.
  Period.
- **Flatpak / OS-package shipping of `.desktop`**. M8.
- **Telemetry of hotkey-vs-tray-vs-CLI-trigger ratio**. Out of
  scope; tracing logs `trigger=hotkey|tray|cli` already, that
  is enough.
- **Cross-host shortcut sync**. The portal owns binding state;
  per-host config is the contract.

## Coordination notes

- **No teammate edits a file owned by another teammate.** The
  groups above are file-disjoint by construction. Tests live
  in the same files as the code they cover (Rust convention)
  and follow the same file-ownership split.
- **Group F (CLI) blocks on Group B (toggle logic) for the
  `toggle.rs` command, and on Group C+D for the `hotkey.rs`
  command.** Group F can stub the missing pieces during
  early development with the `unimplemented!()` placeholder
  and a `#[ignore]`d test; the `unimplemented!()` MUST be
  resolved before each DoD item ticks.
- **The product-engineer quality gate verifies every DoD item
  with file:line evidence in `docs/M6-verification.md`.** No
  "I think we're done" — only the verification doc closes the
  milestone.
- **The manual verification matrix is the gating step for
  shipping.** Failing the i3 row or the KDE Plasma 6 row =
  re-delegate, not ship.

## Plan headlines (10-bullet summary)

1. **A1 fix is the hinge.** `GetStatus` only ever returns
   `"idle"` or `"recording"` (verified at `recorder_service.rs:326-340`),
   so the architect's "starting/stopping → NoOp" idea is
   structurally impossible without breaking the M3 wire freeze.
   We resolve it by combining a 1.5 s post-stop cooldown with
   an `active-session.json` on-disk fallback (the file already
   exists during the entire transcription drain, cleared only
   at terminal `StateChanged`).
2. **No daemon changes.** `Recorder1` / `Profiles1` are byte-
   identical between M5 and M6. DoD #21 enforces this.
3. **One new lib crate** `zwhisper-hotkey` (Linux-only) hosts
   the toggle decision logic, the portal adapter trait
   (`PortalAdapter` with `AshpdAdapter` and `FakePortal`), the
   debouncer, the cooldown clock, the active-session reader,
   and the probe.
4. **Tray gets one new task** (`run_hotkey`, sibling to
   `run_dispatcher`) plus one new state field
   (`TrayState::hotkey`) plus one new menu entry. No edits to
   the pump.
5. **CLI gets two new top-level commands**: `zwhisper toggle`
   (universal — works without tray, ideal for i3 `bindsym`) and
   `zwhisper hotkey {status,bind,unbind,probe}`.
6. **Daemon-down in i3 must not be silent (E2).** `zwhisper
   toggle` falls back to `notify-send` (then `notify-rust`) on
   any `ServiceUnknown` from the daemon. DoD #14.
7. **Portal sender validation (G1)** — `Activated` events are
   only honoured when sender = `org.freedesktop.portal.Desktop`.
   Defends against bus-signal spoofing.
8. **Manual verification gate** (KDE + i3 mandatory; GNOME +
   Sway best-effort). dbusmock cannot model the portal's
   Request/Response flow (R7); FakePortal covers everything in-
   process; the live portal is exercised by hand and recorded
   in `docs/M6-verification.md`.
9. **One new `.desktop` file** at
   `packaging/zwhisper.desktop` (app-id
   `cz.zajca.Zwhisper1.Tray`; matches the existing single-
   instance D-Bus name). One install script. No `.desktop`
   means KDE/GNOME may refuse to persist bindings — verify
   empirically (R6).
10. **All defaults are documented constants in
    `zwhisper-hotkey::config`, never invented at parse time.**
    Corrupt `hotkey.toml` logs a warning and falls back to
    those constants — never kills the tray (D2). Per CLAUDE.md
    "no silent defaults", the constants are the documented
    contract, and a missing optional file is normal (parse
    failure is the surprise, hence the warn).

---

**Question for the user:** Ready to spawn the implementation team
(4 teammates + product-engineer per § "Suggested team sizing"),
or any plan adjustments before kickoff? In particular: the A1
resolution (D1) introduces a hard 1500 ms post-stop cooldown by
default — is that acceptable, or should the default be tighter
(750 ms) at the cost of a smaller safety margin?
