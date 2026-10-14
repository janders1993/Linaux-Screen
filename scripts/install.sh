#!/usr/bin/env bash
# Installs (or updates) Linaux Screen for the current user. No root needed.
#
# From a release download (the app sits next to this script):
#   ./install.sh               install it
#   ./install.sh --uninstall   remove it again
#
# From a source checkout (scripts/install.sh):
#   scripts/install.sh              build in the "linaux" distrobox, then install
#   scripts/install.sh --no-build   install the already-built binary
#
# Run it on the host (e.g. Bazzite), not inside a distrobox.
set -euo pipefail

APP_ID="io.github.janders1993.LinauxScreen"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOX="${LINAUX_BOX:-linaux}"
BIN_DIR="$HOME/.local/bin"
APPS_DIR="$HOME/.local/share/applications"
PLUGIN_DIR="$HOME/.local/share/gstreamer-1.0/plugins"
ICON_DIR="$HOME/.local/share/icons"
BIN="$BIN_DIR/linaux-screen"
DESKTOP="$APPS_DIR/$APP_ID.desktop"

say() { printf '\033[1m%s\033[0m\n' "$*"; }
warn() { printf '\033[33m! %s\033[0m\n' "$*"; }

if [[ "${1:-}" == "--uninstall" ]]; then
    pkill -x linaux-screen 2>/dev/null || true
    rm -f "$BIN" "$DESKTOP"
    find "$ICON_DIR/hicolor" -name "$APP_ID*.svg" -delete 2>/dev/null || true
    update-desktop-database "$APPS_DIR" 2>/dev/null || true
    say "Removed. (Settings and logs are left in ~/.config/linaux-screen and ~/.local/state/linaux-screen.)"
    exit 0
fi

if [[ -f /run/.containerenv || -n "${DISTROBOX_ENTER_PATH:-}" ]]; then
    echo "Run this on the host, not inside a distrobox." >&2
    exit 1
fi

# A release download has the app right here; a source checkout builds it.
if [[ -x "$HERE/linaux-screen" ]]; then
    BUILT="$HERE/linaux-screen"
    ICONS="$HERE/icons"
else
    REPO="$(cd "$HERE/.." && pwd)"
    if [[ "${1:-}" != "--no-build" ]]; then
        say "Building in the '$BOX' distrobox…"
        distrobox enter "$BOX" -- bash -lc "cd '$REPO' && source ~/.cargo/env && cargo build --release"
    fi
    BUILT="$REPO/target/release/linaux-screen"
    ICONS="$REPO/assets/icons"
    if [[ ! -x "$BUILT" ]]; then
        echo "No build found at $BUILT. Build it first (see docs/DEVELOPMENT.md)." >&2
        exit 1
    fi
fi

# --- Check the streaming pieces this PC needs ------------------------------
has() { gst-inspect-1.0 "$1" >/dev/null 2>&1; }

if ! command -v gst-inspect-1.0 >/dev/null; then
    warn "GStreamer's tools aren't installed, so the streaming pieces can't be checked."
else
    # Bazzite doesn't ship GStreamer's nicesrc (WebRTC networking). Release
    # downloads bring a copy; it goes in your own plugin folder, not the system.
    if ! has nicesrc && [[ -f "$HERE/gstreamer-plugins/libgstnice.so" ]]; then
        say "Adding GStreamer's nicesrc to $PLUGIN_DIR…"
        mkdir -p "$PLUGIN_DIR"
        cp "$HERE/gstreamer-plugins/libgstnice.so" "$PLUGIN_DIR/"
    fi
    missing=()
    for e in webrtcbin nicesrc pipewiresrc dtlsenc srtpenc; do
        has "$e" || missing+=("$e")
    done
    if ((${#missing[@]})); then
        warn "Missing GStreamer pieces: ${missing[*]}. Streaming won't work until they're installed."
        warn "On Fedora: sudo dnf install gstreamer1-plugins-bad-free pipewire-gstreamer libnice-gstreamer1"
    fi
    if ! has nvh264enc && ! has vah264enc; then
        if has x264enc; then
            warn "No hardware H.264 encoder found; using the CPU (x264), which is slower."
        else
            warn "No H.264 encoder found (nvh264enc, vah264enc or x264enc). Streaming won't work."
        fi
    fi
fi
if ! command -v pactl >/dev/null || ! command -v pw-cli >/dev/null; then
    warn "pactl and pw-cli (PipeWire tools) are missing, so sound can't be sent to devices."
fi

# --- Install ---------------------------------------------------------------
say "Installing…"
mkdir -p "$BIN_DIR" "$APPS_DIR"
# Copy to a temp name and rename, so a running copy isn't disturbed.
cp "$BUILT" "$BIN.new"
mv -f "$BIN.new" "$BIN"

# App and tray icons, into your own icon theme folder.
if [[ -d "$ICONS/hicolor" ]]; then
    mkdir -p "$ICON_DIR"
    cp -r "$ICONS/hicolor" "$ICON_DIR/"
    touch "$ICON_DIR/hicolor"
fi

cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=Linaux Screen
GenericName=Second screen
Comment=Use a phone or tablet as a second monitor
Exec=$BIN
Icon=$APP_ID
Terminal=false
Categories=Utility;
Keywords=spacedesk;monitor;display;second screen;tablet;phone;
StartupNotify=false
X-KDE-StartupNotify=false
EOF
update-desktop-database "$APPS_DIR" 2>/dev/null || true

if pgrep -x linaux-screen >/dev/null; then
    say "Installed $("$BIN" --version). It's running now; use Restart in its tray menu to switch to the new version."
else
    say "Installed $("$BIN" --version). Start it from the app menu or KRunner: \"Linaux Screen\"."
fi
