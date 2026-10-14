# Linaux Screen — notes for coding sessions

Spacedesk-style second screen for KDE Plasma (Wayland), viewed in a phone/tablet browser. Target machine: Bazzite. Plan and status: `README.md` and the GitHub issues (the roadmap in the README links them).

## Layout

- `src/main.rs` — CLI flags, startup (single-instance via the port, log file), shutdown/restart (restart = exec self)
- `src/control.rs` — shared state: connected viewers, disconnect-all, exit requests, input `Access` (pending/granted, deny cooldown by IP); auto-stop countdown (starts only after a device has connected and the last one left)
- `src/input.rs` — accepted input → portal calls (`InputSink`; logs instead in test mode), `DeviceInput` tracks held keys/buttons and releases them when control ends, `ask_host` (Accept/Deny notification, 30 s = deny), `evdev_key` (KeyboardEvent.code → Linux keycode)
- `src/tray.rs` — ksni tray icon (status, control requests Accept/Deny/Revoke, copy address, sound output, restart, disconnect all, stop); `src/desktop.rs` — notifications (incl. `ask` with action buttons) + clipboard over D-Bus
- `scripts/install.sh` — host-side install: binary to ~/.local/bin + `io.github.janders1993.LinauxScreen.desktop`
- `src/capture.rs` — xdg-desktop-portal via `ashpd`: a RemoteDesktop session with the ScreenCast (virtual monitor or mirror) inside it, so one prompt covers input; falls back to screencast-only. Restore tokens per launcher in `~/.config/linaux-screen/` (`_remote` ones for the combined session)
- `src/pipeline.rs` — builds GStreamer pipeline descriptions (capture, encoder choice + automatic bitrate, viewer), `element_desc()`, logging helpers, `--capture-only` diagnostic
- `src/hub.rs` — `CaptureHub`: one long-running capture pipeline (pipewiresrc → NV12 → appsink) started at launch, pushing frames to every subscribed viewer; optional per-viewer fps cap, with a catch-up thread that sends a skipped frame once due, so a still screen is never stale
- `src/quality.rs` — per-device presets (Battery saver / Smooth / Sharp / Ultra): fps cap + bitrate multiplier, applied live via `Session::set_quality` (encoder is named `enc`). No memory: each connection starts on Smooth
- `src/session.rs` — `Signal` (WebSocket messages) and `Session`: one viewer's pipeline (appsrc → H.264 → rtph264pay → webrtcbin)
- `src/audio.rs` — sound to one chosen device: a `linaux-screen` null sink owned by a `pw-cli` child (PR_SET_PDEATHSIG, so it vanishes if the app dies), set as default via `pactl` (previous default saved in ~/.local/state for crash recovery), captured with `pulsesrc` → `AudioRouter` → the chosen viewer's `asrc`. Ignored tests exercise it against a real PipeWire; they share it, so run them one at a time: `cargo test -- --ignored --test-threads=1`.
- `src/server.rs` — axum: `/` serves the viewer, `/ws` does signalling; up to `MAX_VIEWERS` (4) devices all mirror the same screen, each with its own encode; sends the device count on change; sends `bye` (disconnected / stopped / full) so viewers don't auto-retry
- `web/index.html` — the viewer, embedded into the binary with `include_str!`

## Conventions

- Starting is always a deliberate user action (app menu); never auto-start at login or on connection. Likewise sound: nothing changes until a device is picked in the tray; the PC is always the fallback. No microphone.

- Keep capture in its own pipeline. Capture inside the per-viewer WebRTC pipeline ran at ~1 fps on KWin; separate, it runs at full rate. Also: no `keepalive-time` on pipewiresrc (throttled KWin to ~1 fps), and always request `max-framerate` (otherwise ~1 fps).

- Server is always the WebRTC offerer; the browser answers. It offers once: size changes travel in-band in H.264 (a re-offer crashed webrtcbin 1.24 with the audio track).
- Input: nothing from a device reaches the PC until the PC accepts it. The server checks `has_control` on every input message (the viewer also sends nothing before approval). Request control lives only in the three-finger menu. No on-screen keyboard.
- Removed on purpose (in git history): `--fit-device` (resizing the virtual monitor to each device via `kscreen-doctor`; killed fps on KDE + NVIDIA, manual Display settings won) and `--dmabuf` (GL capture path, ~20 fps vs ~55 on NVIDIA).
- Encoder/element properties go through `element_desc()`, which drops properties or enum values the installed GStreamer doesn't support, so one unknown property can't break the pipeline.
- Keep the viewer a single dependency-free HTML file.

## Testing without a Plasma desktop

`cargo run -- --source test --encoder x264` streams a test pattern with no portal involved. In test mode, accepted input is logged (`test input: …`) instead of sent. Control requests still go through the notification; without a notification server they time out to Deny after 30 s (or accept from the tray). A headless client (e.g. aiortc) can verify the WebRTC path; note aiortc only offers ECDSA DTLS ciphers, so widen its cipher list to include `ECDHE-RSA-*` to match browsers.

Build deps (Debian/Ubuntu): `libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev libdbus-1-dev`; runtime: `gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-nice gstreamer1.0-plugins-ugly`.
