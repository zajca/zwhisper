# M7 — Settings GUI verification

> Companion to [`docs/M7-plan.md`](./M7-plan.md). Each row in the
> manual matrix is recorded with screenshots and timestamps before
> M7 is marked shipped. Open questions R1–R5 from the plan are
> answered here once measurement data is available.
>
> Date: _pending_. Verifier: _pending_.

## Status

_Pending — implementation in progress (Group A skeleton merged)._

## Test totals

```
$ cargo test --workspace
  (recorded after Groups A–E land)

$ cargo clippy --workspace --all-targets -- -D warnings
  (recorded after Groups A–E land)

$ cargo fmt --check
  (recorded after Groups A–E land)
```

## Manual verification matrix

Mirrors `docs/M7-plan.md` § "Manual verification gate". Ship is
gated on every row reading PASS.

| #     | Scenario                                       | Acceptance                                                                                                                       | Result  |
| ----- | ---------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- | ------- |
| MV-1  | KDE Plasma 6 Wayland @ 1.0× scaling            | All four tabs render without clipping; profile save round-trips; model download (tiny) completes; hotkey rebind dialog opens.    | pending |
| MV-2  | KDE Plasma 6 Wayland @ 1.5× scaling (A1 gate)  | Same as MV-1. Failure → freeze ship; open M7.1 for Slint.                                                                        | pending |
| MV-3  | GNOME 47+ Wayland @ 1.0×                       | Same as MV-1. Hotkey rebind via GNOME's portal-gnome backend.                                                                    | pending |
| MV-4  | sway/wlroots @ 1.0×                            | All tabs except Hotkey render; Hotkey tab shows "Portal unavailable" graceful banner.                                            | pending |
| MV-5  | KDE Plasma 6 X11 @ 1.0×                        | Same as MV-1; FLTK auto-detects X11 backend (no `WAYLAND_DISPLAY`).                                                              | pending |
| MV-6  | RAM footprint                                  | `/usr/bin/time -v target/release/zwhisper-settings` idle RSS < 60 MB; peak during `large-v3` download < 80 MB.                   | pending |
| MV-7  | Single-instance                                | Two consecutive `zwhisper-settings &` invocations: second exits 0, first window raises.                                          | pending |
| MV-8  | Save-during-recording (DoD #3)                 | Start recording from CLI; open settings; save profile; observe modal warning + skipped reload.                                   | pending |
| MV-9  | Captive portal simulation (DoD #9)             | Run a local HTTP server returning HTML 200; point `models.toml` at it; click Download → "Endpoint returned non-binary response". | pending |
| MV-10 | Cross-FS rename (DoD #7)                       | Mount `<models_dir>` as separate filesystem; download tiny; assert no `EXDEV` and `.bin` final lands correctly.                  | pending |

## Open questions (M7-plan § "Open questions for ship")

- **R1 — measured RAM footprint on idle and during download.**
  Run `/usr/bin/time -v target/release/zwhisper-settings`; record
  idle RSS and peak RSS during a `large-v3` download. If > 80 MB,
  flag for M8 perf review.
  - Idle RSS: _pending_
  - Peak RSS during `large-v3`: _pending_
- **R2 — KDE Plasma 6 HiDPI fractional-scale matrix.** Covered by
  MV-2; record screenshots at 1.0×, 1.25×, 1.5×, 1.75×, 2.0×.
  - Result: _pending_
- **R3 — Wayland vs X11 backend auto-detection.** Confirm
  `WAYLAND_DISPLAY` flips backend without manual `FLTK_BACKEND`
  env. On wlroots without portal: confirm graceful degradation
  (Hotkey tab shows "Portal unavailable").
  - Result: _pending_
- **R4 — `Profiles1.reload` round-trip latency under load.** Just
  measure; no contract change.
  - Result: _pending_
- **R5 — `.part.meta.json` durability vs throughput.** Measure
  download throughput with and without per-chunk `fsync_data()`.
  If throughput drops > 2×, switch to per-chunk-count fsync (every
  16 chunks) and lengthen the on-resume re-hash window.
  - Result: _pending_

## Sign-off

| Date    | Scenarios passed | Sign-off |
| ------- | ---------------- | -------- |
| pending | _pending_        | _pending_ |
