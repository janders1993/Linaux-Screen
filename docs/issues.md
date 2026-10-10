# Planned issues

One issue per section. Labels in brackets.

## 1. Spike: virtual monitor streamed to a phone browser [milestone-1, spike]
Prove the core pipeline before building anything else.

- [ ] Request a virtual output via the ScreenCast portal (`ashpd`, source type VIRTUAL) and confirm it appears in KDE display settings
- [ ] Pipe the PipeWire node into GStreamer and encode H.264 on the GPU (`vah264enc` / `nvh264enc`, software `x264enc` fallback)
- [ ] Stream with `webrtcbin`; WebSocket signalling served on port 4444
- [ ] Minimal viewer page: full-window `<video>`, auto-connect
- [ ] Test on iOS Safari and Android Chrome
- [ ] Measure glass-to-glass latency (stopwatch on desktop, photo of both screens)

**Done when:** dragging a window onto the virtual monitor shows it on a phone with acceptable latency.

## 2. Touch as mouse + keyboard [milestone-2, input]
- [ ] Pointer Events on the viewer → WebRTC data channel (compact binary messages)
- [ ] RemoteDesktop portal session tied to the screencast stream; absolute pointer motion mapped to the virtual output
- [ ] Tap = click, long-press = right-click, two-finger drag = scroll
- [ ] On-screen keyboard toggle; hardware keyboard events from iPad/Android
- [ ] Store portal restore token so consent is only asked once

## 3. Audio [milestone-3, polish]
- [ ] Capture default sink monitor from PipeWire
- [ ] Opus track in the same WebRTC session
- [ ] Per-device toggle; handle iOS autoplay rules (tap to unmute)

## 4. Rotation, resolution and scaling per device [milestone-3, polish]
- [ ] Viewer reports screen size, devicePixelRatio and orientation on connect
- [ ] Virtual output created at matching resolution; recreate on rotation
- [ ] Sensible default scale factor per device class
- [ ] Bitrate / quality setting

## 5. Pen pressure and tilt [milestone-4, input]
- [ ] Virtual tablet device via uinput (ABS_X/Y, ABS_PRESSURE, ABS_TILT_X/Y, BTN_TOOL_PEN)
- [ ] Map the tablet to the matching virtual output (KWin tablet output mapping)
- [ ] Forward Pointer Events `pressure`, `tiltX`, `tiltY` for pen pointers only
- [ ] udev rule for `/dev/uinput` access; document for Bazzite
- [ ] Test with Krita on Apple Pencil and S-Pen

## 6. Multiple devices + pairing [milestone-5, multi-device]
- [ ] One virtual output and session per connected device
- [ ] Device list with friendly names and connect/disconnect
- [ ] QR code on the desktop with URL and one-time pairing token
- [ ] Reject unpaired clients

## 7. Android USB mode [milestone-5, usb]
- [ ] Detect device via adb and run `adb reverse tcp:4444 tcp:4444`
- [ ] Document setup

## 8. Desktop app shell [milestone-5, packaging]
- [ ] systemd user service
- [ ] Tray icon (StatusNotifierItem): show QR, list devices, quit
- [ ] Config file (port, codec, bitrate)
- [ ] Install script for Bazzite (`~/.local/bin` + unit)

## 9. HTTPS + PWA [milestone-5, viewer]
- [ ] Optional self-signed cert generation
- [ ] Web app manifest and icons for home-screen fullscreen
- [ ] Wake lock to keep the device screen on

## 10. Stretch: video wall + iOS USB [stretch]
- [ ] Arrange several devices as one large surface
- [ ] iOS over USB via `usbmuxd`/`iproxy`
