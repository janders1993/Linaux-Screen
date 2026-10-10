#!/usr/bin/env bash
# Installs (or updates) Linaux Screen for the current user. No root needed.
#
#   scripts/install.sh              build in the "linaux" distrobox, then install
#   scripts/install.sh --no-build   install the already-built binary
#   scripts/install.sh --uninstall  remove it again
#
# Run it on the host (Bazzite), not inside the distrobox.
set -euo pipefail

APP_ID="io.github.janders1993.LinauxScreen"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BOX="${LINAUX_BOX:-linaux}"
BIN_DIR="$HOME/.local/bin"
APPS_DIR="$HOME/.local/share/applications"
BIN="$BIN_DIR/linaux-screen"
DESKTOP="$APPS_DIR/$APP_ID.desktop"

say() { printf '\033[1m%s\033[0m\n' "$*"; }

if [[ "${1:-}" == "--uninstall" ]]; then
    pkill -x linaux-screen 2>/dev/null || true
    rm -f "$BIN" "$DESKTOP"
    update-desktop-database "$APPS_DIR" 2>/dev/null || true
    say "Removed. (Settings and logs are left in ~/.config/linaux-screen and ~/.local/state/linaux-screen.)"
    exit 0
fi

if [[ -f /run/.containerenv || -n "${DISTROBOX_ENTER_PATH:-}" ]]; then
    echo "Run this on the host, not inside the distrobox." >&2
    exit 1
fi

if [[ "${1:-}" != "--no-build" ]]; then
    say "Building in the '$BOX' distrobox…"
    distrobox enter "$BOX" -- bash -lc "cd '$REPO' && source ~/.cargo/env && cargo build --release"
fi

BUILT="$REPO/target/release/linaux-screen"
if [[ ! -x "$BUILT" ]]; then
    echo "No build found at $BUILT. Build it first (see README)." >&2
    exit 1
fi

say "Installing…"
mkdir -p "$BIN_DIR" "$APPS_DIR"
# Copy to a temp name and rename, so a running copy isn't disturbed.
cp "$BUILT" "$BIN.new"
mv -f "$BIN.new" "$BIN"

cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=Linaux Screen
GenericName=Second screen
Comment=Use a phone or tablet as a second monitor
Exec=$BIN
Icon=video-display
Terminal=false
Categories=Utility;
Keywords=spacedesk;monitor;display;second screen;tablet;phone;
StartupNotify=false
X-KDE-StartupNotify=false
EOF
update-desktop-database "$APPS_DIR" 2>/dev/null || true

if pgrep -x linaux-screen >/dev/null; then
    say "Installed. It's running now; use Restart in its tray menu to switch to the new version."
else
    say "Installed. Start it from the app menu or KRunner: \"Linaux Screen\"."
fi
