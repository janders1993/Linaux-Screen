//! Mouse and keyboard from a device, once the PC has accepted it.
//!
//! Gating happens in `server.rs` (with the state in `control.rs`): events
//! from a device without control are dropped before they get here. This
//! module turns accepted events into portal calls, remembers which keys and
//! buttons a device is holding so they're let go when its control ends, and
//! asks the person at the PC to accept or deny.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use ashpd::desktop::remote_desktop::{KeyState, NotifyPointerAxisOptions};
use tokio::sync::mpsc;

use crate::capture::RemoteInput;
use crate::control::{Control, REQUEST_TIMEOUT};
use crate::desktop;
use crate::hub::CaptureHub;

const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;
/// More held keys than any real typing needs; guards against a page that
/// never sends key-ups.
const MAX_HELD_KEYS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cmd {
    /// Absolute position, 0..1 across the shared screen.
    Motion {
        x: f64,
        y: f64,
    },
    Button {
        code: i32,
        pressed: bool,
    },
    Scroll {
        dx: f64,
        dy: f64,
        end: bool,
    },
    Key {
        code: i32,
        pressed: bool,
    },
}

/// Where accepted input goes: the portal, or the log in test mode. Events
/// are sent in order on one task.
#[derive(Clone)]
pub struct InputSink {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl InputSink {
    /// Sends input to the PC through the portal session.
    pub fn portal(remote: Arc<RemoteInput>, hub: Arc<CaptureHub>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_portal(remote, hub, rx));
        Self { tx }
    }

    /// Logs input instead of sending it (test pattern mode).
    pub fn logging() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Cmd>();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                tracing::info!("test input: {cmd:?}");
            }
        });
        Self { tx }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

async fn run_portal(
    remote: Arc<RemoteInput>,
    hub: Arc<CaptureHub>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let session = &remote.session;
    let proxy = &remote.proxy;
    // The portal wants positions in the stream's logical pixels. It tells us
    // that size at the start; if the monitor's mode changes later, scale it
    // by how much the captured frames changed.
    let mut first_frame: Option<(u32, u32)> = None;
    let mut warned = false;
    while let Some(cmd) = rx.recv().await {
        let result = match cmd {
            Cmd::Motion { x, y } => {
                let frame = hub.frame_size();
                if first_frame.is_none() {
                    first_frame = frame;
                }
                let (w, h) = logical_size(remote.logical_size, first_frame, frame);
                proxy
                    .notify_pointer_motion_absolute(
                        session,
                        remote.stream,
                        x * w,
                        y * h,
                        Default::default(),
                    )
                    .await
            }
            Cmd::Button { code, pressed } => {
                proxy
                    .notify_pointer_button(session, code, key_state(pressed), Default::default())
                    .await
            }
            Cmd::Scroll { dx, dy, end } => {
                proxy
                    .notify_pointer_axis(
                        session,
                        dx,
                        dy,
                        NotifyPointerAxisOptions::default().set_finish(end),
                    )
                    .await
            }
            Cmd::Key { code, pressed } => {
                proxy
                    .notify_keyboard_keycode(session, code, key_state(pressed), Default::default())
                    .await
            }
        };
        if let Err(e) = result {
            if !warned {
                tracing::warn!("couldn't send input to the PC: {e}");
                warned = true;
            }
        }
    }
}

fn key_state(pressed: bool) -> KeyState {
    if pressed {
        KeyState::Pressed
    } else {
        KeyState::Released
    }
}

/// The stream's current size in logical pixels.
fn logical_size(
    portal: Option<(i32, i32)>,
    first_frame: Option<(u32, u32)>,
    frame: Option<(u32, u32)>,
) -> (f64, f64) {
    match (portal, first_frame, frame) {
        (Some((lw, lh)), Some((fw0, fh0)), Some((fw, fh))) if fw0 > 0 && fh0 > 0 => (
            lw as f64 * fw as f64 / fw0 as f64,
            lh as f64 * fh as f64 / fh0 as f64,
        ),
        (Some((lw, lh)), _, _) => (lw as f64, lh as f64),
        (None, _, Some((fw, fh))) => (fw as f64, fh as f64),
        (None, _, None) => (1920.0, 1080.0),
    }
}

/// One device's input while it has control. Remembers what it's holding
/// down, and lets go of everything when control ends or it's dropped.
pub struct DeviceInput {
    sink: InputSink,
    keys: HashSet<i32>,
    buttons: HashSet<i32>,
}

impl DeviceInput {
    pub fn new(sink: InputSink) -> Self {
        Self {
            sink,
            keys: HashSet::new(),
            buttons: HashSet::new(),
        }
    }

    pub fn pointer(&mut self, x: f64, y: f64) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        self.sink.send(Cmd::Motion {
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
        });
    }

    pub fn button(&mut self, button: &str, down: bool) {
        let code = match button {
            "left" => BTN_LEFT,
            "right" => BTN_RIGHT,
            "middle" => BTN_MIDDLE,
            _ => return,
        };
        // Only send changes, so a lost or repeated message can't leave a
        // button stuck or release one that wasn't pressed.
        let changed = if down {
            self.buttons.insert(code)
        } else {
            self.buttons.remove(&code)
        };
        if changed {
            self.sink.send(Cmd::Button {
                code,
                pressed: down,
            });
        }
    }

    pub fn scroll(&mut self, dx: f64, dy: f64, end: bool) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.sink.send(Cmd::Scroll {
            dx: dx.clamp(-2000.0, 2000.0),
            dy: dy.clamp(-2000.0, 2000.0),
            end,
        });
    }

    pub fn key(&mut self, code: &str, down: bool) {
        let Some(code) = evdev_key(code) else {
            return;
        };
        let changed = if down {
            self.keys.len() < MAX_HELD_KEYS && self.keys.insert(code)
        } else {
            self.keys.remove(&code)
        };
        if changed {
            self.sink.send(Cmd::Key {
                code,
                pressed: down,
            });
        }
    }

    /// Lets go of every key and button this device is holding.
    pub fn release_all(&mut self) {
        for code in self.keys.drain() {
            self.sink.send(Cmd::Key {
                code,
                pressed: false,
            });
        }
        for code in self.buttons.drain() {
            self.sink.send(Cmd::Button {
                code,
                pressed: false,
            });
        }
    }
}

impl Drop for DeviceInput {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// Asks the person at the PC whether `peer` may take control, with a
/// notification that has Accept and Deny buttons. The same request can be
/// answered from the tray instead. No answer within `REQUEST_TIMEOUT`, or
/// closing the notification, counts as Deny.
pub async fn ask_host(control: Arc<Control>, peer: SocketAddr) {
    let label = control.viewer_label(peer);
    tracing::info!("{label} asks for control of the mouse and keyboard");
    let mut access = control.access.subscribe();
    let answered_elsewhere = async {
        let _ = tokio::time::timeout(
            REQUEST_TIMEOUT,
            access.wait_for(|a| !a.pending.contains(&peer)),
        )
        .await;
    };
    let choice = desktop::ask(
        "Allow control of this PC?",
        &format!(
            "{label} wants to use this PC's mouse and keyboard. \
             Only accept if it's your device."
        ),
        &[("deny", "Deny"), ("accept", "Accept")],
        answered_elsewhere,
    )
    .await;
    let accept = choice.as_deref() == Some("accept");
    // If the tray answered first, or the device left, this does nothing.
    if control.answer(peer, accept) {
        tracing::info!(
            "{label}: control {}",
            if accept { "accepted" } else { "denied" }
        );
    }
}

/// Linux key code for a browser `KeyboardEvent.code` (the physical key, so
/// the PC's own keyboard layout decides which character it types).
pub fn evdev_key(code: &str) -> Option<i32> {
    if let Some(c) = code.strip_prefix("Key") {
        const LETTERS: [i32; 26] = [
            30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47,
            17, 45, 21, 44,
        ];
        let b = c.as_bytes();
        if b.len() == 1 && b[0].is_ascii_uppercase() {
            return Some(LETTERS[(b[0] - b'A') as usize]);
        }
        return None;
    }
    if let Some(d) = code.strip_prefix("Digit") {
        return match d.parse::<i32>().ok()? {
            0 => Some(11),
            n @ 1..=9 => Some(n + 1),
            _ => None,
        };
    }
    if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<i32>().ok()) {
        return match n {
            1..=10 => Some(58 + n),
            11 => Some(87),
            12 => Some(88),
            13..=24 => Some(170 + n),
            _ => None,
        };
    }
    Some(match code {
        "Escape" => 1,
        "Minus" => 12,
        "Equal" => 13,
        "Backspace" => 14,
        "Tab" => 15,
        "BracketLeft" => 26,
        "BracketRight" => 27,
        "Enter" => 28,
        "ControlLeft" => 29,
        "Semicolon" => 39,
        "Quote" => 40,
        "Backquote" => 41,
        "ShiftLeft" => 42,
        "Backslash" => 43,
        "Comma" => 51,
        "Period" => 52,
        "Slash" => 53,
        "ShiftRight" => 54,
        "NumpadMultiply" => 55,
        "AltLeft" => 56,
        "Space" => 57,
        "CapsLock" => 58,
        "NumLock" => 69,
        "ScrollLock" => 70,
        "Numpad7" => 71,
        "Numpad8" => 72,
        "Numpad9" => 73,
        "NumpadSubtract" => 74,
        "Numpad4" => 75,
        "Numpad5" => 76,
        "Numpad6" => 77,
        "NumpadAdd" => 78,
        "Numpad1" => 79,
        "Numpad2" => 80,
        "Numpad3" => 81,
        "Numpad0" => 82,
        "NumpadDecimal" => 83,
        "IntlBackslash" => 86,
        "IntlRo" => 89,
        "NumpadEnter" => 96,
        "ControlRight" => 97,
        "NumpadDivide" => 98,
        "PrintScreen" => 99,
        "AltRight" => 100,
        "Home" => 102,
        "ArrowUp" => 103,
        "PageUp" => 104,
        "ArrowLeft" => 105,
        "ArrowRight" => 106,
        "End" => 107,
        "ArrowDown" => 108,
        "PageDown" => 109,
        "Insert" => 110,
        "Delete" => 111,
        "AudioVolumeMute" => 113,
        "AudioVolumeDown" => 114,
        "AudioVolumeUp" => 115,
        "NumpadEqual" => 117,
        "Pause" => 119,
        "NumpadComma" => 121,
        "IntlYen" => 124,
        // The iPad's Cmd keys report as Meta: Super on the PC.
        "MetaLeft" | "OSLeft" => 125,
        "MetaRight" | "OSRight" => 126,
        "ContextMenu" => 127,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_codes() {
        assert_eq!(evdev_key("KeyA"), Some(30));
        assert_eq!(evdev_key("KeyQ"), Some(16));
        assert_eq!(evdev_key("KeyZ"), Some(44));
        assert_eq!(evdev_key("KeyM"), Some(50));
        assert_eq!(evdev_key("Digit1"), Some(2));
        assert_eq!(evdev_key("Digit0"), Some(11));
        assert_eq!(evdev_key("F1"), Some(59));
        assert_eq!(evdev_key("F10"), Some(68));
        assert_eq!(evdev_key("F12"), Some(88));
        assert_eq!(evdev_key("F13"), Some(183));
        assert_eq!(evdev_key("MetaLeft"), Some(125));
        assert_eq!(evdev_key("MetaRight"), Some(126));
        assert_eq!(evdev_key("IntlBackslash"), Some(86));
        assert_eq!(evdev_key("Keyboard"), None);
        assert_eq!(evdev_key("Digit10"), None);
        assert_eq!(evdev_key("Fn"), None);
        assert_eq!(evdev_key(""), None);
    }

    fn collecting() -> (InputSink, mpsc::UnboundedReceiver<Cmd>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (InputSink { tx }, rx)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Vec<Cmd> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn lets_go_of_held_keys_and_buttons() {
        let (sink, mut rx) = collecting();
        let mut input = DeviceInput::new(sink);
        input.key("MetaLeft", true);
        input.key("MetaLeft", true); // repeat: ignored
        input.button("left", true);
        input.key("KeyA", true);
        input.key("KeyA", false);
        assert_eq!(drain(&mut rx).len(), 4);
        drop(input);
        let mut released = drain(&mut rx);
        released.sort_by_key(|c| format!("{c:?}"));
        assert_eq!(
            released,
            vec![
                Cmd::Button {
                    code: BTN_LEFT,
                    pressed: false
                },
                Cmd::Key {
                    code: 125,
                    pressed: false
                },
            ]
        );
    }

    #[test]
    fn ignores_releases_of_things_not_held() {
        let (sink, mut rx) = collecting();
        let mut input = DeviceInput::new(sink);
        input.button("left", false);
        input.key("KeyA", false);
        input.button("thumb", true);
        input.key("NotAKey", true);
        input.pointer(f64::NAN, 0.5);
        assert!(drain(&mut rx).is_empty());
        input.pointer(1.5, -1.0);
        assert_eq!(drain(&mut rx), vec![Cmd::Motion { x: 1.0, y: 0.0 }]);
    }

    #[test]
    fn logical_size_follows_mode_changes() {
        // 1920x1080 at 125%: the portal says 1536x864 logical.
        let portal = Some((1536, 864));
        let first = Some((1920, 1080));
        assert_eq!(logical_size(portal, first, first), (1536.0, 864.0));
        assert_eq!(
            logical_size(portal, first, Some((2560, 1440))),
            (2048.0, 1152.0)
        );
        assert_eq!(logical_size(None, None, Some((1280, 720))), (1280.0, 720.0));
    }
}
