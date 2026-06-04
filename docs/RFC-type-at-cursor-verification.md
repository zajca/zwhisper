# RFC type-at-cursor — manual verification

> Companion to [`docs/RFC-type-at-cursor.md`](./RFC-type-at-cursor.md). The
> automated suite covers the pure decision logic (`decide_type`), the
> `desktop_hint` advisory, the constant `wtype` argv, the injected-runner
> success/failure paths, and the `encode_outputs` tag. None of that exercises a
> live compositor, so the end-to-end "keystrokes land in the focused window"
> behaviour is verified by hand here.
>
> Date: ____. Verifier: ____.

## Why CI cannot cover this

The `type_at_cursor` output drives the real `wtype` binary against the Wayland
`virtual-keyboard-v1` protocol. That requires a running wlroots compositor
(Sway/Hyprland) with a focused window to receive keystrokes — and, for the
fallback cases, GNOME/Mutter and KDE/KWin sessions that deliberately lack
that protocol. A headless wlroots compositor is out of reach for the CI host,
exactly like the M4 "5-second paste survives" check
(`docs/M4-verification.md` § 9). The unit tests pin everything that is pure;
this document records the manual procedure for everything that is not.

The decision/gate logic these steps exercise:

- F4 `decide_type` — size ceiling checked first, then foreground-only.
- F5 `TypeSink` — spawns `wtype -`, transcript on stdin, constant argv.
- F6 capability gate + fallback — `wtype` presence gate, then attempt, then on
  any failure (missing binary, unsupported compositor, non-zero exit, timeout)
  best-effort clipboard inject + a notification (OD4: inject runs even on a
  `type_at_cursor`-only profile).
- `TYPE_MAX_BYTES = 8_192` ceiling (OD1).

## Preconditions

1. `cargo build --release --workspace` builds clean.
2. `zwhisperd` and `zwhisper` installed (or symlinked from `target/release/`),
   and the session-bound `zwhisper deliver --listen` consumer running on the
   graphical session under test (it is the only component with graphical
   access; typing happens there).
3. A profile that lists the new output, e.g.:

   ```toml
   [[output]]
   type = "type_at_cursor"
   ```

   For some steps a compose profile is handy (`file` for a durable copy +
   `type_at_cursor` for live insertion).
4. `wtype` installed for the typing steps; temporarily uninstalled / removed
   from `$PATH` for the "wtype absent" step.
5. A focused, editable target window (text editor / terminal prompt) before
   completing each foreground job.

---

## 1. Sway/wlroots foreground job — transcript typed at the cursor

**Compositor:** Sway (or Hyprland), `wtype` installed.

**Steps**

1. Open a text editor; click into it so the cursor is focused there.
2. Run a **foreground** recording with the `type_at_cursor` profile, speak a
   short phrase, complete the job.
3. Watch the editor.

**Expected**

- The transcript is **typed** into the editor at the cursor position.
- **No manual paste** (no Ctrl+V) is needed.
- UTF-8 and newlines come through verbatim, independent of the active XKB
  layout (G3 — `wtype` uploads its own keymap).
- No fallback notification fires; the consumer logs a typing success
  (`tracing::info`).

---

## 2. GNOME/Mutter — clipboard + fallback notification, no typing

**Compositor:** GNOME (Wayland / Mutter), `wtype` installed.

**Steps**

1. Same profile, same foreground recording flow, into a focused editor.

**Expected**

- **Nothing is typed** — Mutter does not implement `virtual-keyboard-v1` for
  these clients, so `wtype` exits non-zero quickly (it does not hang).
- The transcript lands in the **clipboard** (best-effort inject, OD4 — happens
  even though the profile only lists `type_at_cursor`).
- A **fallback notification** fires: "Typed delivery unavailable — transcript
  copied to clipboard (Ctrl+V), or run `zwhisper output last --to clipboard`."
- The notification/log is **enriched** with the desktop hint ("looks like
  GNOME — `wtype` needs a wlroots compositor (Sway/Hyprland)") derived from
  `XDG_CURRENT_DESKTOP` / `XDG_SESSION_DESKTOP`. The hint is advisory only — it
  enriches the message but never suppresses the attempt.
- Paste (Ctrl+V) into the editor reproduces the transcript.

---

## 3. KDE/KWin — clipboard + fallback notification, no typing

**Compositor:** KDE Plasma 6 (Wayland / KWin), `wtype` installed.

**Steps**

1. Same profile, same foreground recording flow, into a focused editor.

**Expected**

- Same as step 2: **no typing** (KWin returns "Compositor does not support the
  virtual keyboard protocol"), transcript in the **clipboard**, **fallback
  notification** fired.
- Desktop hint reads "looks like KDE/KWin — `wtype` needs a wlroots compositor
  (Sway/Hyprland)".
- Ctrl+V reproduces the transcript.

---

## 4. Oversized transcript on a foreground job — notify, no typing

**Compositor:** Sway/wlroots, `wtype` installed.

**Steps**

1. Produce a transcript **larger than `TYPE_MAX_BYTES` (8 KB)** for a
   foreground job (a long dictation, or replay an over-ceiling transcript).
2. Complete the foreground job into a focused editor.

**Expected**

- **No typing** — `decide_type` checks the size ceiling **first**, so a job that
  is foreground but over the ceiling yields `NotifyWithAction`.
- The consumer **notifies with an action** offering manual recovery
  (`zwhisper output last --to type` / `--to clipboard`).
- Per F4/F6, the over-ceiling `NotifyWithAction` path notifies only — it does
  **not** start typing the 8 KB+ blob character-by-character into the focused
  window.

---

## 5. Detached / auto job — never types

**Compositor:** Sway/wlroots, `wtype` installed.

**Steps**

1. Run a **detached** (or `auto`) job with the `type_at_cursor` profile.
2. Let it complete while a focused editor is in front.

**Expected**

- **Nothing is typed**, regardless of transcript size. `decide_type` returns
  `NotifyWithAction` for any non-`foreground` submit mode (detached / auto /
  unknown). This bounds keystroke emission to the moment the user is actively
  waiting and shrinks the focus-race window.
- A **notify-with-action** appears offering manual replay.

---

## 6. One-shot CLI `zwhisper output last --to type`

**Compositor:** Sway/wlroots, `wtype` installed.

**Steps**

1. After a completed recording, focus an editor.
2. Run `zwhisper output last --to type`.

**Expected**

- The **last transcript is replayed at the cursor** via `wtype`.
- A one-shot CLI invocation is inherently foreground intent, so there is **no
  `decide_type` mode check** — but the **size ceiling still applies**.
- **Over-ceiling variant:** with a last transcript larger than `TYPE_MAX_BYTES`,
  the command is **rejected** (non-zero exit) with a message suggesting
  `--to clipboard` instead. Nothing is typed.

---

## 7. `wtype` not installed — clipboard + notify fallback

**Compositor:** Sway/wlroots, `wtype` **removed** from `$PATH`.

**Steps**

1. Confirm `command -v wtype` returns nothing.
2. **Foreground** job with the `type_at_cursor` profile into a focused editor.

**Expected**

- The **presence gate** trips before any spawn (cached PATH lookup): no `wtype`
  process is started.
- The transcript is injected into the **clipboard** (best-effort) and a
  **fallback notification** fires (same message as step 2/3).
- The one-shot `zwhisper output last --to type` likewise reports a clear error
  suggesting `--to clipboard`, and does not type.
- The transcript remains on disk regardless (best-effort delivery, N6).

---

## Accepted caveat — focus race

Typed output goes to **whatever window is focused at completion**. Between job
submission and completion the focused window may change; `wtype` types into the
window focused at the moment of delivery, not the one focused at submission.
This is accepted and documented (RFC § Security & correctness). Mitigations
baked into the design: foreground-only gating (step 5), the size ceiling
(step 4), and the typically short interactive-dictation latency. The consumer
does **not** track focus (it has no input-event access by design, G4).

When verifying, do not switch focus during a foreground job unless you are
specifically exercising this caveat.

---

## Checklist

- [ ] 1. Sway/wlroots foreground job types the transcript at the cursor, no manual paste, layout-independent.
- [ ] 2. GNOME/Mutter: no typing; transcript in clipboard + fallback notification; GNOME desktop hint present.
- [ ] 3. KDE/KWin: no typing; transcript in clipboard + fallback notification; KWin desktop hint present.
- [ ] 4. Oversized (> 8 KB / `TYPE_MAX_BYTES`) foreground job: notify-with-action, no typing.
- [ ] 5. Detached / auto job: never types; notify-with-action.
- [ ] 6. `zwhisper output last --to type` on Sway replays at the cursor; over-ceiling is rejected with a clipboard suggestion.
- [ ] 7. `wtype` absent: presence gate trips → clipboard + fallback notification; CLI errors toward `--to clipboard`.
- [ ] Focus-race caveat understood and accepted; transcript always present on disk.

## Verdict

____ (set only when every box above is ticked on the listed compositors).
