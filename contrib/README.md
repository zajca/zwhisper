# contrib — desktop integration

Optional, ready-to-use helpers for driving zwhisper from a Wayland
desktop (tested on Sway + Waybar, works on any wlroots compositor).

Nothing here is required to use zwhisper — the CLI and daemon work on
their own. These are the glue most people end up writing by hand.

| Path | What it is |
| --- | --- |
| `bin/zwhisper-dictate` | Push-to-dictate toggle: record the mic, transcribe, copy the text to the clipboard. Mic-only (no system audio), daemon-free. |
| `bin/zwhisper-cycle-profile` | Switch the active profile to the next one and notify. |
| `sway/zwhisper.conf` | Sway/i3 key bindings (`include` it from your config). |
| `waybar/zwhisper.jsonc` | Waybar custom module (status + click actions). |
| `waybar/style.css` | Optional CSS to colour the module red while recording. |
| `profiles/*.toml` | Example user profiles (Parakeet + whisper.cpp). |
| `install.sh` | Installs the scripts and profiles, checks deps, prints the wiring steps. |

## Quick install

```sh
contrib/install.sh
```

Then follow the printed steps to add the `include` line to your Sway
config and the module to your Waybar config. See the
[Desktop integration](../README.md#desktop-integration-sway--waybar)
section of the main README for the full walk-through.

## Default key bindings

| Shortcut | Action |
| --- | --- |
| `Super+Ctrl+R` | Dictate: record → stop → transcript in clipboard |
| `Super+Ctrl+P` | Cycle the active profile |
| `Super+Ctrl+S` | Show daemon status in a notification |
| `Super+Ctrl+T` | Toggle a daemon meeting recording (mic + system audio) |

Change the chord by editing `$zwhisper_mod` in `sway/zwhisper.conf`.

## Dictation vs. meeting recording

- **Dictation** (`zwhisper-dictate`) records **only your microphone** and
  is the right tool for voice typing — whatever is playing on your
  speakers is never captured. It writes the result to the clipboard.
- **Meeting recording** (`zwhisper toggle`) uses the active profile,
  which captures a **mono mix of mic + system output** so both sides of
  a call are recorded, and writes a FLAC + transcript to your
  `~/Recordings`.

## Configuration

`zwhisper-dictate` reads a few optional environment variables — set them
in your compositor environment to override the defaults:

| Variable | Default | Meaning |
| --- | --- | --- |
| `ZWHISPER_DICTATE_BACKEND` | `parakeet` | transcription backend |
| `ZWHISPER_DICTATE_MODEL` | `parakeet-tdt-0.6b-v3` | model id |
| `ZWHISPER_DICTATE_LANGUAGE` | `auto` | language or `auto` |
| `ZWHISPER_DICTATE_SOURCE` | system default | PipeWire source name |

If you have not built zwhisper with the `parakeet` feature, point these
at whisper.cpp instead, e.g. `ZWHISPER_DICTATE_BACKEND=whisper-cpp
ZWHISPER_DICTATE_MODEL=large-v3-turbo-q5_0`.
