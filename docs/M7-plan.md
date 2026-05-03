# M7 — Settings GUI (FLTK): implementation plan

> Status: **PROPOSAL — pending user approval**.
>
> Scope (locked-in by user, 2026-05-03):
> - On-demand `zwhisper-settings` binary, spawned from tray "Settings…" menu.
> - Four tabs: Profiles, Models, Whisper-CLI, Hotkey.
> - Hotkey rebind tab IN (M6 portal layer reuse).
> - Secrets editor OUT (deferred post-M8).
> - FLTK only — no Slint fallback. Manual KDE Plasma 6 1.5× scaling
>   verification is a ship-blocker.
> - Model downloader source URL configurable (HuggingFace default).
>
> Synthesised from `/tmp/m7-architecture.md` and `/tmp/m7-devils-advocate.md`.
> Format mirrors `docs/M6-plan.md`.

---

## Status snapshot (2026-05-03)

| Area | State | Evidence |
|---|---|---|
| `Profiles1.list / list_v2 / get_active / set_active / reload` wire format frozen | done (M3 + M5) | `crates/zwhisper-ipc/src/profiles.rs:41-65` |
| `Recorder1.{StartRecording,StopRecording,GetStatus}` frozen | done (M3) | `crates/zwhisper-ipc/src/recorder.rs:60-86` |
| `zwhisper-hotkey::PortalAdapter` trait + `AshpdAdapter` impl + `HotkeyConfig` defaults | done (M6) | `crates/zwhisper-hotkey/src/{portal.rs,config.rs}` |
| `zwhisper-core::profile::{listing,loader,schema,paths,migrations}` | done (M2) | `crates/zwhisper-core/src/profile/*` |
| `zwhisper-core::transcribe::{models,discovery}` | done (M1+M2) | `crates/zwhisper-core/src/transcribe/{models,discovery}.rs` |
| `paths::{validate_name,user_override_path,shipped_path,user_profiles_dir}` | `pub(crate)` | `crates/zwhisper-core/src/profile/paths.rs:10,25,41,49` |
| `transcribe::models::resolve_model` / `models_dir` | `pub(crate)` | `crates/zwhisper-core/src/transcribe/models.rs:83,112` |
| `transcribe::discovery::locate_whisper_cli` | `pub(crate)` | `crates/zwhisper-core/src/transcribe/discovery.rs:148` |
| Tray single-instance pattern via `cz.zajca.Zwhisper1.Tray` D-Bus name | done (M4) | `crates/zwhisper-tray/src/single_instance.rs:42,62` |
| `crates/zwhisper-settings/` | absent | does not exist |
| `fltk` workspace dep | absent | `Cargo.toml [workspace.dependencies]` has no `fltk` |
| `sha2` workspace dep | absent | not yet added |
| `~/.config/zwhisper/models.toml` | absent | does not exist |
| `crates/zwhisper-settings/checksums.toml` | absent | does not exist |
| `IDEA.md § 11` row M7 | not yet shipped | `IDEA.md:589` |

**Verdict.** M7 is a **client-side-only** milestone. Daemon is not
modified. The work splits into four parallel batches: (a) workspace
skeleton + FLTK app shell + threading bridge, (b) profile editor tab +
list / form / atomic save / diff, (c) model downloader tab + tokio
download state machine + SHA256 + configurable URL + checksum manifest,
(d) whisper-cli detector + hotkey rebind tab + tray rebind notification.
Plus minor `pub(crate) → pub` upgrades in `zwhisper-core` (§ Wire-surface
contract).

**M7 unlocks.** M8 (packaging) ships `zwhisper-settings` desktop entry,
release process maintains `checksums.toml`. Future MX (post-M8) adds
secrets editor on top of this skeleton.

---

## Definition of done

Each item below is a testable assertion. Items 1–8 lock the four tabs'
core flows; 9–13 the cross-cutting blockers from devils-advocate;
14–17 the wire-surface upgrades; 18–22 packaging, config, manual gate.

### Profile editor tab

1. Opening the **Profiles tab** lists three sections — User / Shipped /
   Embedded — populated from `zwhisper_core::profile::listing::list_entries()`.
   Test: `zwhisper_settings::tabs::profile::tests::list_groups_by_source`.
2. Clicking a profile loads it via `zwhisper_core::profile::load(name)`
   into a form. **Save** validates with `Profile::validate()` (any
   `Err` → red inline label, no disk write), then writes via
   `tempfile::NamedTempFile::persist` to
   `paths::user_override_path(name)`, then calls
   `Profiles1Proxy::reload()` (when daemon is reachable).
   Test: `tabs::profile::tests::save_validates_then_atomic_writes_then_reloads`.
3. **Save while daemon is recording (C1).** Before calling `reload`,
   query `Recorder1.GetStatus`; if `state == "recording"`, surface a
   modal "Daemon is recording. Save will apply on next recording —
   continue?" with OK / Cancel. On OK, write only — skip `reload`. On
   Cancel, abort. Test:
   `tabs::profile::tests::save_during_recording_warns_and_defers_reload`.
4. **Profile name validation (G2).** The Clone dialog validates the
   destination name through `paths::validate_name` *before* I/O.
   Rejects: empty, `/` or `\` characters, leading `.`, length > 64,
   non-UTF-8, control chars. Inline red label; Save button disabled
   until valid. Test:
   `tabs::profile::tests::clone_name_traversal_rejected`.
5. **Diff against shipped/embedded.** "Show diff" reads the user file
   and the corresponding shipped/embedded body, renders a hand-rolled
   line-by-line diff (no new dep). Test:
   `tabs::profile::tests::diff_marks_added_removed_lines`.

### Model downloader tab

6. Selecting a model from the list and clicking **Download** transitions
   the state machine `Idle → Resolving → Fetching{progress} →
   Verifying → Installed`. SHA256 verified against
   `crates/zwhisper-settings/checksums.toml` (compile-time embedded).
   Atomic rename from `<models_dir>/.partial/ggml-<name>.bin.part` to
   `<models_dir>/ggml-<name>.bin`. Test (with `wiremock`):
   `download::tests::happy_path_resolves_fetches_verifies_installs`.
7. **Cross-FS rename mitigation (G1).** `.part` lives **under
   `<models_dir>/.partial/`** (same filesystem as final), not under
   `$XDG_CACHE_HOME`. Test: `download::tests::part_file_lives_alongside_final`.
8. **Resume after partial corruption (B2).** On launch, if a `.part`
   exists, the resume path **re-hashes the entire `.part` from byte 0**
   (rolling SHA via `sha2::Sha256::update`) **before** sending
   `Range: bytes=<size>-`. On the *first* checksum mismatch after
   stream completion, delete `.part` and restart with informative
   banner. Test:
   `download::tests::resume_re_hashes_from_zero_then_continues`.
9. **HEAD validation (B3).** In `Resolving`, HEAD request must:
   (a) succeed with `2xx`, (b) `Content-Type` ∈ {`application/octet-stream`,
   `application/x-binary`, `binary/octet-stream`} — reject HTML/JSON,
   (c) `Content-Length == checksums.toml.size_bytes` ± 0.
   Failure → typed error + friendly banner; no `.part` opened.
   Test: `download::tests::html_response_aborts_before_writing_part`.
10. **Unknown model refusal (B1).** If user-typed model name is absent
    from the embedded `checksums.toml`, the **Download button is
    disabled** with tooltip "Unknown model — wait for next zwhisper
    release". No download attempt issued.
    Test: `download::tests::unknown_model_refuses_with_friendly_error`.
11. **HF rate-limit handling (F3).** HTTP 429 → state
    `Failed{rate_limited, retry_after_secs}`; Retry button is
    disabled-with-countdown until `Retry-After` elapses.
    Test: `download::tests::http_429_shows_retry_after_countdown`.
12. **Configurable base URL.** `~/.config/zwhisper/models.toml`
    `base_url` (default
    `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}.bin`)
    is parsed at launch; absent file → built-in default; malformed →
    typed error + red banner + fall back to default.
    Substitution is `String::replace("{model}", name)` only.
    Test: `config::tests::base_url_substitutes_model_name`.
13. **Cancel + close (B4).** Clicking Cancel transitions UI to
    `Cancelling{...}`; window close at this state waits up to
    `shutdown_timeout` for the runtime cancel to finish before drop.
    `.part` left on disk for resume.
    Test: `download::tests::cancel_then_close_leaves_consistent_part_file`.

### Whisper-CLI tab

14. Whisper-cli tab spawns one `detect_whisper_cli()` call on tab open;
    renders one of three states: `Found{path}` / `NotFound{install_hint}` /
    `MultipleFound{paths}`. **[Refresh]** button re-runs detection.
    Test: `tabs::whisper_cli::tests::refresh_picks_up_late_install`.

### Hotkey rebind tab

15. **Rebind button** spawns a tokio task → `AshpdAdapter::bind` wrapped
    in `tokio::time::timeout(HotkeyConfig::bind_timeout_secs)`. Outcomes:
    `Ok(...)` → relabel; `BindCancelled` → toast; `BindTimeout` → modal;
    `Unavailable` → tab fallback message, no Rebind button.
    Test: `tabs::hotkey::tests::rebind_outcomes_truth_table`.
16. **Tray rebind notification (E2).** After successful `bind`, settings
    emits a D-Bus signal `cz.zajca.Zwhisper1.Settings.HotkeyRebound`.
    Tray's `run_hotkey` listener subscribes and recreates its
    `HotkeySession` on receipt. Test (in `zwhisper-tray`):
    `hotkey::tests::tray_picks_up_settings_rebind_signal`.

### Cross-cutting

17. **Single-instance enforcement (E1).** `zwhisper-settings` claims
    D-Bus name `cz.zajca.Zwhisper1.Settings` at launch. If already
    taken, send a `Raise` method call to the existing instance and
    exit 0. Pattern mirrors `zwhisper-tray::single_instance` (`crates/
    zwhisper-tray/src/single_instance.rs:42-95`).
    Test: `app::tests::second_launch_raises_existing_window`.

### Wire-surface upgrades (zwhisper-core)

18. New `pub fn` re-exports in `zwhisper-core` (no signature changes,
    no behaviour changes, no new types):
    - `profile::paths::validate_name`
    - `profile::paths::user_override_path`
    - `profile::paths::shipped_path`
    - `profile::paths::user_profiles_dir`
    - `transcribe::models::models_dir() -> PathBuf` (NEW thin wrapper)
    - `transcribe::models::resolve_model`
    - `transcribe::discovery::detect_whisper_cli() -> Result<PathBuf, TranscribeError>`
      (NEW thin wrapper around `locate_whisper_cli`)
    Test (compile-time): `tests/wire_freeze.rs` adds `use` lines that
    fail-to-compile if any of the seven items become private again.

### Packaging + manual gate

19. `packaging/zwhisper-settings.desktop` shipped with `Categories=Settings;Audio;`
    and `Exec=zwhisper-settings`; validated by `desktop-file-validate`
    in CI. Test: `tests/desktop_file.rs::settings_file_parses_via_validator`.
20. `~/.config/zwhisper/models.toml` is read-only in M7 (no edit UI). A
    sample `models.toml.example` is shipped at
    `crates/zwhisper-settings/models.toml.example`.
21. `docs/M7-verification.md` documents the **Manual verification gate**
    (§ Manual verification gate below). Ship is gated on KDE Plasma 6
    1.0× and 1.5× passing (DoD #22 below).
22. **HiDPI scaling gate (A1).** Manual verification on KDE Plasma 6
    Wayland: at scaling factors 1.0× (X11), 1.0× (Wayland), 1.5×
    (Wayland), all four tabs render without clipped text or
    unclickable buttons. Failure → freeze M7 ship; M7.1 evaluates
    Slint swap-in. **Settings tab launch** also runs a one-shot scale
    detector: at startup, if scale ∉ {1.0, 2.0}, show a non-blocking
    warning banner with `FLTK_SCALING_FACTOR=1` override hint.

---

## Architectural decisions

### D1 — On-demand single-binary FLTK with `fltk-bundled`

Per IDEA.md § 10: `fltk = "1.5"` with features `["fltk-bundled",
"use-wayland"]`. Rationale: ~1 MB linked, ~5 MB bundled FLTK static
lib, hybrid X11/Wayland backend, smallest RAM idle of any Rust GUI
toolkit measured. Build chain (cmake + g++11 + curl + tar) is
documented as `makedepends` for M8 PKGBUILD.

Trade-off accepted: the HiDPI fractional-scaling story on KDE Plasma 6
is empirically unverified for this codebase. **Confidence: 65%**
(architect's verdict). Mitigation: manual matrix gate at DoD #22; if
gate fails → M7.1 evaluates Slint software-renderer fallback.

### D2 — Threading via `UiBridge` (mpsc + `awake_callback`)

FLTK widget mutation is main-thread-only. We spawn a tokio
**multi-thread** runtime on a side thread (one task at a time, but
multi-thread keeps the IO driver and timer driver decoupled from any
single worker). Cross-thread events flow: worker `tx.send(UiMessage)`
→ `fltk::app::awake_callback` → main loop drains `rx` → widget paint.
Channel is `tokio::sync::mpsc::UnboundedSender` (back-pressure-free —
the worker stalls only when CPU-blocked).

Runtime lifecycle: `spawn_runtime() → (UiBridge, Runtime)`. `Runtime`
owned by `main`'s stack; `Fl::run()` blocks until window close, then
`main` cancels the `CancellationToken` and calls
`rt.shutdown_timeout(Duration::from_secs(2))`.

Alternative considered and rejected: `fltk::app::add_idle()` polling.
Wastes CPU; awake-callback is the upstream-documented pattern.

### D3 — Compile-time SHA256 manifest, not runtime fetch

`crates/zwhisper-settings/checksums.toml` is `include_str!`'d at build
time. M8 release process bumps it. Trade-off: a new ggml model release
requires a zwhisper release to expose it (post-M7-punt B5). Accepted —
the M7 model set is fixed at five classics (tiny/base/small/medium/
large-v3). **No runtime manifest fetch** (no central signed source
exists per researcher) prevents the supply-chain hole where the
manifest itself is attacker-controlled.

### D4 — `.part` co-located with final destination

To avoid `EXDEV` on cross-filesystem `rename(2)` (G1 from
devils-advocate), `.part` lives at
`<models_dir>/.partial/ggml-<name>.bin.part`, not under
`$XDG_CACHE_HOME`. `<models_dir>` resolves via `dirs::data_local_dir()`
matching the runtime resolver path (`models.rs:96-99`). The
`.partial/` subdir keeps interrupted downloads visible in the same
hierarchy users already inspect.

### D5 — Single-instance via D-Bus name claim

Settings claims `cz.zajca.Zwhisper1.Settings`. Pattern mirrors
`zwhisper-tray::single_instance` (`crates/zwhisper-tray/src/single_instance.rs:42-95`).
On collision: send `org.freedesktop.Application.Activate` (or our own
`cz.zajca.Zwhisper1.Settings.Raise`) method call to the holder, exit 0.

### D6 — `Profiles1.reload` is conditional on daemon state

Settings calls `Profiles1.reload()` after a successful profile write
**only** when `Recorder1.GetStatus.state != "recording"`. During an
active recording, the user is asked (modal) whether to skip reload.
Rationale: M3 lock-in says session-bound state is captured at
`StartRecording`, but daemon's profile cache is shared — reloading
mid-recording could expose a partial-write window even though we
write atomically (race between `rename` syscall and daemon's TOML
parse on `inotify`-style reload). C1 from devils-advocate.

### D7 — Hotkey rebind notification via dedicated D-Bus signal

Settings emits `cz.zajca.Zwhisper1.Settings.HotkeyRebound` after
successful `bind`; tray subscribes. Alternative considered: tray
subscribes to portal's `ShortcutsChanged` (M6 B2). Rejected: the
portal signal may not be delivered on all backends (R3 from M6); the
dedicated signal is a known-good fallback. Both can coexist.

### D8 — Recorder1 + Profiles1 wire surface stays untouched

No D-Bus method or signal is added or modified on the daemon. The
only new wire-surface artifact is the
`cz.zajca.Zwhisper1.Settings.HotkeyRebound` signal owned by the
settings binary (and the tray subscribes as a client).

### D9 — `pub(crate) → pub` minimum-delta promotion

We expose the seven items in DoD #18 by re-exporting through
`zwhisper-core::lib.rs` rather than promoting `pub(crate)` traits to
`pub`. Two new thin wrappers (`models_dir`, `detect_whisper_cli`) wrap
trait-based code so the trait surface stays internal. This keeps the
review delta small.

### D10 — No `notify` / `inotify` profile-watching

Settings is on-demand and ephemeral. The user reopens Settings to see
disk changes. Rejecting `notify` keeps the dep tree small and the
threading model simple.

### D11 — Tests rely on fakes; live FLTK is manual-only

Unit tests cover `download.rs` (state machine via `wiremock`),
`config.rs` (URL substitution), `checksums.rs` (lookup), and the new
`zwhisper-core` re-exports. FLTK-bound tests use `Fl::enable_offscreen()`
for tab construction smoke tests only. The KDE 1.5× HiDPI render test
is **manual** and gated by DoD #22.

---

## Risks

Pulled from `/tmp/m7-devils-advocate.md`, ordered by severity. Each row
points to where it is addressed (DoD item or architectural decision).

| ID | Severity | Summary | Addressed by |
|---|---|---|---|
| A1 | Critical | FLTK on KDE Plasma 6 Wayland at 1.5× renders unusable widgets | DoD #22 (manual gate) + scale detector banner |
| B1 | Critical | Configurable URL + missing checksum = silent untrusted install | DoD #10 (refuse unknown models) + D3 |
| G1 | Critical | Atomic rename across filesystems silently fails (`EXDEV`) | DoD #7 + D4 (`.part` co-located) |
| C1 | High | Save profile during active recording corrupts daemon state | DoD #3 + D6 |
| B2 | High | `.part` resume after partial corruption produces poisoned model | DoD #8 (re-hash from zero on resume) |
| B3 | High | HF returns HTML 200 (captive portal) → SHA mismatch is the only signal | DoD #9 (HEAD Content-Type + Content-Length validation) |
| E1 | High | Multiple settings windows race to write same profile | DoD #17 + D5 |
| E2 | High | Hotkey rebind from settings collides with tray's listener | DoD #16 + D7 |
| G2 | High | Profile name field path traversal | DoD #4 (`validate_name` rules) |
| A2 | High | FLTK widget panic in callback poisons whole window | Risk-only mitigation: callback adapter pattern (no widget mutation inside closure body — body returns `Result<(), SettingsError>`); release profile sets `panic = "abort"`; `catch_unwind` wraps every callback. Documented limitation: FLTK's C++ FFI boundary is not unwind-safe; `panic = "abort"` is the safety net. Test: `tabs::profile::tests::callback_returning_err_logs_and_shows_inline_label`. |
| A3 | Medium | Window close mid-download leaks tokio task + `.part` | Risk-only: persist `.part.meta.json` `{ bytes_committed, sha256_state }` after every flushed chunk; resume trusts `bytes_committed`; `shutdown_timeout(2s)` is best-effort. Documented in M7-verification.md. |
| F1 | Medium | `fltk-bundled` build chain breaks on minimal Arch install | Documented in `crates/zwhisper-settings/README.md` and M8 PKGBUILD `makedepends`. Build-time error is OK at this stage. |
| C2 | Medium | Whisper-cli detector caches stale "not installed" | DoD #14 (Refresh button) |
| B4 | Medium | Cancel + immediate close = orphaned `.part` | DoD #13 (Cancelling state + close-wait) |
| F2 | Medium | < 60 MB RAM budget likely blown by tokio + reqwest TLS + FLTK | Single-thread runtime via `Builder::new_current_thread()` for downloader; cap chunk-buffer at 256 KB; document realistic budget post-measurement. Defer hard-cap enforcement to M8 perf review. |
| F3 | Medium | HuggingFace 429 rate-limit | DoD #11 |
| B5 | Low | Embedded checksums staleness | Out of scope (post-M7-punt). Documented in Models tab "Available models pinned at release time. Custom models: copy manually to `~/.local/share/zwhisper/models/`." |
| A4 | Low | Settings + `Profiles1.reload` while daemon is starting | `ServiceUnknown` D-Bus error treated identically to "daemon not running" — toast "Saved (daemon not yet running, will pick up on next start)". Test: `tabs::profile::tests::service_unknown_treated_as_daemon_off`. |

## Open questions for ship (verify during implementation)

1. **R1 — measured RAM footprint on idle and during download.** Run
   `/usr/bin/time -v target/release/zwhisper-settings`; record idle RSS
   and peak RSS during a `large-v3` download. If > 80 MB, flag for
   M8 perf review. Recorded in `docs/M7-verification.md`.
2. **R2 — KDE Plasma 6 HiDPI fractional-scale matrix.** DoD #22.
3. **R3 — Wayland vs X11 backend auto-detection.** Confirm
   `WAYLAND_DISPLAY` flips backend without manual `FLTK_BACKEND` env.
   On wlroots without portal: confirm graceful degradation (Hotkey tab
   shows "Portal unavailable").
4. **R4 — `Profiles1.reload` round-trip latency under load.** Just
   measure; no contract change.
5. **R5 — `.part.meta.json` durability vs throughput.** Measure
   download throughput with and without per-chunk `fsync_data()`. If
   throughput drops >2×, switch to per-chunk-count fsync (every 16
   chunks) and lengthen the on-resume re-hash window.

---

## Implementation tasks

Tech-lead style decomposition. Each task is owner-agnostic and carries
(a) a one-line goal, (b) the files it owns, (c) its dependency on
previous tasks. Group letters (A/B/C/D) correspond to "parallel
groups" — letters can run concurrently; within a letter, ordering
matters.

### A. Workspace + new crate skeleton + FLTK app shell

- **A1.** Add `fltk = { version = "1.5", features = ["fltk-bundled", "use-wayland"] }`
  and `sha2 = "0.10"` to `Cargo.toml [workspace.dependencies]`. Verify
  `cargo metadata` resolves. Owns: `Cargo.toml`. Dep: —.
- **A2.** Create `crates/zwhisper-settings/Cargo.toml` per architecture
  § 1.1. Owns: `crates/zwhisper-settings/Cargo.toml`. Dep: A1.
- **A3.** Create `crates/zwhisper-settings/src/{main.rs,app.rs,
  runtime.rs,error.rs,config.rs,checksums.rs,download.rs}` and
  `tabs/{mod.rs,profile.rs,models.rs,hotkey.rs,whisper_cli.rs}` per
  architecture § 1.2. Owns: `crates/zwhisper-settings/src/**`. Dep: A2.
- **A4.** Implement `runtime.rs` UiBridge (multi-thread runtime + mpsc +
  `awake_callback`). Implement `error.rs` `SettingsError` enum.
  Owns: `runtime.rs`, `error.rs`. Dep: A3.
- **A5.** Implement `app.rs` — `App` struct, FLTK window construction,
  `Tabs` widget with four `Group` children (one per tab),
  `UiMessage` router, single-instance D-Bus claim (DoD #17), HiDPI
  scale detector banner (DoD #22). Owns: `main.rs`, `app.rs`. Dep: A4.
- **A6.** Add settings binary to workspace `members` (already implicit
  glob; verify). Owns: `Cargo.toml`. Dep: A2.
- **A7.** Manual smoke test: `cargo run -p zwhisper-settings` opens an
  empty 4-tab window on Wayland and X11. Dep: A5.

### B. Profile editor tab

- **B1.** Implement `tabs::profile::ProfileTab::build(parent, bridge)`
  — list pane (left), form pane (right), buttons (Save / Revert / Show
  diff / Clone). Owns: `tabs/profile.rs`. Dep: A5, D2.
- **B2.** Wire list pane to `zwhisper_core::profile::listing::list_entries()`
  — three sections by `ProfileEntry::source`. Owns: same. Dep: B1, D2.
- **B3.** Wire form pane: serialize `Profile` struct → form widgets
  (`Input` / `Choice` / `Check`); on Save, deserialize back, run
  `Profile::validate()`. Owns: same. Dep: B1.
- **B4.** Implement atomic save: `tempfile::NamedTempFile` →
  `persist(target)`. After success, conditionally call
  `Profiles1Proxy::reload` per D6 — query `Recorder1.GetStatus` first
  (DoD #3). Owns: same. Dep: B3, D2.
- **B5.** Implement Clone dialog with `paths::validate_name` (DoD #4).
  Owns: same. Dep: B3, D2.
- **B6.** Implement `diff_lines(user: &str, shipped: &str) -> String` —
  hand-rolled line diff (DoD #5). Owns: same. Dep: B1.
- **B7.** Tests: list_groups_by_source, save_validates_then_atomic_writes_then_reloads,
  save_during_recording_warns_and_defers_reload, clone_name_traversal_rejected,
  diff_marks_added_removed_lines, callback_returning_err_logs_and_shows_inline_label,
  service_unknown_treated_as_daemon_off. Owns: same. Dep: B1–B6.

### C. Model downloader tab

- **C1.** Embed `crates/zwhisper-settings/checksums.toml` via
  `include_str!`. Implement `checksums.rs` parse + `lookup(name)`.
  Populate with the five classics (tiny / base / small / medium /
  large-v3). Owns: `checksums.toml`, `checksums.rs`. Dep: A3.
- **C2.** Implement `config::ModelsConfig` — read
  `~/.config/zwhisper/models.toml` `base_url`, default to HF URL,
  `{model}` substitution (DoD #12). Owns: `config.rs`. Dep: A3.
- **C3.** Implement `download::ModelDownloader` state machine: `Idle →
  Resolving → Fetching → Verifying → Installed | Failed | Cancelled`.
  Use `reqwest` (rustls + stream) + `sha2` + `tokio_util::sync::CancellationToken`.
  HEAD validation (DoD #9). Range resume + re-hash from zero (DoD #8).
  Atomic rename `<models_dir>/.partial/` → `<models_dir>/` (DoD #7).
  HTTP 429 → Failed{rate_limited} (DoD #11). Owns: `download.rs`.
  Dep: C1, C2, D2.
- **C4.** Implement `tabs::models::ModelsTab` — list of five models
  (each row: name, size, installed?, [Download] / [Resume] /
  [Cancel] / [Failed: Retry]); progress bar; status label.
  Refuse-unknown-model handling (DoD #10).
  Owns: `tabs/models.rs`. Dep: A5, C3.
- **C5.** Implement persist `.part.meta.json` after every flushed
  chunk for crash-resume (A3 mitigation). Owns: `download.rs`. Dep: C3.
- **C6.** Tests: happy_path_resolves_fetches_verifies_installs,
  part_file_lives_alongside_final, resume_re_hashes_from_zero_then_continues,
  html_response_aborts_before_writing_part, unknown_model_refuses_with_friendly_error,
  http_429_shows_retry_after_countdown,
  cancel_then_close_leaves_consistent_part_file,
  base_url_substitutes_model_name. Owns: `download.rs`,
  `config.rs` test mods. Dep: C3, C5.

### D. Whisper-CLI + Hotkey + zwhisper-core surface upgrades

- **D1.** Promote (or wrap) the seven items in DoD #18:
  - `zwhisper-core::profile::paths::{validate_name, user_override_path,
    shipped_path, user_profiles_dir}` → `pub`
  - `zwhisper-core::transcribe::models::{models_dir, resolve_model}` →
    `pub`. Add new thin wrapper `pub fn models_dir() -> PathBuf` that
    does not expose `ModelDirProvider` trait.
  - `zwhisper-core::transcribe::discovery::detect_whisper_cli() ->
    Result<PathBuf, TranscribeError>` → NEW thin wrapper around
    `locate_whisper_cli` (which stays `pub(crate)`).
  - Add `use` lines in `crates/zwhisper-ipc/tests/wire_freeze.rs` so a
    future re-privatization fails to compile (DoD #18).
  Owns: `crates/zwhisper-core/src/{profile/paths.rs,profile/mod.rs,
  transcribe/models.rs,transcribe/discovery.rs,transcribe/mod.rs,
  lib.rs}`, `crates/zwhisper-ipc/tests/wire_freeze.rs`. Dep: —.
- **D2.** Implement profile editor's D-Bus client wrapper:
  `app::client::ProfilesClient` and `RecorderClient` — small typed
  facades over `zwhisper_ipc::Profiles1Proxy` and
  `Recorder1Proxy`, both treating `ServiceUnknown` as "daemon off"
  (returns `None` / `Ok(StatusUnavailable)`). Owns: `app.rs` or
  `app/client.rs`. Dep: A4.
- **D3.** Implement `tabs::whisper_cli::WhisperCliTab` — three states
  (Found / NotFound / MultipleFound), Refresh button (DoD #14).
  Owns: `tabs/whisper_cli.rs`. Dep: A5, D1.
- **D4.** Implement `tabs::hotkey::HotkeyTab` — current binding label,
  Rebind button, fallback states (`BindCancelled`, `BindTimeout`,
  `Unavailable`). Wraps `AshpdAdapter::bind` in
  `tokio::time::timeout(HotkeyConfig::bind_timeout_secs)` (DoD #15).
  After successful bind, emits D-Bus signal
  `cz.zajca.Zwhisper1.Settings.HotkeyRebound` (DoD #16). Owns:
  `tabs/hotkey.rs`. Dep: A5, D2.
- **D5.** In `crates/zwhisper-tray/src/hotkey.rs`, subscribe to the
  new `cz.zajca.Zwhisper1.Settings.HotkeyRebound` signal and recreate
  the `HotkeySession` on receipt (DoD #16). Owns:
  `crates/zwhisper-tray/src/hotkey.rs`. Dep: D4.
- **D6.** Tests: `tabs::whisper_cli::tests::refresh_picks_up_late_install`,
  `tabs::hotkey::tests::rebind_outcomes_truth_table`,
  `crates/zwhisper-tray/src/hotkey.rs::tests::tray_picks_up_settings_rebind_signal`.
  Owns: same files. Dep: D3, D4, D5.

### E. Packaging + docs + manual gate

- **E1.** `packaging/zwhisper-settings.desktop` per DoD #19. Owns:
  `packaging/zwhisper-settings.desktop`. Dep: —.
- **E2.** `crates/zwhisper-settings/models.toml.example` per DoD #20.
  Owns: same. Dep: C2.
- **E3.** `docs/M7-verification.md` — manual matrix per DoD #21–#22 +
  open questions R1–R5. Owns: same. Dep: A7, B7, C6, D6.
- **E4.** `crates/zwhisper-settings/README.md` — fltk-bundled build
  deps, run hints, configurable URL example. Owns: same. Dep: —.
- **E5.** Update `IDEA.md § 11` row M7 with "✅ shipped <date>" after
  manual gate passes. Owns: `IDEA.md`. Dep: E3.

### Suggested team sizing

- Group A (skeleton + app shell) — 1 teammate, ~7 tasks.
- Group B (profile editor) — 1 teammate, ~7 tasks.
- Group C (model downloader) — 1 teammate, ~6 tasks.
- Group D (whisper-cli + hotkey + zwhisper-core surface) — 1 teammate,
  ~6 tasks (also owns the tray-side D5 task because it is the only
  zwhisper-tray edit).
- Group E (packaging + docs) — folded into Group A's teammate.
- 1 product-engineer for the quality gate. Within the CLAUDE.md "Team
  Rules" cap of 5.

Total: 4 implementation teammates + 1 product-engineer.

Dependency graph:

```
A → B, C, D (all three feature groups depend on A's app shell)
D1 (zwhisper-core surface) → B (profile editor needs paths::*),
                             D3 (whisper-cli tab)
D2 (D-Bus clients) → B4 (Profiles1.reload + Recorder1.GetStatus),
                     D4 (hotkey tab)
C1 (checksums) → C3 (download state machine) → C4 (models tab)
D4 (hotkey tab signal emission) → D5 (tray subscribes)
A, B, C, D → E (docs/packaging close out)
```

---

## Test matrix

New tests by file (name → assertion summary).

| File | Test | Asserts |
|---|---|---|
| `zwhisper-settings/src/checksums.rs` | `parse_embedded_manifest_lists_five_classics` | tiny, base, small, medium, large-v3 each present with sha256 + size_bytes |
| `zwhisper-settings/src/checksums.rs` | `lookup_unknown_returns_none_with_typed_error` | DoD #10 — refuse-unknown precondition |
| `zwhisper-settings/src/config.rs` | `defaults_to_huggingface_url_when_file_absent` | DoD #12 |
| `zwhisper-settings/src/config.rs` | `base_url_substitutes_model_name` | DoD #12 — `{model}` → "tiny" |
| `zwhisper-settings/src/config.rs` | `malformed_toml_falls_back_with_typed_error` | DoD #12 |
| `zwhisper-settings/src/download.rs` | `happy_path_resolves_fetches_verifies_installs` | DoD #6 |
| `zwhisper-settings/src/download.rs` | `part_file_lives_alongside_final` | DoD #7 + D4 |
| `zwhisper-settings/src/download.rs` | `resume_re_hashes_from_zero_then_continues` | DoD #8 |
| `zwhisper-settings/src/download.rs` | `html_response_aborts_before_writing_part` | DoD #9 — Content-Type guard |
| `zwhisper-settings/src/download.rs` | `unknown_model_refuses_with_friendly_error` | DoD #10 |
| `zwhisper-settings/src/download.rs` | `http_429_shows_retry_after_countdown` | DoD #11 |
| `zwhisper-settings/src/download.rs` | `cancel_then_close_leaves_consistent_part_file` | DoD #13 |
| `zwhisper-settings/src/download.rs` | `kill_mid_chunk_then_resume_succeeds` | A3 |
| `zwhisper-settings/src/download.rs` | `content_length_mismatch_aborts` | DoD #9 second guard |
| `zwhisper-settings/src/app.rs` | `second_launch_raises_existing_window` | DoD #17 |
| `zwhisper-settings/src/tabs/profile.rs` | `list_groups_by_source` | DoD #1 |
| `zwhisper-settings/src/tabs/profile.rs` | `save_validates_then_atomic_writes_then_reloads` | DoD #2 |
| `zwhisper-settings/src/tabs/profile.rs` | `save_during_recording_warns_and_defers_reload` | DoD #3 + D6 |
| `zwhisper-settings/src/tabs/profile.rs` | `clone_name_traversal_rejected` | DoD #4 |
| `zwhisper-settings/src/tabs/profile.rs` | `diff_marks_added_removed_lines` | DoD #5 |
| `zwhisper-settings/src/tabs/profile.rs` | `callback_returning_err_logs_and_shows_inline_label` | A2 mitigation |
| `zwhisper-settings/src/tabs/profile.rs` | `service_unknown_treated_as_daemon_off` | A4 mitigation |
| `zwhisper-settings/src/tabs/whisper_cli.rs` | `refresh_picks_up_late_install` | DoD #14 |
| `zwhisper-settings/src/tabs/hotkey.rs` | `rebind_outcomes_truth_table` | DoD #15 |
| `zwhisper-tray/src/hotkey.rs` | `tray_picks_up_settings_rebind_signal` | DoD #16 + D7 |
| `zwhisper-ipc/tests/wire_freeze.rs` | `m7_pub_surface_does_not_regress` | DoD #18 (compile-time) |
| `zwhisper-ipc/tests/wire_freeze.rs` | `recorder_wire_format_unchanged_in_m7` | DoD wire-freeze |
| `zwhisper-ipc/tests/wire_freeze.rs` | `profiles_wire_format_unchanged_in_m7` | DoD wire-freeze |
| `tests/desktop_file.rs` | `settings_file_parses_via_validator` | DoD #19 |

Approx. count: **29 new tests** (24 unit, 3 integration, 2 wire-freeze
checkpoints). Workspace test total target after M7: **515 (M6) + ~29 ≈
544**.

---

## Wire-surface contract

### Frozen — touched by NO code in M7

- `Recorder1.{StartRecording, StopRecording, GetStatus}` —
  `crates/zwhisper-ipc/src/recorder.rs:60-86`. Settings only **calls
  GetStatus** as a read-only client; never modifies the trait or
  daemon impl.
- `Profiles1.{list, list_v2, get_active, set_active, reload}` —
  `crates/zwhisper-ipc/src/profiles.rs:43-65`. Settings calls
  `reload` after writing a profile; never modifies the trait.
- `Recorder1.StateChanged` / `RecordingComplete` signal payloads —
  unchanged.
- `cz.zajca.Zwhisper1.Tray` D-Bus name (M4 + M6) — unchanged.
- All M5 cloud-backend code paths (`secrets/resolver.rs`,
  `transcribe/deepgram.rs`) — unchanged.
- All M6 hotkey-toggle code paths (`zwhisper-hotkey/**`,
  `zwhisper-cli/src/commands/{toggle,hotkey}.rs`,
  `zwhisper-tray/src/hotkey.rs` except for D5 below) — unchanged.

### Additive — new in M7, evolution-safe

- New crate `zwhisper-settings` with binary target.
- New D-Bus name `cz.zajca.Zwhisper1.Settings` (settings binary
  claims; tray does not).
- New D-Bus signal `cz.zajca.Zwhisper1.Settings.HotkeyRebound` —
  emitted by settings, subscribed by tray.
- New file `~/.config/zwhisper/models.toml` (read-only in M7).
- New file `crates/zwhisper-settings/checksums.toml` (compile-time
  embedded).
- New `pub` re-exports in `zwhisper-core` (DoD #18) — no signature
  changes.
- New file `<models_dir>/.partial/ggml-<name>.bin.part` (and sidecar
  `.part.meta.json`) — cleaned by successful download or by user.
- New tray code in `zwhisper-tray/src/hotkey.rs` for D5 subscription
  — additive, gated by signal arrival.

### Forbidden — must NOT change

- No new `Recorder1` or `Profiles1` methods or signals.
- No daemon code modification (`crates/zwhisperd/**` must be
  untouched).
- No new dep added beyond `fltk` and `sha2` (and the dev-dep
  `wiremock` which is already present).
- No `.desktop` entry for the daemon or CLI (only for settings).
- No environment-variable backdoors except those documented in
  M7-verification.md (`FLTK_SCALING_FACTOR`, `FLTK_BACKEND`).

---

## Manual verification gate

Mirroring `docs/M6-verification.md`. Ship is gated on these scenarios
all passing.

| # | Scenario | Acceptance |
|---|---|---|
| MV-1 | KDE Plasma 6 Wayland @ 1.0× scaling | All four tabs render without clipping; profile save round-trips; model download (tiny) completes; hotkey rebind dialog opens. |
| MV-2 | KDE Plasma 6 Wayland @ 1.5× scaling (**A1 gate**) | Same as MV-1. Failure → freeze ship; open M7.1 for Slint. |
| MV-3 | GNOME 47+ Wayland @ 1.0× | Same as MV-1. Hotkey rebind via GNOME's portal-gnome backend. |
| MV-4 | sway/wlroots @ 1.0× | All tabs except Hotkey render; Hotkey tab shows "Portal unavailable" graceful banner. |
| MV-5 | KDE Plasma 6 X11 @ 1.0× | Same as MV-1; FLTK auto-detects X11 backend (no `WAYLAND_DISPLAY`). |
| MV-6 | RAM footprint | `/usr/bin/time -v target/release/zwhisper-settings` idle RSS < 60 MB; peak during `large-v3` download < 80 MB. |
| MV-7 | Single-instance | Two consecutive `zwhisper-settings &` invocations: second exits 0, first window raises. |
| MV-8 | Save-during-recording (DoD #3) | Start recording from CLI; open settings; save profile; observe modal warning + skipped reload. |
| MV-9 | Captive portal simulation (DoD #9) | Run a local HTTP server returning HTML 200; point `models.toml` at it; click Download → "Endpoint returned non-binary response" banner. |
| MV-10 | Cross-FS rename (DoD #7) | Mount `<models_dir>` as separate filesystem; download tiny; assert no `EXDEV` and `.bin` final lands correctly. |

Each row is recorded in `docs/M7-verification.md` with screenshots and
timestamps before M7 is marked shipped.

---

## Out of scope (deferred to M7.1+ / MX)

1. **Secrets editor** (deferred MX, post-M8).
2. **Slint software-renderer fallback** (M7.1, only if MV-2 fails).
3. **Dark theme / theming.**
4. **Model deletion confirmation flow** (rm from CLI suffices).
5. **Model auto-update / "newer version available" detection.**
6. **Profile import/export (zip / share URL).**
7. **Live `inotify` watch on profile dir** (close + reopen instead).
8. **Translation / i18n** (English-only strings).
9. **`hotkey.toml` editor for `cooldown_ms` / `debounce_ms`** (defaults
   only; expert users edit by hand).
10. **`models.toml` editor** (read-only in M7).
11. **`large-v3-turbo` and other newer ggml variants** (B5 — embedded
    manifest is fixed at five classics).
12. **Custom keyring backend selection** (covered by post-M8 secrets
    editor).

---

## Coordination notes

- **CLAUDE.md adherence:** all code in English; per-file < 600 lines;
  no `unwrap`/`expect`/`panic`/`todo`/`dbg!`. `unsafe_code = "deny"`
  (workspace-wide). All callbacks return `Result<(), SettingsError>`;
  no silent defaults — every error is logged via `tracing` and
  surfaced to the user.
- **No mocks in production code.** D-Bus clients (D2) treat
  `ServiceUnknown` as a typed `Err`, never silently succeed; tests use
  `wiremock` and `FakePortal` (M6's existing fake).
- **No silent defaults.** `models.toml` absent → use **explicitly
  documented** built-in default URL; malformed → typed error + visible
  banner.
- **Configurable values:** `bind_timeout_secs` (HotkeyConfig — already
  M6); `base_url` (models.toml — new in M7). No magic numbers in
  source.

---

## Plan headlines (10-bullet summary)

1. **New crate `zwhisper-settings`** as on-demand FLTK binary; spawned
   from tray's "Settings…" menu. Four tabs: Profiles, Models,
   Whisper-CLI, Hotkey.
2. **No daemon modification.** Wire surface is read-only client +
   `Profiles1.reload` + new `Settings.HotkeyRebound` signal.
3. **Threading:** tokio multi-thread runtime on side thread; FLTK on
   main; `mpsc::UnboundedSender` + `Fl::awake_callback` bridge.
4. **Profile editor:** atomic write via `tempfile::persist`, validates
   before write; `Profiles1.reload` deferred when daemon is recording.
5. **Model downloader:** state machine with HEAD validation
   (Content-Type + Content-Length), Range-resume with re-hash from
   byte 0 to handle crash-corrupted `.part`, atomic same-FS rename.
6. **Compile-time SHA256 manifest** (`crates/zwhisper-settings/checksums.toml`)
   — five classics only. Unknown models refused (no silent install).
7. **Configurable model base URL** at `~/.config/zwhisper/models.toml`
   with `{model}` substitution; HF default.
8. **Hotkey rebind** reuses M6 `AshpdAdapter::bind`; emits D-Bus
   signal so the tray refreshes its `HotkeySession`.
9. **Single-instance** via D-Bus name claim
   `cz.zajca.Zwhisper1.Settings`; second launch raises existing
   window.
10. **HiDPI fractional-scale gate (A1)** is the ship-blocker:
    KDE Plasma 6 Wayland @ 1.5× must pass MV-2 before merge. Failure
    → M7.1 evaluates Slint swap-in. Pre-allocate budget.

Estimated ~29 new tests; ~544 workspace tests after M7.
