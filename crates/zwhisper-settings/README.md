# zwhisper-settings

On-demand FLTK settings GUI for zwhisper. Spawned by the tray's
"Settings…" menu entry; closes back to the system tray. Hosts four
tabs: **Profiles**, **Models**, **Whisper-CLI**, **Hotkey**. The
binary is ephemeral — closing the window exits.

## Build dependencies

This crate links FLTK statically through the `fltk-bundled` feature.
The bundled build pulls and compiles FLTK at build time and requires:

- `cmake >= 3.11`
- `gcc >= 11` (or another C++17-capable compiler)
- `curl`
- `tar`

On Arch Linux: `pacman -S --needed cmake gcc curl tar`.

The first compile takes several minutes (FLTK ~30 MB of C++).
Subsequent builds reuse the cached static library.

## Running

```
cargo run --release -p zwhisper-settings
```

Invariants enforced at boot:

- Only one instance per session — second launch detects the
  `cz.zajca.Zwhisper1.Settings` D-Bus name and exits 0.
- A non-integer FLTK scale (e.g. KDE 1.5×) shows a yellow banner
  pointing at `FLTK_SCALING_FACTOR=1` as the documented escape
  hatch.

## Configurable model URL

The Models tab reads `~/.config/zwhisper/models.toml` for the
download base URL. Default points at HuggingFace; the `{model}`
placeholder is substituted at request time:

```toml
# ~/.config/zwhisper/models.toml
base_url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}.bin"
```

If the file is absent the built-in default (HuggingFace) is used.
Malformed TOML surfaces a typed error — silent fallback to a wrong
URL is explicitly rejected (CLAUDE.md "no silent defaults").

## Pointers

- Implementation plan: `docs/M7-plan.md`
- Manual verification matrix: `docs/M7-verification.md`
- Hotkey portal layer (reused from M6): `crates/zwhisper-hotkey/`
- Profile loader (reused from M2): `crates/zwhisper-core/src/profile/`
