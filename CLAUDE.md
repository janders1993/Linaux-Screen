# Linaux Screen — notes for coding sessions

Spacedesk-style second screen for KDE Plasma (Wayland), viewed in a phone/tablet browser. Target machine: Bazzite. Plan and issue breakdown: `README.md`, `docs/issues.md`, GitHub issues #1–10.

## Layout

- `src/main.rs` — CLI flags, startup, prints the LAN URL
- `src/capture.rs` — xdg-desktop-portal ScreenCast via `ashpd` (virtual monitor or mirror); stores a restore token in `~/.config/linaux-screen/`
- `src/stream.rs` — `CaptureHub`: one long-running capture pipeline (pipewiresrc → NV12 → appsink) started at launch; each viewer `Session` is its own pipeline (appsrc → H.264 → rtph264pay → webrtcbin) fed by the hub. Also signalling message types and debug modes (`--capture-only`, `LINAUX_*` env overrides)
- `src/display.rs` — resizes the virtual monitor to the viewer's screen via `kscreen-doctor` (finds our output by diffing outputs before/after the portal starts; adds a custom mode, sets mode + scale)
- `src/server.rs` — axum: `/` serves the viewer, `/ws` does signalling; one streaming viewer at a time (newest wins)
- `web/index.html` — the viewer, embedded into the binary with `include_str!`

## Conventions

- Keep capture in its own pipeline. Capture inside the per-viewer WebRTC pipeline ran at ~1 fps on KWin; separate, it runs at full rate. Also: no `keepalive-time` on pipewiresrc (throttled KWin to ~1 fps), and always request `max-framerate` (otherwise ~1 fps).

- Server is always the WebRTC offerer; the browser answers.
- Encoder/element properties go through `element_desc()`, which drops properties or enum values the installed GStreamer doesn't support, so one unknown property can't break the pipeline.
- Keep the viewer a single dependency-free HTML file.

## Testing without a Plasma desktop

`cargo run -- --source test --encoder x264` streams a test pattern with no portal involved. In test mode, viewer `display` messages resize the test pattern, exercising the same renegotiation path as a real monitor resize. A headless client (e.g. aiortc) can verify the WebRTC path; note aiortc only offers ECDSA DTLS ciphers, so widen its cipher list to include `ECDHE-RSA-*` to match browsers.

Build deps (Debian/Ubuntu): `libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev libdbus-1-dev`; runtime: `gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-nice gstreamer1.0-plugins-ugly`.
