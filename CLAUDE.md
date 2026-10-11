# Linaux Screen — notes for coding sessions

Spacedesk-style second screen for KDE Plasma (Wayland), viewed in a phone/tablet browser. Target machine: Bazzite. Plan and status: `README.md` and the GitHub issues (the roadmap in the README links them).

## Layout

- `src/main.rs` — CLI flags, startup (single-instance via the port, log file), shutdown/restart (restart = exec self)
- `src/control.rs` — shared state: connected viewers, disconnect-all, exit requests; auto-stop countdown (starts only after a device has connected and the last one left)
- `src/tray.rs` — ksni tray icon (status, copy address, restart, disconnect all, stop); `src/desktop.rs` — notifications + clipboard over D-Bus
- `scripts/install.sh` — host-side install: binary to ~/.local/bin + `io.github.janders1993.LinauxScreen.desktop`
- `src/capture.rs` — xdg-desktop-portal ScreenCast via `ashpd` (virtual monitor or mirror); stores a restore token in `~/.config/linaux-screen/`
- `src/pipeline.rs` — builds GStreamer pipeline descriptions (capture, encoder choice + automatic bitrate, viewer), `element_desc()`, logging helpers, `--capture-only` diagnostic
- `src/hub.rs` — `CaptureHub`: one long-running capture pipeline (pipewiresrc → NV12 → appsink) started at launch, pushing frames to every subscribed viewer
- `src/session.rs` — `Signal` (WebSocket messages) and `Session`: one viewer's pipeline (appsrc → H.264 → rtph264pay → webrtcbin)
- `src/display.rs` — (opt-in via `--fit-device`, experimental: killed fps on KDE+NVIDIA) resizes the virtual monitor to the viewer's screen via `kscreen-doctor` (finds our output by diffing outputs before/after the portal starts; adds a custom mode, sets mode + scale)
- `src/server.rs` — axum: `/` serves the viewer, `/ws` does signalling; up to `MAX_VIEWERS` (4) devices all mirror the same screen, each with its own encode; sends the device count on change; sends `bye` (disconnected / stopped / full) so viewers don't auto-retry; with `--fit-device` only the first device steers the size
- `web/index.html` — the viewer, embedded into the binary with `include_str!`

## Conventions

- Starting is always a deliberate user action (app menu); never auto-start at login or on connection.

- Keep capture in its own pipeline. Capture inside the per-viewer WebRTC pipeline ran at ~1 fps on KWin; separate, it runs at full rate. Also: no `keepalive-time` on pipewiresrc (throttled KWin to ~1 fps), and always request `max-framerate` (otherwise ~1 fps).

- Server is always the WebRTC offerer; the browser answers.
- Encoder/element properties go through `element_desc()`, which drops properties or enum values the installed GStreamer doesn't support, so one unknown property can't break the pipeline.
- Keep the viewer a single dependency-free HTML file.

## Testing without a Plasma desktop

`cargo run -- --source test --encoder x264` streams a test pattern with no portal involved. In test mode, viewer `display` messages resize the test pattern, exercising the same renegotiation path as a real monitor resize. A headless client (e.g. aiortc) can verify the WebRTC path; note aiortc only offers ECDSA DTLS ciphers, so widen its cipher list to include `ECDHE-RSA-*` to match browsers.

Build deps (Debian/Ubuntu): `libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev libdbus-1-dev`; runtime: `gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-nice gstreamer1.0-plugins-ugly`.
