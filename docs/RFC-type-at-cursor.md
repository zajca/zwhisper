# RFC: Type-at-Cursor Output — `wtype`-backed transcript insertion

## Status

Accepted (open decisions OD1–OD4 resolved 2026-06-04). Ready for implementation.

This RFC promotes **type-at-cursor** from the R&D queue (IDEA.md §12, §13)
into a committed, opt-in delivery destination. It is a design document, not an
implementation plan. It extends RFC-daemon-role Feature 3 (Session Delivery):
the daemon resolves outputs and carries them in `Jobs1.JobCompleted`; the
session-bound `zwhisper deliver --listen` consumer is the only component with
graphical access and is therefore where typing happens.

Scope-defining product decisions, taken before this RFC and treated here as
binding constraints:

- **Mechanism: `wtype`** (the Wayland `virtual-keyboard-v1` protocol). No
  `libei`, no `ydotool`/uinput, no X11/`xdotool`.
- **Supported compositors: wlroots only** (Sway, Hyprland). Both **GNOME/Mutter**
  ([mutter#4124](https://gitlab.gnome.org/GNOME/mutter/-/work_items/4124)) and
  **KDE/KWin** (Plasma 6 returns *"Compositor does not support the virtual
  keyboard protocol"*, tested Nov 2025) lack `virtual-keyboard-v1` for these
  clients, so `wtype` cannot work there. On any unsupported session the feature
  degrades to the existing clipboard/notification path rather than failing. KDE
  users would need a future `libei` backend (N2) — KWin supports EIS, wlroots
  does not, which is the mirror image of `wtype`'s reach.

## Summary

Today the delivery consumer honours three output tags: `file` (written by the
daemon), `clipboard` (injected via `arboard`, gated by the F3.3 intent guard),
and `notification`. Cursor-position insertion does not exist — the user pastes
manually with Ctrl+V.

This RFC adds a fourth `OutputDest` variant, `TypeAtCursor`, that types the
transcript into the focused window via `wtype`. It reuses the existing
data flow end-to-end (schema → `encode_outputs` → `JobCompleted aas` →
consumer dispatch) and the existing safety model (intent guard + size ceiling +
notify-with-action fallback), adding only:

1. a new enum variant and its `aas` encoding,
2. a `TypeSink` that shells out to `wtype -` over stdin inside
   `spawn_blocking`,
3. a **stricter** pure intent guard (`decide_type`) — typing into whatever
   window happens to be focused is more hostile than overwriting the clipboard,
   so the bar for auto-typing is higher,
4. a best-effort **compositor capability gate** that turns the feature into a
   clipboard/notify fallback on unsupported sessions (GNOME, or any session
   where `wtype` is absent/non-functional).

## Goals

- G1. A profile can declare `[[output]]` with `type = "type_at_cursor"` and,
  for a foreground job on a supported compositor, the transcript is typed at
  the cursor with no manual paste.
- G2. On an unsupported session (GNOME/Mutter, missing `wtype`, or a `wtype`
  failure) the user **never silently loses the transcript**: it degrades to
  clipboard + notification.
- G3. Typed output is **keyboard-layout independent** — the transcript types
  identically regardless of the user's active XKB layout (cs/en/…). This is a
  property `wtype` already provides by uploading its own keymap; the RFC only
  requires we do not defeat it.
- G4. No new always-running process, no new D-Bus surface, no compositor/clipboard
  access added to the daemon. Everything lands in the existing `deliver`
  consumer.
- G5. The decision logic (`decide_type`) and the command construction are pure
  / injectable and exhaustively unit-tested, mirroring `decide_clipboard`.

## Non-goals

- N1. Non-wlroots typing — **KWin and Mutter** (Plasma, GNOME). Neither
  implements `virtual-keyboard-v1` for these clients, so `wtype` cannot reach
  them; they get the clipboard/notify fallback. Revisit via N2, not by
  extending `wtype`.
- N2. A `libei`/`reis` backend (the path that *would* cover KWin/Mutter). The
  Rust bindings are still experimental (`reis`: "API subject to change", no
  published releases; `enigo`'s `libei` feature is behind a flag due to bugs)
  and wlroots has no EIS server at all
  ([wlroots#2378](https://github.com/swaywm/wlroots/issues/2378) closed). When
  `reis` stabilises, a second backend can be added behind the same
  `OutputDest::TypeAtCursor` variant without a schema change — the consumer
  would pick `wtype` vs `libei` by compositor capability.
- N3. `ydotool`/uinput. It is focus-unaware and needs `ydotoold` + `input`
  group membership; unsuitable as a default.
- N4. X11 (`xdotool`). The delivery consumer is already Wayland-gated
  (`WAYLAND_DISPLAY`); X11 typing is not in scope.
- N5. True push-to-talk. Unrelated; tracked separately.
- N6. A persistent retry/outbox for typed delivery. Best-effort only, exactly
  like clipboard (RFC-daemon-role F3.2): a missed signal means the transcript
  is on disk.

## Background: why `wtype`, why Wayland-only

| Compositor (target machine: Sway/wlroots) | `wtype` (vkbd-v1) | `libei`/EIS | Verdict |
|---|---|---|---|
| **wlroots** (Sway, Hyprland) | works | no EIS server | **supported** |
| **KWin** (Plasma 6) | not implemented (vkbd-v1 absent) | works | **not supported** (libei-only, N2) |
| **Mutter** (GNOME) | not implemented | works | **not supported** (N1) |

`wtype` and `libei` have **disjoint** compositor reach: `wtype` covers wlroots,
`libei` covers KWin/Mutter. This RFC ships the `wtype`/wlroots half now; the
`libei` half is deferred (N2) until `reis` stabilises.

`wtype` reads text to type from a positional argument **or from stdin** via the
`-` form (`wtype -`). We use **stdin**: it sidesteps `ARG_MAX`, removes any
need for the `--` dash-separator dance, types UTF-8 and newlines as-is, and
mirrors the established `printf '%s' "$text" | wl-copy` pattern in
`contrib/bin/zwhisper-dictate`. Default inter-keystroke delay is `0`.

## Feature specification

### F1 — Schema: `OutputDest::TypeAtCursor`

Add a fourth variant to `crates/zwhisper-core/src/profile/schema.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputDest {
    File { path: String },
    Clipboard,
    Notification,
    TypeAtCursor, // TOML: type = "type_at_cursor"
}
```

The serde tag field is literally named `type`; the variant renders as
`type_at_cursor` under `rename_all = "snake_case"`. Profile usage:

```toml
[[output]]
type = "type_at_cursor"
```

- **Compatibility.** Additive for any reader that knows the variant. An *older*
  binary reading a profile that uses `type_at_cursor` will fail to deserialize —
  acceptable: only users who opt into the new feature write the new value, and
  `schema_version` stays `1` (no migration; nothing to rewrite in existing
  profiles).
- `Profile::validate` gains no new invariant. `TypeAtCursor` composes freely
  with other outputs (see F6).

### F2 — Daemon encoding (`aas` payload)

`crates/zwhisperd/src/jobs/mod.rs::encode_outputs` gains one arm, keeping the
"first element is the tag" convention:

```rust
OutputDest::TypeAtCursor => vec!["type_at_cursor".to_owned()],
```

No change to the `JobCompleted` signature — `aas` already carries arbitrary
tag vectors. The `encode_outputs_covers_all_variants` test is extended.

### F3 — Consumer dispatch

`crates/zwhisper-cli/src/commands/deliver/mod.rs::handle_completed` gains a
match arm:

```rust
Some("type_at_cursor") => {
    handle_type(&type_sink, &clipboard, submit_mode, transcript_path, bytes).await;
}
```

`handle_type` drives the F4 guard and the F5 sink, with the F6 fallback chain.

### F4 — `decide_type`: a stricter intent guard

A pure function alongside `decide_clipboard`, same shape, **stricter ceiling
and no "type later" path**:

```rust
pub(crate) fn decide_type(submit_mode: &str, bytes: u64, max_bytes: u64) -> TypeDecision {
    if bytes > max_bytes {
        return TypeDecision::NotifyWithAction; // too large to type char-by-char
    }
    match submit_mode {
        "foreground" => TypeDecision::Type,
        _ => TypeDecision::NotifyWithAction, // detached/auto/unknown: never type
    }
}
```

Rationale, by priority:

1. **Size ceiling first.** `wtype` types character-by-character; a large blob
   would hold the virtual keyboard for a long time and spray keystrokes into the
   focused app. The typing ceiling is **smaller** than the clipboard ceiling —
   `TYPE_MAX_BYTES = 8_192` (design ceiling, not a tunable; CLAUDE.md "no
   hardcoded values" → named const). 8 KB ≈ 4–5k chars ≈ ~6–8 min of speech,
   covering any realistic dictation while a 100 KB meeting transcript never
   starts typing. Over the ceiling → notify-with-action.
2. **Foreground only.** Identical reasoning to `decide_clipboard` but the stakes
   are higher: typing into whatever window has focus minutes after a detached
   job finished could fire keystrokes into an unrelated app (a shell, a chat,
   a password field). Detached / auto / any unknown future mode → never type.

Unit tests mirror the `decide_clipboard` suite (boundary at the ceiling,
foreground/detached/auto/unknown/empty modes, size-beats-intent).

### F5 — `TypeSink`: invoking `wtype`

A small struct in `deliver/sink.rs`, symmetric with `ClipboardSink`:

- Construction is cheap; it holds no handle (each completion spawns one
  short-lived `wtype` process).
- `async fn type_text(&self, text: &str) -> Result<(), String>` runs inside
  `spawn_blocking` and:
  1. spawns `wtype -` (argv is fixed and constant — **no shell**, so no command
     injection; `-` means "read text from stdin", so the transcript is never an
     argv element and dash-prefixed text needs no `--`),
  2. writes `text` to the child's stdin and closes it (no `-d` delay flag:
     `WTYPE_KEYSTROKE_DELAY_MS = 0`, a named const left at `wtype`'s default;
     raised only if the manual Sway pass shows dropped characters),
  3. waits for exit with a **timeout** (`WTYPE_TIMEOUT`, named const) so a
     compositor that accepts the connection but never drains keystrokes cannot
     wedge the consumer; on timeout the child is killed and an error returned,
  4. maps non-zero exit / spawn error / timeout to `Err(String)` so the caller
     can run the F6 fallback.
- The synchronous `std::process` work lives in `spawn_blocking`, consistent with
  how `notify` and `ClipboardSink::inject` keep the tokio reactor responsive.
- **Injectable runner.** The actual spawn is behind a tiny trait/closure
  (`CommandRunner`) so unit tests assert the constructed argv + stdin payload
  without a live compositor, and can simulate exit codes / timeouts.

### F6 — Capability gate + fallback chain

Typing can be unavailable for three reasons: an unsupported compositor
(KWin/Mutter — no `virtual-keyboard-v1`), the `wtype` binary missing, or a
`wtype` runtime failure. The consumer resolves this as a **gate then fallback**:

1. **Upfront gate (cheap, before spawning).**
   - Already-present Wayland gate (`WAYLAND_DISPLAY`) stays.
   - **`wtype` presence:** detect via PATH lookup once (cached). Absent → fallback.
   - **Authoritative capability signal is the spawn itself.** On an unsupported
     compositor `wtype` exits **non-zero quickly** with *"Compositor does not
     support the virtual keyboard protocol"* — it does **not** hang — so the
     attempt-and-fallback (step 3) is the source of truth, not a fragile
     desktop-name sniff.
   - **Optional log hint (best-effort, not a gate):** read
     `XDG_CURRENT_DESKTOP` / `XDG_SESSION_DESKTOP` (case-insensitive, handles
     `ubuntu:GNOME`) only to *enrich the fallback log/notification* with the
     likely reason (e.g. "looks like GNOME/KWin — `wtype` needs wlroots"). It
     never suppresses the attempt: a misreported environment must not block a
     compositor that would actually work.
2. **Decision (F4).** If `decide_type` says `NotifyWithAction` (too large /
   not foreground), skip typing and notify.
3. **Attempt + fallback on failure.** On `Type`: read the transcript, call
   `TypeSink::type_text`. On **any** failure (gate, spawn, non-zero, timeout):
   - best-effort inject the text into the **clipboard** (so the user still has
     it), then
   - raise a notification: *"Typed delivery unavailable — transcript copied to
     clipboard (Ctrl+V), or run `zwhisper output last --to clipboard`."*

   This reuses `ClipboardSink` already owned by the consumer; no duplicate
   logic. The clipboard inject runs **even on a `type_at_cursor`-only profile**
   that never listed `clipboard` (OD4 resolved): typing only ever runs for a
   foreground job, so the user is actively waiting and Ctrl+V is the fastest
   recovery; it is always announced via the notification, and the transcript is
   on disk regardless. If the profile *also* lists `clipboard`, that entry runs
   independently — the fallback inject is idempotent (same text) and harmless.

Failure is always **observable**: a WARN log + a user-visible notification,
never a swallowed error (CLAUDE.md "no silent failures").

## Data flow (end to end)

```
profile.toml  [[output]] type="type_at_cursor"
      │  (read once by daemon at completion, F3.1)
      ▼
zwhisperd: encode_outputs ──► ["type_at_cursor"]
      │
      ▼  Jobs1.JobCompleted(... aas outputs ...)  (best-effort, F3.2)
      ▼
zwhisper deliver --listen  (session-bound, has graphical access)
      │  handle_completed → "type_at_cursor"
      ├─ gate: Wayland? not GNOME? wtype present?  ──no──┐
      ├─ decide_type(submit_mode, bytes, TYPE_MAX)       │
      │     ├─ Type ──► wtype -  (stdin) ──ok──► done     │
      │     │                      └─fail─┐               │
      │     └─ NotifyWithAction ─────────┼───────────────┤
      │                                  ▼               ▼
      │                         clipboard inject (best-effort) + notify
      ▼
focused window receives keystrokes
```

## Security & correctness considerations

- **No command injection.** `wtype` is spawned with a constant argv (`["wtype",
  "-"]`) via `std::process`, never a shell. The transcript travels on stdin, so
  it is never interpreted as an argument or option (no `--`/dash hazard).
- **Intent gating.** Foreground-only (F4) bounds *when* keystrokes are emitted
  to the moment the user is actively waiting, shrinking the focus-race window
  (below).
- **Focus race (accepted, documented).** Between job submission and completion
  the focused window may change; `wtype` types into *whatever* is focused at
  completion. Mitigations: foreground-only, plus typical transcription latency
  is short for the interactive dictation flow this targets. We do **not**
  attempt focus tracking (the consumer has no input-event access by design, G4).
  This residual risk is the reason for the size ceiling and foreground gate, and
  is called out in user docs.
- **Size ceiling.** `TYPE_MAX_BYTES` caps how long the virtual keyboard can be
  held and how many keystrokes can be sprayed; over-ceiling degrades to
  clipboard.
- **Layout independence (G3).** `wtype` uploads its own keymap, so output does
  not depend on the user's active XKB layout. We must not pre-transform the
  text per-layout; pass it through verbatim.
- **No new privilege.** Unlike `ydotool` (uinput, `input` group), `wtype` uses
  an unprivileged Wayland protocol — no setuid, no group membership, no daemon.

## Configuration & UX

- **Profile:** add `[[output]] type = "type_at_cursor"`. Compose with others,
  e.g. `file` (durable copy) + `type_at_cursor` (live insertion).
- **One-shot CLI (`zwhisper output last --to type`, OD2 resolved: ship it):**
  mirrors `--to clipboard`, replays the last transcript into the cursor
  manually. Reuses the F5 `TypeSink` and the F6 gate/fallback. Note: a one-shot
  CLI invocation is inherently "foreground" intent, so no `decide_type` mode
  check is needed — the size ceiling still applies.
- **`zwhisper-dictate` contrib script:** unchanged by default (clipboard).
  A follow-up can offer a `--type` mode once the output exists.

## Packaging

- Add to `packaging/arch/PKGBUILD` `optdepends`:
  `'wtype: type transcripts at the cursor for the type_at_cursor output (wlroots compositors only — Sway/Hyprland; not GNOME/KWin)'`.
- Not a hard `depends` — same philosophy as `whisper.cpp`/`xdg-desktop-portal`:
  detected at runtime, absent → clean fallback (F6).
- `contrib/install.sh` optional-tools check can mention `wtype`.

## Testing strategy

- **Pure unit (`decide_type`)** — full boundary/mode matrix, mirroring the
  13-case `decide_clipboard` suite.
- **Pure unit (log hint)** — `desktop_hint(env)` over injected env
  (`ubuntu:GNOME`, `KDE`, `sway`, empty) returns the right human reason string;
  asserts it is advisory only (never returned as a "skip" decision).
- **`TypeSink` with injected `CommandRunner`** — asserts argv is exactly
  `["wtype", "-"]`, the stdin payload equals the transcript (UTF-8, newlines
  preserved), and that non-zero/timeout/spawn-error map to `Err` and trigger the
  F6 fallback (verified by a fake clipboard + notify spy).
- **`encode_outputs`** — extended to cover the new variant.
- **Manual integration (gated, documented).** A headless wlroots compositor in
  CI is out of reach today, exactly like the M4 "5-second paste survives" check
  (`docs/M4-verification.md`). A `docs/RFC-type-at-cursor-verification.md` (or a
  new milestone verification doc) records the manual Sway/KWin procedure:
  foreground job on Sway → text appears at cursor; GNOME *and* KWin → text lands
  in clipboard with the fallback notification; oversized transcript → notify, no
  typing.

## Resolved decisions

- **OD1 — `TYPE_MAX_BYTES = 8_192`.** 8 KB design ceiling (≈ 4–5k chars ≈ ~6–8
  min of speech) covers any realistic dictation; larger transcripts degrade to
  clipboard. Confirm "does not feel arbitrary" in the manual Sway pass; adjust
  the const if needed (single-line change).
- **OD2 — Ship `zwhisper output last --to type` in the same change.** Low cost
  given `TypeSink`; symmetry with `--to clipboard` (see Config & UX).
- **OD3 — Inter-keystroke delay `WTYPE_KEYSTROKE_DELAY_MS = 0`.** Named const at
  `wtype`'s default; raise only if the manual pass shows dropped characters.
- **OD4 — Fallback always injects to clipboard,** including on a
  `type_at_cursor`-only profile. Typing only runs for foreground jobs (user
  actively waiting), the inject is announced via notification, and the
  transcript is on disk regardless — convenience wins over an unsolicited but
  announced clipboard write. (See F6.)

## Out of scope (this RFC)

- GNOME/Mutter typing (N1), `libei`/`reis` backend (N2), `ydotool` (N3), X11
  (N4), true PTT (N5), persistent typed-delivery outbox (N6).
