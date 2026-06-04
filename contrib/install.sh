#!/usr/bin/env bash
# zwhisper desktop integration installer.
#
# Installs the helper scripts and example profiles, checks the runtime
# dependencies, and prints the exact lines to add to your Sway and
# Waybar configs. It never edits your compositor or bar config for you.
#
# Usage:  contrib/install.sh
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN_DEST="$HOME/.local/bin"
PROFILE_DEST="${XDG_CONFIG_HOME:-$HOME/.config}/zwhisper/profiles"

say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m  %s\n' "$*"; }

# --- 1. dependency check -------------------------------------------------
say "Checking dependencies"
missing=()
for cmd in zwhisper pw-record pactl ffmpeg wl-copy notify-send jq; do
    command -v "$cmd" >/dev/null 2>&1 || missing+=("$cmd")
done
if [ "${#missing[@]}" -gt 0 ]; then
    warn "Missing: ${missing[*]}"
    warn "On Arch:  sudo pacman -S --needed pipewire wireplumber ffmpeg wl-clipboard libnotify jq"
    warn "And install zwhisper itself (see the main README)."
    echo
fi

# Optional: wtype powers the `type_at_cursor` output (types transcripts at the
# cursor). wlroots compositors only (Sway/Hyprland); on GNOME/KWin or when it is
# absent, zwhisper falls back to clipboard + notification. Not required.
if ! command -v wtype >/dev/null 2>&1; then
    warn "Optional: wtype not found — the type_at_cursor output falls back to clipboard."
    warn "On Arch (wlroots/Sway/Hyprland only):  sudo pacman -S --needed wtype"
    echo
fi

# --- 2. helper scripts ---------------------------------------------------
say "Installing helper scripts into $BIN_DEST"
mkdir -p "$BIN_DEST"
for s in zwhisper-dictate zwhisper-cycle-profile; do
    install -Dm755 "$SCRIPT_DIR/bin/$s" "$BIN_DEST/$s"
    echo "    $BIN_DEST/$s"
done
case ":$PATH:" in
    *":$BIN_DEST:"*) ;;
    *) warn "$BIN_DEST is not on your PATH — add it to your shell profile." ;;
esac

# --- 3. example profiles (never overwrite existing) ----------------------
say "Installing example profiles into $PROFILE_DEST"
mkdir -p "$PROFILE_DEST"
for p in "$SCRIPT_DIR"/profiles/*.toml; do
    name="$(basename "$p")"
    if [ -e "$PROFILE_DEST/$name" ]; then
        echo "    skip (exists): $name"
    else
        install -Dm644 "$p" "$PROFILE_DEST/$name"
        echo "    installed: $name"
    fi
done

# --- 4. next steps -------------------------------------------------------
cat <<EOF

$(say "Almost done. Finish the wiring:")

  Sway:   add this line to ~/.config/sway/config, then \`swaymsg reload\`:
              include $SCRIPT_DIR/sway/zwhisper.conf
          (or copy the bindsym lines from that file directly)

  Waybar: add "custom/zwhisper" to a module list in ~/.config/waybar/config
          and merge the module object from:
              $SCRIPT_DIR/waybar/zwhisper.jsonc
          optional styling: append $SCRIPT_DIR/waybar/style.css to your style.css

  Model:  install a transcription model, e.g.
              zwhisper model install parakeet-tdt-0.6b-v3   # fast local (needs parakeet build)
              zwhisper model install large-v3-turbo-q5_0    # whisper.cpp, higher accuracy

  Profile: pick one
              zwhisper profile set parakeet-fast
              zwhisper profile set whisper-meeting

  Mic:    if dictation comes back empty, your input gain is likely too
          high (broadband noise). Open pavucontrol -> Input Devices and
          lower the mic until your voice peaks around -12 dB.

Try it: press Super+Ctrl+R, speak, press again — the text lands in your clipboard.
EOF
