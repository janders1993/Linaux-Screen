# Linaux Screen

Turn any phone or tablet into a second monitor for your Linux desktop — no app install on the device. Inspired by spacedesk, built for KDE Plasma on Wayland (developed and tested on Bazzite).

The desktop runs a small service. Your iPad, iPhone or Android device opens `http://<your-pc-ip>:4444` in the browser and becomes an extended (or mirrored) display with touch, pen, keyboard and audio.

> **Status:** early planning. Nothing works yet — see the [roadmap](#roadmap) and the issues.

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

## Building

Not yet. Instructions arrive with the spike.

## Bazzite notes

- Bazzite is immutable: the plan is a single static-ish binary in `~/.local/bin` plus a systemd user unit, no rpm-ostree layering needed for normal use.
- Portals ask for consent on first run; the app stores the restore token so it won't ask every session.
