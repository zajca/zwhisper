# M6 — Hotkey toggle verification

> Companion to [`docs/M6-plan.md`](./M6-plan.md). Each row of the
> manual matrix in `M6-plan.md` § "Manual verification gate" is
> recorded here with date, distro, package versions, and Pass/Fail
> per row before M6 is signed off as closed.
>
> Date: _pending — written retroactively after M6 ship_.
> Verifier: _pending_.

## Status

| Stage                                                                  | State |
| ---------------------------------------------------------------------- | ----- |
| Plan written (`docs/M6-plan.md`)                                       | done  |
| `crates/zwhisper-hotkey` skeleton (Cargo.toml + module split)          | done  |
| Toggle decision + Debouncer + cooldown (`toggle.rs`)                   | done  |
| `PortalAdapter` trait + `AshpdAdapter` + `FakePortal`                  | done  |
| `zwhisper hotkey probe` + `zwhisper hotkey status` + `zwhisper hotkey bind` | done  |
| Tray integration (`crates/zwhisper-tray/src/hotkey.rs`)                | done  |
| Round-2 multi-model code review fixes (Claude + Copilot GPT-5.4)       | done  |
| Workspace test suite green at M6 ship                                  | done  |
| Manual verification gate (M6-plan § "Manual verification gate")        | _pending — run on real desktop sessions_ |

The implementation landed across commits `16e2d60 feat(m6): hotkey
toggle via xdg-desktop-portal GlobalShortcuts` and the round-2
review-fix commit on `main`. Subsequent milestones (M7's
`hotkey_signal.rs`, M8's protocol handshake) build on the M6
surface unchanged.

## Automated test inventory (M6)

| File                                                       | Tests | DoD     |
| ---------------------------------------------------------- | ----- | ------- |
| `crates/zwhisper-hotkey/src/toggle.rs` (`#[cfg(test)]`)    | 10    | #1–#6   |
| `crates/zwhisper-hotkey/src/portal.rs` (`#[cfg(test)]`)    | 8     | #7–#9   |
| `crates/zwhisper-hotkey/src/probe.rs` (`#[cfg(test)]`)     | 6     | #10     |
| `crates/zwhisper-hotkey/src/active_session.rs` (`#[cfg(test)]`) | 7     | #2 (active-session.json read helper) |
| `crates/zwhisper-hotkey/src/config.rs` (`#[cfg(test)]`)    | 7     | #4, #5 (cooldown + debounce defaults) |
| `crates/zwhisper-cli/src/commands/hotkey.rs` (CLI)         | covered via `tests/cli.rs` | #11, #12 |
| `crates/zwhisper-tray/src/hotkey.rs` (tray listener)       | unit tests in module       | #15–#16 |

`grep -rn '^#\[test\]\|#\[tokio::test\]' crates/zwhisper-hotkey/src/`
reports **32** test functions in the dedicated crate alone.

## Open questions resolved during ship

The "Open questions for ship" block in `docs/M6-plan.md` produced
these decisions, recorded here so the answers don't get lost in
the plan:

- **D1 (post-stop cooldown).** Default `cooldown_ms = 1500` ms with
  override via `~/.config/zwhisper/hotkey.toml`. Matches plan §
  "Architectural decisions / D1".
- **D3 (portal session lifecycle).** Lazy create with
  `auto_bind_on_startup = true` soft attempt. Failure does not
  block tray launch.
- **D4 (app-id).** `cz.zajca.Zwhisper1.Tray` is the portal app-id
  for both tray and CLI bind paths; both `Activated` signal
  subscriptions filter by this app-id plus the
  `org.freedesktop.portal.Desktop` sender.
- **D5 (chord identity).** Portal-owned. zwhisper persists no
  chord state — `ListShortcuts` is the source of truth on every
  startup.
- **D6 (wire surface).** `Recorder1` D-Bus surface is unchanged.
  M6 added only an internal listener, no new IPC.

## Risks and mitigations (post-ship review)

| ID | Status |
|----|--------|
| R1 (portal absent on Sway/wlroots) | Mitigated: `probe` returns `UNAVAILABLE` with a typed reason; tray Hotkey tab shows graceful degradation. |
| R2 (portal backend crash recovery) | Mitigated: B1 reconnect-on-`ServiceUnknown` path with 500 ms debounce + post-reconnect `list_shortcuts` verification. Test: `portal::tests::reconnect_on_service_unknown`. |
| R3 (forged `Activated` from non-portal sender) | Mitigated: subscription filters by sender + app-id. Test: `portal::tests::activated_from_non_portal_sender_dropped`. |
| R4 (toggle during transcription drain) | Mitigated: A1 cooldown window (1500 ms default) + `active-session.json` fallback. Test: `toggle::tests::draining_window_emits_noop_not_start`. |
| R5 (portal restart while bound) | Manually verified during M6 round-2 testing on i3/X11 fallback path; portal-backed restart deferred to MV gate below. |

## Manual verification gate (MV-1..MV-13)

Mirrors `docs/M6-plan.md` § "Manual verification gate". Ship is
gated on every KDE Plasma 6 row reading PASS; GNOME, Sway, and
i3 rows record outcome only (best-effort, not pass/fail-blocking
per the plan).

| #     | Desktop      | Test                                                                                    | Expected output                                                                                                  | Result  |
| ----- | ------------ | --------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | ------- |
| MV-1  | KDE Plasma 6 | Tray "Bind hotkey…" → pick `Ctrl+Alt+R`                                                 | Tray label `Hotkey: Ctrl+Alt+R`; KDE System Settings → Shortcuts lists `zwhisper` as owner.                      | pending |
| MV-2  | KDE Plasma 6 | Press `Ctrl+Alt+R` from another app's focus                                             | Tray flips to recording within 250 ms; "Recording started (<active_profile>)" notification; FLAC file appears.   | pending |
| MV-3  | KDE Plasma 6 | Press `Ctrl+Alt+R` again                                                                | `StateChanged "stopping"` propagates; tray flips to stopping; transcription completes; tray returns to idle.     | pending |
| MV-4  | KDE Plasma 6 | Press `Ctrl+Alt+R` during transcription drain                                           | NoOp — no second recording; tray label briefly shows "draining" or stays in stopping.                            | pending |
| MV-5  | KDE Plasma 6 | `systemctl --user restart xdg-desktop-portal-kde` while bound                           | Tray re-establishes session; `zwhisper hotkey status` still `BOUND`.                                             | pending |
| MV-6  | KDE Plasma 6 | `zwhisper hotkey status` from a fresh terminal                                          | `BOUND (Ctrl+Alt+R, portal=kde, session=...)`, exit 0.                                                           | pending |
| MV-7  | KDE Plasma 6 | `systemctl --user stop zwhisperd; zwhisper toggle`                                      | `notify-send` notification (critical urgency); stderr `toggle: FAIL (daemon not running)`; exit 2.               | pending |
| MV-8  | GNOME 47+    | Launch tray, attempt bind                                                               | Document UX; KDE-comparable expected; record any deviation.                                                      | pending |
| MV-9  | GNOME 47+    | Press chord, observe toggle                                                             | Same as MV-2; document any UX deviation.                                                                         | pending |
| MV-10 | Sway/wlroots | `zwhisper hotkey probe`                                                                 | If `xdg-desktop-portal-wlr` installed: `available`; else `UNAVAILABLE`.                                          | pending |
| MV-11 | i3 (X11)     | `zwhisper hotkey probe`                                                                 | `hotkey: portal=NONE (no GlobalShortcuts portal — bind via your WM)`, exit 2.                                    | pending |
| MV-12 | i3 (X11)     | `bindsym Mod4+Shift+r exec --no-startup-id /usr/bin/zwhisper toggle`, press chord       | Recording starts via D-Bus; second press stops.                                                                  | pending |
| MV-13 | i3 (X11)     | Same `bindsym` with daemon stopped                                                      | `notify-send` notification (DoD #14 — E2).                                                                       | pending |

> Note on retroactive verification: the M6 review-fix commit and
> the M7 milestone, which both consume the M6 hotkey surface
> without modification, provide implicit operational coverage of
> the toggle decision logic and portal adapter. The MV gate above
> is still required for the explicit pass/fail record before
> stamping `READY` on this document.

## Sign-off

| Date    | Scenarios passed | Sign-off |
| ------- | ---------------- | -------- |
| pending | _pending_        | _pending_ |

> **Verdict line below is set only when every KDE row reads PASS
> and the GNOME/Sway/i3 outcomes are recorded above.**

## Verdict

_Pending manual verification gate run._
