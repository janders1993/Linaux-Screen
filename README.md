# Linaux Screen

Turn any phone or tablet into a second monitor for your Linux desktop — no app install on the device. Inspired by spacedesk, built for KDE Plasma on Wayland (developed and tested on Bazzite).

The desktop runs a small service. Your iPad, iPhone or Android device opens `http://<your-pc-ip>:4444` in the browser and becomes an extended (or mirrored) display with touch, pen, keyboard and audio.

> **Status:** spike (issue #1). Streaming to a browser works end to end with a test pattern; capturing a real KDE virtual monitor still needs testing on a Plasma desktop.

## Goals

- **Extend** — each connected device gets its own real virtual monitor in KDE's display settings.
- **Mirror** — duplicate an existing monitor to a device.
- **Input** — touch as mouse, on-screen and hardware keyboards, pen with pressure and tilt (Apple Pencil, S-Pen).
- **Audio** — desktop audio plays on the device.
- **Zero install on clients** — any modern browser on iOS or Android; add to home screen for fullscreen.
- **Low latency** — hardware H.264 encoding, WebRTC transport, target under 60 ms on a good LAN.
- **Later:** USB mode (Android via `adb reverse`, iOS via `iproxy`), multi-device video wall.

## How it works

```
 KWin virtual output ──► PipeWire ──► GStreamer (VAAPI / NVENC H.264)
                                            │
                                         webrtcbin ──► browser on phone/tablet
                                            ▲                  │
 RemoteDesktop portal / uinput  ◄── data channel ◄── pointer, pen, keys
```

| Piece | Technology |
|---|---|
| Virtual monitor | KWin virtual output via the ScreenCast portal (virtual source type) |
| Capture | PipeWire |
| Encode | GStreamer, `vah264enc` (AMD/Intel) or `nvh264enc` (NVIDIA) |
| Transport | WebRTC (`webrtcbin`), signalling over WebSocket |
| Mouse / touch / keyboard | xdg-desktop-portal RemoteDesktop (absolute pointer mapped to the stream) |
| Pen pressure / tilt | uinput virtual tablet, mapped to the virtual output |
| Audio | PipeWire capture → Opus in the same WebRTC session |
| Server | Rust: `ashpd`, `gstreamer-rs`, `axum` |
| Viewer | Plain HTML/JS page served by the binary, PWA manifest |
| Running | systemd user service + tray icon |

## Requirements (planned)

- KDE Plasma 6 on Wayland
- PipeWire, xdg-desktop-portal-kde
- GPU with hardware H.264 encoding
- Access to `/dev/uinput` for pen support (udev rule)

## Roadmap

1. **Spike** — create a virtual monitor and stream it to a phone browser; measure latency.
2. **Input** — touch as mouse, keyboard.
3. **Polish** — audio, rotation, per-device resolution and scaling.
4. **Pen** — pressure and tilt via uinput.
5. **Multi-device** — several clients at once, QR-code pairing, Android USB.
6. **Stretch** — video wall, iOS USB.

Each step is tracked as a GitHub issue.

## Building and running (spike)

Bazzite is immutable, so build inside a Fedora distrobox (matching Bazzite's Fedora version), then run the binary on the host. Running on the host means you get Bazzite's GStreamer, including its hardware H.264 encoders, which plain Fedora leaves out.

**Build** (once, and after each `git pull`):

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

**Check the host has the runtime pieces** (in a normal Bazzite terminal):

```bash
for e in webrtcbin nicesrc pipewiresrc dtlsenc srtpenc vah264enc nvh264enc x264enc; do gst-inspect-1.0 $e >/dev/null 2>&1 && echo "OK      $e" || echo "MISSING $e"; done
```

You need `webrtcbin`, `nicesrc`, `pipewiresrc`, `dtlsenc` and `srtpenc`, plus at least one encoder (`vah264enc` on AMD/Intel, `nvh264enc` on NVIDIA, or `x264enc`). If any are missing, open an issue with the output.

Bazzite doesn't ship `nicesrc`. Copy it from the box into your user plugin folder (no reboot or layering needed):

```bash
distrobox enter linaux -- sudo dnf install -y libnice-gstreamer1
mkdir -p ~/.local/share/gstreamer-1.0/plugins
distrobox enter linaux -- cp /usr/lib64/gstreamer-1.0/libgstnice.so ~/.local/share/gstreamer-1.0/plugins/
```

**Run** (on the host):

```bash
~/Linaux-Screen/target/release/linaux-screen
```

It prints a URL such as `http://192.168.1.20:4444`. Open it on your phone or tablet on the same Wi-Fi. KDE asks once to allow screen sharing, then a new monitor appears in **System Settings → Display**. Arrange it and set its resolution there.

Options:

| Flag | Default | Meaning |
|---|---|---|
| `--source virtual\|monitor\|test` | `virtual` | New extended monitor, mirror an existing one, or a test pattern (no capture) |
| `--encoder auto\|vaapi\|nvenc\|x264` | `auto` | H.264 encoder; `auto` tries VA-API, NVENC, then software |
| `--bitrate <kbps>` | `8000` | Video bitrate |
| `--port <n>` | `4444` | Port to serve on |

On the device: tap with **three fingers** (or press `s`) to show stats — resolution, fps, bitrate, buffer and decode time. On Android a tap goes fullscreen; on iPhone use **Share → Add to Home Screen**.

**Troubleshooting**

- Check which GStreamer pieces you have: `gst-inspect-1.0 webrtcbin nicesrc pipewiresrc vah264enc nvh264enc x264enc`
- If something fails, run with `RUST_LOG=debug GST_DEBUG=3 cargo run --release` and attach the output to an issue.
- If the network path is in doubt, `--source test` streams a moving test pattern with no screen capture involved.
- Firewall: allow TCP 4444 and UDP (WebRTC picks random high ports), e.g. `sudo firewall-cmd --add-port=4444/tcp --add-port=1024-65535/udp` for a quick test.

## Bazzite notes

- Bazzite is immutable: the plan is a single static-ish binary in `~/.local/bin` plus a systemd user unit, no rpm-ostree layering needed for normal use.
- Portals ask for consent on first run; the app stores the restore token so it won't ask every session.
