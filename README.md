# Linaux Screen

Turn any phone or tablet into a second monitor for your Linux desktop — no app install on the device. Inspired by spacedesk, built for KDE Plasma on Wayland (developed and tested on Bazzite).

You start the app on the PC, and your iPad, iPhone or Android device opens `http://<your-pc-ip>:4444` in the browser and becomes an extra (or mirrored) display.

> **Status:** working, used day to day. Runs from the app menu with a tray icon, streams at ~55 fps (1080p) with no noticeable delay, and stops itself when no device is connected. Tested on Bazzite, KDE Plasma, NVIDIA/NVENC, a Galaxy S22 Ultra and an iPad. Touch and keyboard control of the PC, once you accept the device on the PC; pen pressure is planned.

## Goals

- **Extend** — each connected device gets its own real virtual monitor in KDE's display settings.
- **Mirror** — duplicate an existing monitor to a device.
- **Input** — touch as mouse, on-screen and hardware keyboards, pen with pressure and tilt (Apple Pencil, S-Pen).
- **Audio** — desktop audio plays on the device.
- **Zero install on clients** — any modern browser on iOS or Android; tap the video for fullscreen.
- **Low latency** — hardware H.264 encoding, WebRTC transport, target under 60 ms on a good LAN.
- **Wireless only:** no USB mode; the point is several devices over Wi-Fi. A video wall is a stretch goal.

## How it works

```
 KWin virtual output ──► PipeWire ──► capture pipeline (runs from launch)
                                            │  NV12 frames
                                            ▼
                         per-device pipeline: NVENC / VA-API H.264 ──► WebRTC ──► browser
```

Capture runs in its own pipeline for the life of the app and hands frames to one encoding pipeline per connected device. (Keeping them separate was the fix for KDE delivering ~1 fps — see issue #1.)

| Piece | Technology |
|---|---|
| Virtual monitor | KWin virtual output via the ScreenCast portal (virtual source type) |
| Capture | PipeWire → GStreamer, colour-converted to NV12 |
| Encode | GStreamer: `nvh264enc` (NVIDIA), `vah264enc` (AMD/Intel) or `x264enc` (software) |
| Transport | WebRTC (`webrtcbin`), signalling over a WebSocket |
| Server | Rust: `ashpd`, `gstreamer-rs`, `axum`; tray icon via `ksni` |
| Viewer | One plain HTML/JS page served by the app |
| Running | App-menu entry; tray icon while running; stops after a minute with no device |
| Sound | A "Linaux Screen" PipeWire speaker, made the default output while a device is picked → Opus in the same WebRTC session |
| Input | RemoteDesktop portal (same session as the screencast); events over the WebSocket, only from devices accepted on the PC |

Planned, not built yet: pen pressure (uinput).

## Requirements

- KDE Plasma 6 on Wayland, with PipeWire and xdg-desktop-portal-kde
- A GPU with hardware H.264 encoding (or a CPU to spare for software encoding)
- For sound: `pactl` and `pw-cli` (PipeWire tools, normally preinstalled)
- The phone/tablet on the same network, with a modern browser

## Roadmap

Ordered by what matters most day to day; each step is a GitHub issue.

- [x] **Spike** — virtual monitor streamed to a phone browser (#1)
- [x] **Run without a terminal**: app-menu entry, tray icon, auto-stop (#8)
- [x] **Sound**: send the PC's sound to a chosen device from the tray (#3)
- [x] **Several devices at once**: all mirror the same screen (#6)
- [x] **Input**: touch as mouse and hardware keyboards, accepted per device on the PC (#2)
- [ ] **Pen pressure** (#5)
- [ ] **Later**: match the monitor to the device (#4, experimental `--fit-device` flag), video wall (#10)

## Setup

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

**Install** (on the host, after building):

```bash
~/Linaux-Screen/scripts/install.sh --no-build
```

This puts the app in `~/.local/bin` and adds **Linaux Screen** to the app menu. Nothing starts at login. To update later, pull inside the box (that's where you're signed in to GitHub), then re-run the script without `--no-build`; it rebuilds in the box first:

```bash
distrobox enter linaux -- bash -c 'cd ~/Linaux-Screen && git pull'
~/Linaux-Screen/scripts/install.sh
```

`scripts/install.sh --uninstall` removes it again.

## Using it

1. **Start it on the PC.** Open **Linaux Screen** from the app menu or KRunner (Alt+Space). The first time, KDE asks to allow screen sharing and remote control (that covers mouse and keyboard; no device can use them until you accept it, see below). A notification shows the address, and a tray icon appears while it's running.
2. **On the phone or tablet,** open the address, e.g. `http://192.168.1.20:4444`. Saving it as a home-screen bookmark makes this one tap. Tap the video for fullscreen (Android).
3. **When you're done,** just close the page or put the device down. A minute after the last device disconnects, the app stops by itself and the virtual monitor disappears. Nothing counts down while it's waiting for the first device.

Several devices can watch at once (up to four). They all see the same screen, each scaled to fit with black bars where the shapes differ.

The **tray menu** shows who's connected, e.g. *iPad (192.168.1.120)*, and has:
- **Copy address**
- **Restart**: also picks up a newly installed version.
- **Sound output**: *This PC (default)* or any connected device. Picking a device moves all the PC's sound to it and the PC goes quiet. Sound goes back to the PC when you pick *This PC*, the device disconnects, or the app stops or restarts. Nothing changes until you pick.
- **Control requests**: *iPad wants to control this PC* with **Accept** / **Deny**, and *iPad has control — Revoke* while it does.
- **Disconnect all devices**: they show "tap to reconnect".
- **Stop**

A new monitor appears in **System Settings → Display** while it runs. Drag it to where you want it relative to your other screens.

The virtual monitor starts at 1920×1080, letterboxed on the device. You can change its resolution and scale in **System Settings → Display Configuration** (it's listed as *Virtual Output*). KDE remembers the setting, so it's applied every time the app starts. `--fit-device` (experimental) resizes it automatically to match the device and follows rotation, but resizing mid-stream costs a lot of frame rate on KDE + NVIDIA (#4).

On the device: tap with **three fingers** (or press `s`) to show the menu: stats (resolution, fps, bitrate, buffer and decode time, devices connected, sound, control) and the **Request control** button. When a device is picked for sound, it may show **Tap to enable sound**, because browsers only play sound after a tap.

**Controlling the PC from a device.** Nothing a device does reaches the PC until you allow it:
1. On the device, tap with three fingers and press **Request control**. (It's only in that menu, so it can't happen by accident.)
2. On the PC, a notification asks *Allow control of this PC?* with **Accept** / **Deny** (also in the tray). No answer in 30 seconds counts as Deny; after a Deny the device must wait 30 seconds to ask again.
3. While it has control: **tap** to click, **hold** (half a second) to right-click, **drag one finger** to drag (windows, selections), **drag two fingers** to scroll. A **trackpad or mouse** (e.g. the iPad's Magic Keyboard) works like on a PC: the pointer follows it, clicks and two-finger clicks are left and right clicks, two-finger swipes scroll. A physically connected keyboard types on the PC; the iPad's **Cmd** key is the Super (Windows) key. There's no on-screen keyboard. Three-finger taps always open the menu and never reach the PC.
4. Control ends when the device presses **Release control**, disconnects, or you pick **Revoke**, **Disconnect all**, **Restart** or **Stop**. Each device asks separately.

iPadOS keeps a few Cmd shortcuts for itself (Cmd+Tab, Cmd+Space, Cmd+H), so those don't reach the PC. Before control is granted, taps just go fullscreen as before.

**Running from a terminal** works too: `linaux-screen` (with any options below). Logs always go to `~/.local/state/linaux-screen/linaux-screen.log`.

| Flag | Default | Meaning |
|---|---|---|
| `--source virtual\|monitor\|test` | `virtual` | New extended monitor, mirror an existing one, or a test pattern (no capture) |
| `--encoder auto\|vaapi\|nvenc\|x264` | `auto` | H.264 encoder; `auto` tries VA-API, NVENC, then software |
| `--bitrate <kbps>` | automatic | Video bitrate. Automatic picks it from the resolution: about 12000 at 1080p, 34000 at 4K |
| `--port <n>` | `4444` | Port to serve on |
| `--idle-timeout <secs>` | `60` | Stop this long after the last device leaves (`0` = never) |
| `--no-tray` | off | No tray icon |
| `--fit-device` | off | Experimental: match the monitor to the device's screen and rotation (hurts frame rate for now) |

**Troubleshooting**

- Check which GStreamer pieces you have: `gst-inspect-1.0 webrtcbin nicesrc pipewiresrc vah264enc nvh264enc x264enc`
- If something fails, check `~/.local/state/linaux-screen/linaux-screen.log`, or run `RUST_LOG=debug GST_DEBUG=3 linaux-screen` in a terminal and attach the output to an issue.
- If the network path is in doubt, `--source test` streams a moving test pattern with no screen capture involved.
- Firewall: allow TCP 4444 and UDP (WebRTC picks random high ports), e.g. `sudo firewall-cmd --add-port=4444/tcp --add-port=1024-65535/udp` for a quick test.

## Bazzite notes

- Bazzite is immutable: the app is a single binary in `~/.local/bin` plus an app-menu entry. No rpm-ostree layering needed (apart from the `nicesrc` plugin copy above).
- Portals ask for consent on first run; the app stores the restore token so it won't ask every session.
