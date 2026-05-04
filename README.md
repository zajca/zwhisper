# zwhisper

Linux desktop tool for recording PipeWire audio (microphone + system
output) with tray control, profiles, and a transcription pipeline
backed by local [`whisper.cpp`](https://github.com/ggerganov/whisper.cpp)
or cloud backends (Deepgram).

> **Status:** `0.1.0` (M8) — first packageable release. Milestones
> M0–M8 are complete; the M8 manual verification gate
> ([`docs/M8-verification.md`](./docs/M8-verification.md))
> still needs to be exercised on a clean Arch box before tagging
> `v0.1.0`. See [`CHANGELOG.md`](./CHANGELOG.md) and
> [`IDEA.md`](./IDEA.md) for the full architecture and roadmap.

## Features

- **PipeWire-native capture.** Records the microphone *and* the system
  audio monitor into a single FLAC mix, no PulseAudio loopback.
- **Daemon + tray + CLI + settings GUI** — four binaries, single D-Bus
  contract (`cz.zajca.Zwhisper1`).
- **Profiles.** Versioned TOML profiles select the backend, model, and
  capture options. User overrides shadow shipped + embedded profiles.
- **Local whisper.cpp** with optional Deepgram cloud backend (Nova-3).
  Secrets are resolved from `ZWHISPER_<BACKEND>_API_KEY` or
  `~/.config/zwhisper/secrets.toml` (mode 0600 enforced).
- **Global hotkey toggle** via `xdg-desktop-portal` GlobalShortcuts on
  KDE / GNOME / wlroots, or via the window manager's own bind on
  i3/X11.
- **Settings GUI** (FLTK, on-demand) for profile editing, model
  download, hotkey rebind, and whisper.cpp backend health.
- **Protocol-version handshake** — mismatched daemon + client
  binaries refuse to talk and surface a single, actionable error.

## Targets

- **Primary:** Arch Linux + KDE Plasma 6, PipeWire, Wayland-first.
- **Secondary:** GNOME 47+, wlroots compositors (Sway, Hyprland) and
  i3 / X11 — best effort, hotkey rebind degrades gracefully where the
  GlobalShortcuts portal is absent.
- **Out of scope:** non-Linux platforms, PulseAudio-only systems.

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

1. Pulls `makedepends` (`cargo`, `rust>=1.85`, `cmake`, `gcc`,
   X11/Wayland headers, fontconfig, freetype) for the
   `fltk-bundled` build chain.
2. Runs `cargo build --frozen --release --workspace` and
   `cargo test --frozen --release --workspace --lib` (unit tests
   gate the package; integration tests that need a live PipeWire
   bus are skipped here).
3. Installs all four binaries to `/usr/bin/`, the systemd-user
   units to `/usr/lib/systemd/user/`, the D-Bus service file, both
   `.desktop` launchers, the SVG icon, and the example config
   files (see [`packaging/README.md`](./packaging/README.md)
   for the canonical layout).

After install, enable the daemon and tray for your user:

```sh
systemctl --user daemon-reload
systemctl --user enable --now zwhisperd.service
systemctl --user enable --now zwhisper-tray.service
```

The tray autostarts on next login. The daemon is also activated
on demand by D-Bus when any client first calls
`cz.zajca.Zwhisper1`.

### Other distributions

Not yet packaged. The build-from-source path below works on any
modern Linux with PipeWire ≥ 1.0 and Rust ≥ 1.85. Native packages
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
[`rust-toolchain.toml`](./rust-toolchain.toml). You also need
system libraries for PipeWire, GStreamer, D-Bus, and the FLTK GUI.

#### Arch Linux

```sh
sudo pacman -S --needed \
    rust cargo cmake gcc pkgconf \
    gstreamer gst-plugins-base gst-plugins-good gst-plugin-pipewire \
    pipewire wireplumber dbus xdg-desktop-portal libnotify \
    libxft libxcursor libxinerama libxfixes pango fontconfig \
    freetype2 libxkbcommon wayland
```

For KDE Plasma desktops, also install `xdg-desktop-portal-kde`
to enable the GlobalShortcuts hotkey portal. GNOME ships
`xdg-desktop-portal-gnome` by default; wlroots compositors need
`xdg-desktop-portal-wlr`.

#### Fedora / RHEL

```sh
sudo dnf install \
    rust cargo cmake gcc pkgconfig \
    gstreamer1-devel gstreamer1-plugins-base gstreamer1-plugins-good \
    pipewire pipewire-gstreamer wireplumber dbus libnotify \
    libxkbcommon-devel wayland-devel \
    fltk-devel fontconfig-devel freetype-devel \
    libXft-devel libXcursor-devel libXinerama-devel libXfixes-devel \
    pango-devel
```

#### Debian / Ubuntu

```sh
sudo apt install \
    cargo cmake gcc pkg-config \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good gstreamer1.0-pipewire \
    pipewire wireplumber dbus libnotify-dev \
    libxkbcommon-dev libwayland-dev \
    libfltk1.3-dev libfontconfig-dev libfreetype-dev \
    libxft-dev libxcursor-dev libxinerama-dev libxfixes-dev \
    libpango1.0-dev
```

Rust ≥ 1.85 may need to come from `rustup` rather than the distro
repo on older Debian/Ubuntu releases.

### Build

```sh
git clone https://github.com/zajca/zwhisper
cd zwhisper

# Debug build (fast compile, no optimisation)
cargo build --workspace

# Release build (use this for daily driving)
cargo build --workspace --release
```

Release artefacts land in `target/release/`:

| Binary             | Role                                                    |
| ------------------ | ------------------------------------------------------- |
| `zwhisperd`        | D-Bus daemon — owns recording, transcription, profiles. |
| `zwhisper`         | CLI — `record`, `toggle`, `status`, `profile`, `hotkey`, `transcribe`, `backend`. |
| `zwhisper-tray`    | StatusNotifier tray indicator + hotkey listener.        |
| `zwhisper-settings`| FLTK GUI (Profile / Models / Hotkey / WhisperCLI tabs). |

### Manual install (no `makepkg`)

After `cargo build --release`:

```sh
# Binaries
sudo install -Dm755 target/release/zwhisperd        /usr/local/bin/zwhisperd
sudo install -Dm755 target/release/zwhisper         /usr/local/bin/zwhisper
sudo install -Dm755 target/release/zwhisper-tray    /usr/local/bin/zwhisper-tray
sudo install -Dm755 target/release/zwhisper-settings /usr/local/bin/zwhisper-settings

# systemd-user units (note: ExecStart references /usr/bin — edit if
# you installed under /usr/local/bin)
install -Dm644 systemd/zwhisperd.service       ~/.config/systemd/user/zwhisperd.service
install -Dm644 systemd/zwhisper-tray.service   ~/.config/systemd/user/zwhisper-tray.service

# D-Bus auto-activation
sudo install -Dm644 dbus/cz.zajca.Zwhisper1.service \
    /usr/share/dbus-1/services/cz.zajca.Zwhisper1.service

# Desktop entries
sudo install -Dm644 packaging/zwhisper.desktop \
    /usr/share/applications/zwhisper.desktop
sudo install -Dm644 packaging/zwhisper-settings.desktop \
    /usr/share/applications/zwhisper-settings.desktop
sudo install -Dm644 assets/icons/zwhisper.svg \
    /usr/share/icons/hicolor/scalable/apps/zwhisper.svg

systemctl --user daemon-reload
systemctl --user enable --now zwhisperd.service zwhisper-tray.service
```

For a fully-isolated install, prefer `makepkg -si` on Arch — it
puts every file under `pacman` ownership and the
post-install hook refreshes the desktop and icon caches.

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

The Profiles tab in `zwhisper-settings` is the GUI editor for the
same files; it warns on save while the daemon is recording and
defers `Profiles1.reload` until the recording ends.

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

Models live under `~/.local/share/zwhisper/models/`. Use the
**Models** tab in `zwhisper-settings` to download, verify SHA-256,
and manage them. Default download base URL:
`https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}.bin`.

To use a custom mirror or a local cache, copy
[`crates/zwhisper-settings/models.toml.example`](./crates/zwhisper-settings/models.toml.example)
to `~/.config/zwhisper/models.toml` and edit. The `{model}` token
is the only placeholder allowed; HTTPS is required.

### Global hotkey

Default chord: `Ctrl+Alt+R` (toggles recording on/off).

- **KDE / GNOME / Sway with `xdg-desktop-portal-*` installed:** open
  `zwhisper-settings` → **Hotkey** tab → click **Rebind**.
- **i3 / Hyprland / no portal:** bind in your WM config to invoke
  `zwhisper toggle`. Example for i3:
  ```
  bindsym Mod4+Shift+r exec --no-startup-id /usr/bin/zwhisper toggle
  ```

Probe the portal availability with `zwhisper hotkey probe`.

## Use

### CLI quick reference

```sh
zwhisper status              # daemon state + active profile + active session
zwhisper toggle              # start/stop recording with the active profile
zwhisper record              # one-shot recording (foreground)
zwhisper transcribe FILE     # transcribe an existing audio file
zwhisper profile list|set|show
zwhisper hotkey probe|status|bind
zwhisper backend health      # local whisper.cpp + cloud backend reachability
```

Run `zwhisper --help` for the full command surface.

### Tray

The tray icon shows the current state (`idle` / `recording` /
`stopping` / `transcribing`), exposes the profile picker, and shows
toast notifications for transitions, errors, and protocol mismatches.

### Settings GUI

```sh
zwhisper-settings
```

Four tabs: **Profile** (editor + diff viewer), **Models**
(downloader with SHA-256 verification), **Hotkey** (portal-backed
rebind), **WhisperCLI** (whisper.cpp binary health + GGML version
match). Single-instance — a second launch raises the existing window.

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
systemctl --user restart zwhisperd.service zwhisper-tray.service
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

- `portal=NONE` → use your WM's bind (i3/Hyprland section above).
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

### Settings GUI crashes on launch

Most often a missing X11/Wayland or fontconfig library. Verify:

```sh
ldd $(which zwhisper-settings) | grep -i 'not found'
```

Install the missing system library (see
[Build from source § Prerequisites](#prerequisites)).

## Project layout

Cargo workspace. M8 ships eight crates and four binaries.

```
zwhisper/
├── Cargo.toml                       # workspace root, version 0.1.0
├── README.md
├── CHANGELOG.md
├── IDEA.md                          # architecture spec
├── crates/
│   ├── zwhisperd/                   # bin: D-Bus daemon
│   ├── zwhisper-cli/                # bin: CLI (binary name "zwhisper")
│   ├── zwhisper-tray/               # bin: tray indicator
│   ├── zwhisper-settings/           # bin: FLTK GUI
│   ├── zwhisper-core/               # lib: profiles, audio, transcribe, secrets
│   ├── zwhisper-ipc/                # lib: D-Bus wire types + PROTOCOL_VERSION
│   └── zwhisper-hotkey/             # lib: GlobalShortcuts portal adapter
├── systemd/                         # zwhisperd.service, zwhisper-tray.service
├── dbus/                            # cz.zajca.Zwhisper1.service
├── packaging/
│   ├── arch/                        # PKGBUILD, install hook, namcap allow-list
│   ├── zwhisper.desktop             # tray launcher
│   └── zwhisper-settings.desktop    # settings launcher
├── assets/icons/zwhisper.svg
├── scripts/
│   ├── refresh-checksums.sh         # release tool: refresh ggml checksums
│   ├── m0-soak.sh                   # M0 60-min soak harness
│   └── install-desktop.sh
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

# Terminal 2: tray
RUST_LOG=zwhisper_tray=debug ./target/release/zwhisper-tray

# Terminal 3: CLI
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
