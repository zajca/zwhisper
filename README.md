# zwhisper

Linux desktop tool for recording PipeWire audio (mic + system output) with
tray control, profiles, and a transcription pipeline backed by local
[`whisper.cpp`](https://github.com/ggerganov/whisper.cpp) (and optionally
cloud backends).

> **Status: pre-M0, scaffolding only.** See [`IDEA.md`](./IDEA.md) for the
> full architecture spec, scope, and roadmap.

## Targets

- **Primary**: Arch Linux + KDE Plasma 6, PipeWire, Wayland-first
- **Secondary**: GNOME 47+, wlroots compositors (Sway, Hyprland) — best effort
- **Out of scope**: Non-Linux platforms

## Project layout

Cargo workspace. M0 ships only `crates/zwhisper-cli` as a single-process
binary; daemon/tray/settings split lands in M3+.

```
zwhisper/
├── Cargo.toml                 # workspace root
├── IDEA.md                    # architecture spec
├── crates/
│   └── zwhisper-cli/          # bin: CLI (single-process in M0–M2)
└── .github/workflows/         # CI + security audits
```

See [`IDEA.md` § 14](./IDEA.md) for the full target layout.

## Development

### Prerequisites

- Rust stable (pinned in `rust-toolchain.toml`)
- For M0+ audio work: GStreamer + PipeWire system libraries
  (`gstreamer`, `gst-plugins-base`, `gst-plugins-good`, `pipewire`)

### Common commands

```bash
cargo fmt --all                        # format
cargo fmt --all -- --check             # CI check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --workspace
cargo build --workspace --release
```

### CI

Every push and pull request runs `fmt`, `clippy`, `test`, and `build` on
Linux stable. A separate scheduled workflow runs `cargo audit` and
`cargo deny` for vulnerability and license checks.

## License

[MIT](./LICENSE)
