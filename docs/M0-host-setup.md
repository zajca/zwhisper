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
