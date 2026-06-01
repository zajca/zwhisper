# zwhisper

CLI-first Linux tool for recording PipeWire audio (microphone +
system output) with a user daemon, file-based profiles, and a
transcription pipeline backed by local
[`whisper.cpp`](https://github.com/ggerganov/whisper.cpp) or cloud
backends (Deepgram).

> **Status:** `0.1.0` (M8) — first packageable release. Milestones
> M0–M8 are complete; the M8 manual verification gate
> ([`docs/M8-verification.md`](./docs/M8-verification.md))
> still needs to be exercised on a clean Arch box before tagging
> `v0.1.0`. See [`CHANGELOG.md`](./CHANGELOG.md) and
> [`IDEA.md`](./IDEA.md) for the full architecture and roadmap.

## Features

- **PipeWire-native capture.** Records the microphone *and* the system
  audio monitor into a single FLAC mix, no PulseAudio loopback.
- **Daemon + CLI** — `zwhisperd` owns recording/transcription and
  `zwhisper` is the user-facing control surface over the single D-Bus
  contract (`cz.zajca.Zwhisper1`).
- **Profiles.** Versioned TOML profiles select the backend, model, and
  capture options. User overrides shadow shipped + embedded profiles.
- **Local whisper.cpp** with optional Deepgram cloud backend (Nova-3).
  Secrets are resolved from `ZWHISPER_<BACKEND>_API_KEY` or
  `~/.config/zwhisper/secrets.toml` (mode 0600 enforced).
- **Manual desktop integration** via compositor key bindings, Waybar
  custom modules, or scripts that call `zwhisper status` and
  `zwhisper toggle`.
- **File-based configuration** for profiles, secrets, backend options,
  and model cache selection. GUI settings and tray services are not
  part of the CLI-only product.
- **Protocol-version handshake** — mismatched daemon + client
  binaries refuse to talk and surface a single, actionable error.

## Targets

- **Primary:** Arch Linux + KDE Plasma 6, PipeWire, Wayland.
- **Secondary:** wlroots compositors (Sway, Hyprland) and GNOME 47+
  on Wayland — best effort, hotkey rebind degrades gracefully where
  the GlobalShortcuts portal is absent.
- **Out of scope:** X11 sessions, non-Linux platforms, PulseAudio-only systems.

## Install

### Arch Linux (recommended)

The repository ships a hand-maintained PKGBUILD under
[`packaging/arch/`](./packaging/arch/).

```sh
git clone https://github.com/zajca/zwhisper
cd zwhisper/packaging/arch
makepkg -si
```

What `makepkg -si` does:

1. Pulls `makedepends` (`cargo`, `rust>=1.88`, `gcc`, `pkgconf`)
   for the daemon + CLI build.
2. Builds the CLI-only product packages (`zwhisperd` and
   `zwhisper`) and runs the package-gating unit tests that do not
   need a live desktop session.
3. Installs both binaries to `/usr/bin/`, the daemon systemd-user
   unit to `/usr/lib/systemd/user/`, the D-Bus service file, and
   example config files (see
   [`packaging/README.md`](./packaging/README.md) for the canonical
   layout).

After install, enable the daemon for your user if you want it running
before the first CLI call:

```sh
systemctl --user daemon-reload
systemctl --user enable --now zwhisperd.service
```

The daemon is also activated on demand by D-Bus when any client first
calls `cz.zajca.Zwhisper1`, so enabling the unit is optional for
normal CLI use.

### Other distributions

Not yet packaged. The build-from-source path below works on any
modern Linux with PipeWire >= 1.0 and Rust >= 1.88. Native packages
for Debian / Ubuntu (`.deb`), Fedora (`.rpm`), Flatpak, and a NixOS
module are deferred — see
[`docs/M8-plan.md`](./docs/M8-plan.md) § "Out of scope" for
priorities.

### Pre-built binary releases

Not yet published. The first GitHub Release (`v0.1.0`) will land
after the M8 manual verification gate
([`docs/M8-verification.md`](./docs/M8-verification.md))
passes on a clean Arch box. Until then, build from source or use
`makepkg`.

## Build from source

### Prerequisites

The Rust toolchain is pinned in
[`rust-toolchain.toml`](./rust-toolchain.toml). You also need system
libraries for PipeWire, GStreamer, D-Bus, and desktop notifications.

#### Arch Linux

```sh
sudo pacman -S --needed \
    rust cargo gcc pkgconf \
    gstreamer gst-plugins-base gst-plugins-good gst-plugin-pipewire \
    pipewire wireplumber dbus libnotify
```

Install `xdg-desktop-portal-kde`, `xdg-desktop-portal-gnome`, or
`xdg-desktop-portal-wlr` only if you want to use the optional
portal-backed hotkey commands. Compositor-native key binds that run
`zwhisper toggle` do not need the portal.

#### Fedora / RHEL

```sh
sudo dnf install \
    rust cargo gcc pkgconfig \
    gstreamer1-devel gstreamer1-plugins-base gstreamer1-plugins-good \
    pipewire pipewire-gstreamer wireplumber dbus libnotify dbus-devel
```

#### Debian / Ubuntu

```sh
sudo apt install \
    cargo gcc pkg-config \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good gstreamer1.0-pipewire \
    pipewire wireplumber dbus libnotify-dev libdbus-1-dev
```

Rust >= 1.88 may need to come from `rustup` rather than the distro
repo on older Debian/Ubuntu releases.

### Build

```sh
git clone https://github.com/zajca/zwhisper
cd zwhisper

# Debug build (fast compile, no optimisation)
cargo build -p zwhisperd -p zwhisper-cli

# Release build (use this for daily driving)
cargo build -p zwhisperd -p zwhisper-cli --release
```

Product artefacts land in `target/release/`:

| Binary             | Role                                                    |
| ------------------ | ------------------------------------------------------- |
| `zwhisperd`        | D-Bus daemon — owns recording, transcription, profiles. |
| `zwhisper`         | CLI — `record`, `toggle`, `status`, `profile`, `model`, `hotkey`, `transcribe`, `backend`. |

### Manual install (no `makepkg`)

After `cargo build -p zwhisperd -p zwhisper-cli --release`:

```sh
# Binaries
sudo install -Dm755 target/release/zwhisperd        /usr/local/bin/zwhisperd
sudo install -Dm755 target/release/zwhisper         /usr/local/bin/zwhisper

# systemd-user units (note: ExecStart references /usr/bin — edit if
# you installed under /usr/local/bin)
install -Dm644 systemd/zwhisperd.service       ~/.config/systemd/user/zwhisperd.service

# D-Bus auto-activation
sudo install -Dm644 dbus/cz.zajca.Zwhisper1.service \
    /usr/share/dbus-1/services/cz.zajca.Zwhisper1.service

systemctl --user daemon-reload
systemctl --user enable --now zwhisperd.service
```

For a fully-isolated install, prefer `makepkg -si` on Arch — it
puts every file under `pacman` ownership.

## Configure

### Profiles

Profiles are TOML files describing the backend, model, capture
options, and routing. The lookup order on read is:

1. `~/.config/zwhisper/profiles/<name>.toml` — user override.
2. `/usr/share/zwhisper/profiles/<name>.toml` — shipped (read-only).
3. Built-in embedded profiles compiled into the daemon.

Set the active profile:

```sh
zwhisper profile list                    # show available profiles
zwhisper profile set local-whisper       # set active
zwhisper profile show                    # print resolved active profile
```

Edit profiles with `$EDITOR`. If you change the active profile while
the daemon is running, stop the current recording first and reload or
restart the daemon before relying on the new values.

### Secrets (cloud backends)

Cloud backends (Deepgram) need an API key. Resolution order:

1. `ZWHISPER_<BACKEND>_API_KEY` environment variable.
2. `~/.config/zwhisper/secrets.toml` (mode 0600 enforced; parent
   directory must not be group- or other-writable).

Copy [`docs/secrets.toml.example`](./docs/secrets.toml.example):

```sh
mkdir -p ~/.config/zwhisper
install -m 600 docs/secrets.toml.example ~/.config/zwhisper/secrets.toml
$EDITOR ~/.config/zwhisper/secrets.toml
```

Hot-reload the daemon's secret cache without restarting:

```sh
busctl --user call cz.zajca.Zwhisper1 \
    /cz/zajca/Zwhisper1/Daemon1 \
    cz.zajca.Zwhisper1.Daemon1 \
    ReloadSecrets
```

### whisper.cpp models

Models live under `~/.local/share/zwhisper/models/` by default. To
share a single model cache with other whisper.cpp tools, point
`ZWHISPER_MODELS_DIR` at that directory; it must be an absolute path
and contain files named `ggml-{model}.bin`.

```sh
export ZWHISPER_MODELS_DIR="$HOME/.local/share/whisper.cpp/models"
```

The CLI-only target is `zwhisper model ...` for model discovery,
download, verification, and cache-path inspection:

```sh
zwhisper model list
zwhisper model download large-v3-turbo-q5_0
zwhisper model verify large-v3-turbo-q5_0
zwhisper model path
```

During the CLI transition, run `zwhisper model --help` in your
checkout for the exact subcommands that have landed. Until the model
download command is available, place `ggml-{model}.bin` files in the
resolved models directory yourself.

To use a custom mirror or a local cache, create
`~/.config/zwhisper/models.toml` and edit it as a regular TOML config
file. The intended mirror URL format keeps `{model}` as the only
placeholder and requires HTTPS.

Profiles can pass additional whisper.cpp options through the optional
`[transcription.whisper_cpp]` block. zwhisper owns `--model`,
`--language`, `--output-txt`, `--output-json`, `--output-file`, and
the input audio path; those cannot be overridden via `extra_args`.

```toml
[transcription.whisper_cpp]
threads = 16
processors = 1
no_gpu = true
flash_attn = false
vad = true
vad_model = "/absolute/path/to/silero.bin"
extra_args = ["--zen5-special"]
```

### Global hotkey

Default chord: `Ctrl+Alt+R` (toggles recording on/off).

- **Compositor-native bind:** bind a key to `zwhisper toggle`.
  Example for Sway:
  ```
  bindsym Mod4+Shift+r exec /usr/bin/zwhisper toggle
  ```
- **Portal-backed bind:** `zwhisper hotkey probe` and
  `zwhisper hotkey bind` are the intended CLI surface where the
  desktop portal supports GlobalShortcuts.

Probe the portal availability with `zwhisper hotkey probe`.

## Use

### CLI quick reference

```sh
zwhisper status              # daemon state + active profile + active session
zwhisper toggle              # start/stop recording with the active profile
zwhisper record              # one-shot recording (foreground)
zwhisper transcribe FILE     # transcribe an existing audio file
zwhisper profile list|set|show
zwhisper model ...           # intended model list/download/verify/path surface
zwhisper hotkey probe|status|bind
zwhisper backend health      # local whisper.cpp + cloud backend reachability
```

Run `zwhisper --help` for the full command surface.

### Waybar and manual integration

There is no packaged tray service in the CLI-only product. Panels and
window managers should call the CLI directly. A minimal Waybar custom
module can poll status and toggle recording on click:

```json
"custom/zwhisper": {
    "exec": "zwhisper status",
    "on-click": "zwhisper toggle",
    "interval": 2
}
```

For KDE, GNOME, Sway, Hyprland, or other compositors, create a normal
keyboard shortcut whose command is `/usr/bin/zwhisper toggle`.

## Troubleshooting

### Daemon won't start

```sh
systemctl --user status zwhisperd.service
journalctl --user -u zwhisperd.service -n 200 --no-pager
```

Common causes:

- **PipeWire not running** — `systemctl --user status pipewire wireplumber`.
- **Profile parse error** — invalid `~/.config/zwhisper/profiles/*.toml`.
  Check the journal for the exact line.
- **Secrets file mode wrong** — `chmod 600 ~/.config/zwhisper/secrets.toml`
  and `chmod 700 ~/.config/zwhisper/`.

### `daemon protocol mismatch: expected X, got Y`

A partially upgraded host has a daemon and a client at different
versions. Reinstall both:

```sh
# Arch
sudo pacman -Syu zwhisper
systemctl --user restart zwhisperd.service
```

If the message is `daemon does not implement ProtocolVersion
(pre-0.1.0?). Reinstall the daemon.`, the running daemon is from
before the M8 handshake landed — restart it with the upgraded
binary.

### Hotkey doesn't fire

```sh
zwhisper hotkey probe         # check portal availability
zwhisper hotkey status        # check current binding
```

Outcomes:

- `portal=none` → use your compositor bind with `zwhisper toggle`.
- `portal=<name> GlobalShortcuts=unavailable` → install the matching
  `xdg-desktop-portal-{kde,gnome,wlr}` and restart the user session
  (or `systemctl --user restart xdg-desktop-portal*`).
- `BOUND` but no recording starts → check
  `journalctl --user -u zwhisperd.service` for "no active profile" or
  PipeWire errors.

### No audio captured

```sh
pactl info                    # confirm PipeWire is the active server
pactl list short sources      # mic + monitor source names
```

Check the active profile's `mic` and `monitor` fields; the daemon
log will show the `pipewiresrc target-object=<name>` it resolved.

## Project layout

Cargo workspace. The CLI-only product is the daemon, CLI, shared core
libraries, D-Bus interface, and optional hotkey helper code. Legacy
tray/settings crates may still exist in the tree during the
transition, but they are not part of the packaged product.

```
zwhisper/
├── Cargo.toml                       # workspace root, version 0.1.0
├── README.md
├── CHANGELOG.md
├── IDEA.md                          # architecture spec
├── crates/
│   ├── zwhisperd/                   # bin: D-Bus daemon
│   ├── zwhisper-cli/                # bin: CLI (binary name "zwhisper")
│   ├── zwhisper-core/               # lib: profiles, audio, transcribe, secrets
│   ├── zwhisper-ipc/                # lib: D-Bus wire types + PROTOCOL_VERSION
│   └── zwhisper-hotkey/             # lib: optional GlobalShortcuts portal adapter
├── systemd/                         # zwhisperd.service
├── dbus/                            # cz.zajca.Zwhisper1.service
├── packaging/
│   └── arch/                        # PKGBUILD, install hook, namcap allow-list
├── assets/icons/zwhisper.svg
├── scripts/
│   ├── refresh-checksums.sh         # release tool: refresh ggml checksums
│   ├── m0-soak.sh                   # M0 60-min soak harness
│   └── install-desktop.sh           # legacy desktop helper
└── docs/                            # milestone plans + verification docs
```

See [`IDEA.md`](./IDEA.md) for the architecture rationale, and
[`docs/M0-plan.md`](./docs/M0-plan.md) through
[`docs/M8-plan.md`](./docs/M8-plan.md) for milestone history.

## Development

### Common commands

```sh
cargo fmt --all                                                # format
cargo fmt --all -- --check                                     # CI check
cargo clippy --workspace --all-targets -- -D warnings          # lint
cargo test --workspace                                         # unit + integration tests
cargo test --workspace --features audio-it                     # also run PipeWire-gated tests
cargo build --workspace --release                              # release build
cargo doc --workspace --no-deps --open                         # docs
```

### Run locally without installing

```sh
# Terminal 1: daemon
RUST_LOG=zwhisperd=debug ./target/release/zwhisperd

# Terminal 2: CLI
./target/release/zwhisper status
./target/release/zwhisper toggle
```

The daemon claims `cz.zajca.Zwhisper1` on the user session bus;
you cannot run two instances concurrently.

### CI

Every push and pull request runs:

- `fmt --check` + `clippy --all-targets --all-features -- -D warnings`
- `cargo test --workspace`
- `cargo build --workspace --release`
- `packaging-shell` — shell smoke tests under `packaging/arch/tests/`
  and `docs/tests/` (PKGBUILD metadata, install paths, release-doc
  shape).
- `version-handshake` — focused run of the M8 protocol-version
  test files.

A separate scheduled workflow runs `cargo audit` and `cargo deny` for
vulnerability and license checks.

### Release

See [`docs/RELEASE.md`](./docs/RELEASE.md) for the numbered
release procedure (changelog → version bump → checksum refresh →
tag → PKGBUILD `b2sums` refresh → `makepkg -si` dry-run).

## Contributing

Issues and pull requests welcome on
[GitHub](https://github.com/zajca/zwhisper). Before opening a PR:

1. Read the relevant milestone plan under `docs/M*-plan.md`.
2. Run `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
3. If you change the D-Bus wire surface, update
   [`crates/zwhisper-ipc/tests/wire_freeze.rs`](./crates/zwhisper-ipc/tests/wire_freeze.rs)
   and document the change in the corresponding milestone plan.

## License

[MIT](./LICENSE) © Martin Zajíc
