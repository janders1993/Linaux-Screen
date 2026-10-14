#!/usr/bin/env bash
# Builds a release download and creates a *draft* GitHub release for it.
# Run inside the "linaux" distrobox, from an up-to-date main:
#
#   scripts/release.sh              build, pack and create the draft release
#   scripts/release.sh --no-upload  only build and pack (into dist/)
#
# The version comes from Cargo.toml; the notes from docs/releases/v<version>.md.
# Nothing is public until you press Publish on the draft on GitHub.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
say() { printf '\033[1m%s\033[0m\n' "$*"; }
die() { echo "$*" >&2; exit 1; }

UPLOAD=1
[[ "${1:-}" == "--no-upload" ]] && UPLOAD=0

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
TAG="v$VERSION"
NOTES="docs/releases/$TAG.md"
NAME="linaux-screen-$VERSION-x86_64"
[[ -n "$VERSION" ]] || die "Couldn't read the version from Cargo.toml."
[[ -f "$NOTES" ]] || die "Write the release notes first: $NOTES"

if ((UPLOAD)); then
    [[ "$(git rev-parse --abbrev-ref HEAD)" == "main" ]] || die "Switch to main first (git checkout main)."
    [[ -z "$(git status --porcelain)" ]] || die "There are uncommitted changes; commit or stash them first."
    git fetch -q origin
    [[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] || die "main isn't up to date with GitHub; run git pull first."
    command -v gh >/dev/null || die "The GitHub CLI (gh) isn't installed here."
    if gh release view "$TAG" >/dev/null 2>&1; then
        die "A release $TAG already exists on GitHub. Bump the version in Cargo.toml for a new one."
    fi
fi

say "Building $TAG…"
[[ -f ~/.cargo/env ]] && source ~/.cargo/env
cargo build --release --locked

# GStreamer's nicesrc, which Bazzite lacks; install.sh adds it only if needed.
NICE="$(gst-inspect-1.0 nicesrc 2>/dev/null | sed -n 's/^ *Filename *//p' | head -1 || true)"
if [[ -z "$NICE" ]]; then
    say "Installing libnice-gstreamer1 in this box, to bundle nicesrc…"
    sudo dnf install -y libnice-gstreamer1 >/dev/null
    NICE="$(gst-inspect-1.0 nicesrc 2>/dev/null | sed -n 's/^ *Filename *//p' | head -1)"
fi
[[ -f "$NICE" ]] || die "Couldn't find GStreamer's nicesrc plugin to bundle."

say "Packing $NAME.tar.gz…"
rm -rf "dist/$NAME" "dist/$NAME.tar.gz" "dist/$NAME.tar.gz.sha256"
mkdir -p "dist/$NAME/gstreamer-plugins"
cp target/release/linaux-screen scripts/install.sh README.md LICENSE "dist/$NAME/"
cp "$NICE" "dist/$NAME/gstreamer-plugins/libgstnice.so"
cat > "dist/$NAME/gstreamer-plugins/README.txt" <<TXT
libgstnice.so is GStreamer's nicesrc/nicesink plugin from libnice
(https://libnice.freedesktop.org), dual-licensed LGPL-2.1-or-later / MPL-1.1,
taken unmodified from Fedora's libnice-gstreamer1 package. install.sh copies
it into ~/.local/share/gstreamer-1.0/plugins only if your system lacks it.
TXT
tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"
(cd dist && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
say "Built dist/$NAME.tar.gz"

if ((UPLOAD)); then
    say "Creating the draft release $TAG on GitHub…"
    gh release create "$TAG" "dist/$NAME.tar.gz" "dist/$NAME.tar.gz.sha256" \
        --draft --target main --title "Linaux Screen $VERSION" --notes-file "$NOTES"
    say "Done. Check the draft on GitHub (Releases), then press Publish."
fi
