# M0 — Host setup (Arch Linux)

> Packages required to build and run the zwhisper M0 walking skeleton on
> Arch / Arch-based distros. Other distros provide equivalent packages
> under their own names — search for `pipewire`, `gstreamer`,
> `gst-plugin-pipewire`, `flac`, `pkgconf`, `clang`.

## Audio stack (PipeWire + WirePlumber)

```
sudo pacman -S --needed \
    pipewire pipewire-alsa wireplumber
```

`wireplumber` provides `wpctl`, which M0 uses to resolve the default
audio source/sink (see `docs/M0-plan.md`, Phase 2). `pipewire-alsa` is
not strictly required by M0 but is what most desktop apps still talk to,
so installing it keeps the box useful.

## GStreamer + plugins

```
sudo pacman -S --needed \
    gstreamer \
    gst-plugins-base \
    gst-plugins-good \
    gst-plugin-pipewire
```

- `gst-plugins-base` ships `audioconvert`, `audioresample`, `audiomixer`.
- `gst-plugins-good` ships `flacenc` and `filesink`.
- `gst-plugin-pipewire` ships `pipewiresrc`, the bridge to PipeWire.

## FLAC tooling (verification only)

```
sudo pacman -S --needed flac
```

Used by Phase 6 (`flac -t output.flac`,
`metaflac --show-total-samples output.flac`) to confirm the encoded
file is valid and matches the recorded duration.

## Build chain

```
sudo pacman -S --needed pkgconf clang
```

`pkgconf` is required for the `gstreamer-rs` build script to locate the
GStreamer C libraries. `clang` is required by `bindgen`, which
`gstreamer-sys` runs at build time.

The Rust toolchain itself is pinned by `rust-toolchain.toml` at the
repo root; you do not need to install rustc system-wide.

## Smoke test

After installing, confirm the GStreamer ↔ PipeWire bridge actually
works:

```
gst-launch-1.0 pipewiresrc num-buffers=10 ! audioconvert ! fakesink
```

Expected: the command finishes with `Got EOS from element "pipeline0"`
and exits 0 within a second. The `audioconvert` element is required —
without a downstream converter, `pipewiresrc` cannot negotiate caps and
will fail with `target not found` even when a default source exists.
This mirrors the M0 pipeline shape (`pipewiresrc ! audioconvert ! …`),
so it is the test that actually matters.

If `pipewiresrc` is reported missing, double-check `gst-plugin-pipewire`
is installed and that `gst-inspect-1.0 pipewiresrc` succeeds. If the
smoke test fails with `target not found` despite `audioconvert` being
present, confirm `pipewire` and `wireplumber` are running
(`pgrep pipewire wireplumber`) and that `wpctl status` shows at least
one Source.

---

# M1 — Whisper.cpp host setup

## whisper.cpp binary

zwhisper does **not** vendor `whisper.cpp`. M1 detects an
already-installed `whisper-cli` (or `whisper-cpp` on other distros) on
the host's `$PATH`. The PKGBUILD declares `optdepends`, not `depends`.

Arch / Arch-based:

```
# pick one — CUDA build is much faster on NVIDIA, otherwise CPU build
yay -S whisper.cpp           # CPU build (AUR)
yay -S whisper.cpp-cuda      # CUDA build (AUR)
```

Both packages install the binary as `whisper-cli` at `/usr/bin/whisper-cli`.
On other distros the binary is sometimes named `whisper-cpp` — the
detector handles both names (see IDEA.md § 4 and
`crates/zwhisper-cli/src/transcribe/discovery.rs`).

Build-from-source instructions live upstream:
<https://github.com/ggerganov/whisper.cpp>.

## Detection order (5-step)

The runner consults these locations in order:

1. `ZWHISPER_WHISPER_CLI` env var (explicit absolute path)
2. `whisper-cli` on `$PATH`
3. `whisper-cpp` on `$PATH` (other-distro alias)
4. `~/.local/bin/whisper-cli`
5. (Settings UI install hint — M7, not runtime)

If none resolve, `zwhisper transcribe` returns
`TranscribeError::BackendUnavailable { searched }` listing every path
attempted. Install whisper.cpp via the package manager or set
`ZWHISPER_WHISPER_CLI` to the binary path.

## Models

zwhisper does **not** download or bundle models. Models are resolved by
**name only** (IDEA.md § 4):

```
~/.local/share/zwhisper/models/ggml-{name}.bin
```

Examples:

```
~/.local/share/zwhisper/models/ggml-tiny.bin
~/.local/share/zwhisper/models/ggml-small.bin
~/.local/share/zwhisper/models/ggml-large-v3.bin
```

Download from the upstream Hugging Face mirror
<https://huggingface.co/ggerganov/whisper.cpp> — the SHA256 manifest
URL pattern is referenced by M7 settings UI, but the path layout is
locked here. RAM footprint is roughly the model file size resident,
so `large-v3` ≈ 3 GiB.

A missing model surfaces as
`TranscribeError::ModelNotFound { name, expected }` where `expected` is
the canonical path printed verbatim — the user can copy-paste it into a
download command.

## Verified flag-name compatibility (Phase 0 reality check)

Captured from `whisper-cli --help` on the maintainer's host
(`/usr/bin/whisper-cli`, AUR `whisper.cpp` package). Frozen snapshot at
`docs/M1-verification/whisper-cli-help.txt`.

The plan's subprocess contract assumes:

| Plan flag           | Upstream long form    | Upstream short | Status |
|---------------------|-----------------------|----------------|--------|
| `--model <path>`    | `--model FNAME`       | `-m`           | match  |
| `--language <iso>`  | `--language LANG`     | `-l`           | match  |
| `--output-txt`      | `--output-txt`        | `-otxt`        | match  |
| `--output-json`     | `--output-json`       | `-oj`          | match  |
| `--output-file <s>` | `--output-file FNAME` | `-of`          | match  |

All five flags exist with the assumed semantics. No flag-rename
mitigation needed in Phase 3. Input audio is a positional argument
(`whisper-cli [options] file0 file1 ...`); the subprocess invocation
passes `<audio>` last, after `--output-file <stem>`.

## FLAC fallback (only if libsndfile is too old)

The `flac` CLI is already required by M0 verification. If a host's
`whisper-cli` build was linked against a libsndfile too old to read
FLAC, Phase 3 falls back to `flac --decode` into a per-call
`TempDir`. We **do not** introduce a GStreamer-based decoder for this
path — the M0 GStreamer pipeline stays a recorder-only concern.
