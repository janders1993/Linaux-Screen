# Development notes

How Linaux Screen works, how to build it, and where it's heading. For using it, see the [README](../README.md).

## How it works

```
 KWin virtual output ──► PipeWire ──► capture pipeline (runs from launch)
                                            │  NV12 frames
                                            ▼
                         per-device pipeline: NVENC / VA-API H.264 ──► WebRTC ──► browser
```

Capture runs in its own pipeline for the life of the app and hands frames to one encoding pipeline per connected device. Keeping them separate was the fix for KDE delivering ~1 fps (see #1); `CLAUDE.md` lists the other rules learned the hard way.

| Piece | Technology |
|---|---|
| Virtual monitor | KWin virtual output via the ScreenCast portal (virtual source type) |
| Capture | PipeWire → GStreamer, colour-converted to NV12 |
| Encode | GStreamer: `nvh264enc` (NVIDIA), `vah264enc` (AMD/Intel) or `x264enc` (software) |
| Transport | WebRTC (`webrtcbin`), signalling over a WebSocket |
| Server | Rust: `ashpd`, `gstreamer-rs`, `axum`; tray icon via `ksni` |
| Viewer | One dependency-free HTML/JS page served by the app (`web/index.html`) |
| Sound | A "Linaux Screen" PipeWire speaker, made the default output while a device is picked → Opus in the same WebRTC session |
| Input | RemoteDesktop portal (same session as the screencast); events over the WebSocket, only from devices accepted on the PC |

`CLAUDE.md` has a file-by-file map of the source.

## Building from source

Bazzite is immutable, so build inside a Fedora distrobox matching Bazzite's Fedora version, then run the binary on the host. Running on the host means you get Bazzite's GStreamer, including its hardware H.264 encoders, which plain Fedora leaves out.

```bash
distrobox create --name linaux --image registry.fedoraproject.org/fedora:$(rpm -E %fedora)
distrobox enter linaux

sudo dnf install -y git gh gcc pkgconf-pkg-config dbus-devel \
  gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-bad-free-devel
curl --proto '=https' -sSf https://sh.rustup.rs | sh -s -- -y && source ~/.cargo/env

git clone https://github.com/janders1993/Linaux-Screen ~/Linaux-Screen
cd ~/Linaux-Screen && cargo build --release
exit
```

On a regular distribution you can build directly. The build needs the GStreamer development packages (base and bad), D-Bus development files and Rust.

**Install your build** (on the host): `~/Linaux-Screen/scripts/install.sh --no-build`.

**Update:** pull, then run `scripts/install.sh` without `--no-build`. It rebuilds in the `linaux` box first (set `LINAUX_BOX` for another name).

```bash
distrobox enter linaux -- bash -c 'cd ~/Linaux-Screen && git pull'
~/Linaux-Screen/scripts/install.sh
```

### Runtime pieces

Check the host has what streaming needs:

```bash
for e in webrtcbin nicesrc pipewiresrc dtlsenc srtpenc vah264enc nvh264enc x264enc; do gst-inspect-1.0 $e >/dev/null 2>&1 && echo "OK      $e" || echo "MISSING $e"; done
```

You need `webrtcbin`, `nicesrc`, `pipewiresrc`, `dtlsenc` and `srtpenc`, plus at least one encoder: `vah264enc` (AMD/Intel), `nvh264enc` (NVIDIA) or `x264enc`.

Bazzite doesn't ship `nicesrc`. Copy it from the box into your user plugin folder (no reboot or layering needed):

```bash
distrobox enter linaux -- sudo dnf install -y libnice-gstreamer1
mkdir -p ~/.local/share/gstreamer-1.0/plugins
distrobox enter linaux -- cp /usr/lib64/gstreamer-1.0/libgstnice.so ~/.local/share/gstreamer-1.0/plugins/
```

## Command-line options

Run `linaux-screen` from a terminal to use these. Logs always go to `~/.local/state/linaux-screen/linaux-screen.log`.

| Flag | Default | Meaning |
|---|---|---|
| `--source virtual\|monitor\|test` | `virtual` | New extended monitor, mirror an existing one, or a test pattern (no capture) |
| `--encoder auto\|vaapi\|nvenc\|x264` | `auto` | H.264 encoder; `auto` tries VA-API, NVENC, then software |
| `--bitrate <kbps>` | automatic | Base (Smooth) bitrate. Automatic picks it from the resolution: about 12000 at 1080p, 34000 at 4K |
| `--fps <n>` | `60` | Frame rate to ask the compositor for |
| `--port <n>` | `4444` | Port to serve on |
| `--idle-timeout <secs>` | `60` | Stop this long after the last device leaves (`0` = never) |
| `--no-tray` | off | No tray icon |
| `--capture-only` | off | Diagnostic: only capture and count frames |

## Testing

- `cargo test` runs the unit tests.
- `cargo test -- --ignored --test-threads=1` adds the sound tests, which need a running PipeWire.
- `linaux-screen --source test --encoder x264` streams a test pattern with no screen capture, which is handy for checking the network path. Accepted input is logged instead of sent.
- For debugging, run with `RUST_LOG=debug GST_DEBUG=3`.

## Making a release

1. Bump `version` in `Cargo.toml` and write the notes in `docs/releases/v<version>.md`. Merge both to main.
2. Inside the `linaux` box, on an up-to-date main, run `scripts/release.sh`. It:
   1. builds the release binary;
   2. packs `dist/linaux-screen-<version>-x86_64.tar.gz`: the app, `install.sh`, the icons, the README, the licence, and GStreamer's `nicesrc` plugin for systems that lack it;
   3. creates a **draft** GitHub release with the download attached.

   `scripts/release.sh --no-upload` only builds and packs.
3. Check the draft under *Releases* on GitHub, then press **Publish**. Publishing creates the `v<version>` tag on main.

## Roadmap

Each step is a GitHub issue.

- [x] **Spike:** virtual monitor streamed to a phone browser (#1)
- [x] **Run without a terminal:** app-menu entry, tray icon, auto-stop (#8)
- [x] **Sound:** send the PC's sound to a chosen device from the tray (#3)
- [x] **Several devices at once:** all mirror the same screen (#6)
- [x] **Input:** touch as mouse, trackpads and hardware keyboards, accepted per device on the PC (#2)
- [x] **Quality presets:** Battery saver / Smooth / Sharp / Ultra per device (#21)
- [ ] **Parked:** switch between Extend and Mirror from the device (#20), pinch to zoom (#22), video wall (#10)

Deliberately out of scope: a microphone, USB connections (it's wireless only), an on-screen keyboard, pen pressure (#5), and resizing the monitor automatically to each device (#4).
