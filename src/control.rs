//! Shared app state for the tray, the viewers and the auto-stop countdown.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

/// Why the app is shutting down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Stop from the tray, or Ctrl+C.
    Stop,
    /// Restart from the tray: shut down, then start a fresh copy.
    Restart,
    /// No device for the whole countdown.
    Idle,
}

/// The most devices that can watch at once; more are turned away.
pub const MAX_VIEWERS: usize = 4;

/// A connected device.
#[derive(Clone, Debug, PartialEq)]
pub struct Viewer {
    pub addr: SocketAddr,
    /// What the device says it is ("iPad", "Android phone", …), once known.
    pub name: Option<String>,
}

impl Viewer {
    /// e.g. "iPad (192.168.1.120)" or just the address.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("{name} ({})", self.addr.ip()),
            None => self.addr.ip().to_string(),
        }
    }
}

pub struct Control {
    /// Devices currently connected, in the order they connected.
    pub viewers: watch::Sender<Vec<Viewer>>,
    /// Bumped by "Disconnect all devices".
    pub kick: watch::Sender<u64>,
    /// When the auto-stop countdown ends, if it's running.
    pub countdown: watch::Sender<Option<Instant>>,
    /// Set once to shut the app down.
    pub exit: watch::Sender<Option<Exit>>,
    /// Which device plays the PC's sound; `None` = the PC itself.
    pub audio_target: watch::Sender<Option<SocketAddr>>,
    /// Whether sound can be sent to devices at all (PipeWire tools present).
    pub audio_available: bool,
    /// The address devices connect to, e.g. `http://192.168.1.20:4444`.
    pub address: String,
}

impl Control {
    #[cfg(test)]
    pub fn new(address: String) -> Self {
        Self::with_audio(address, false)
    }

    pub fn with_audio(address: String, audio_available: bool) -> Self {
        Self {
            audio_target: watch::Sender::new(None),
            audio_available,
            viewers: watch::Sender::new(Vec::new()),
            kick: watch::Sender::new(0),
            countdown: watch::Sender::new(None),
            exit: watch::Sender::new(None),
            address,
        }
    }

    pub fn request_exit(&self, why: Exit) {
        self.exit.send_if_modified(|e| {
            if e.is_none() {
                *e = Some(why);
                true
            } else {
                false
            }
        });
    }

    pub fn disconnect_all(&self) {
        self.kick.send_modify(|k| *k += 1);
    }

    /// Registers a connected device; it's unregistered when the guard drops.
    /// Returns `None` if `MAX_VIEWERS` devices are already connected.
    pub fn viewer_connected(&self, peer: SocketAddr) -> Option<ViewerGuard<'_>> {
        let mut added = false;
        self.viewers.send_if_modified(|v| {
            if v.len() < MAX_VIEWERS {
                v.push(Viewer {
                    addr: peer,
                    name: None,
                });
                added = true;
            }
            added
        });
        added.then_some(ViewerGuard {
            control: self,
            peer,
        })
    }

    /// Records the name a device gave itself.
    pub fn set_viewer_name(&self, peer: SocketAddr, name: String) {
        self.viewers.send_modify(|v| {
            if let Some(viewer) = v.iter_mut().find(|v| v.addr == peer) {
                viewer.name = Some(name);
            }
        });
    }

    /// Whether `peer` is the device that connected first among those still
    /// connected.
    pub fn is_first_viewer(&self, peer: SocketAddr) -> bool {
        self.viewers
            .borrow()
            .first()
            .is_some_and(|v| v.addr == peer)
    }
}

pub struct ViewerGuard<'a> {
    control: &'a Control,
    peer: SocketAddr,
}

impl Drop for ViewerGuard<'_> {
    fn drop(&mut self) {
        let peer = self.peer;
        // Sound goes back to the PC if this device was playing it.
        self.control.audio_target.send_if_modified(|t| {
            if *t == Some(peer) {
                *t = None;
                true
            } else {
                false
            }
        });
        self.control.viewers.send_modify(|v| {
            if let Some(i) = v.iter().position(|v| v.addr == peer) {
                v.remove(i);
            }
        });
    }
}

/// Stops the app once no device has been connected for `timeout`. The
/// countdown only starts after a device has connected and then the last one
/// has left; before the first connection the app waits indefinitely. A device
/// connecting cancels the countdown.
pub async fn run_auto_stop(control: &Control, timeout: Duration) {
    let mut viewers = control.viewers.subscribe();
    let mut had_viewer = false;
    loop {
        let count = viewers.borrow_and_update().len();
        if count > 0 {
            had_viewer = true;
            control.countdown.send_replace(None);
        } else if had_viewer {
            let deadline = Instant::now() + timeout;
            control.countdown.send_replace(Some(deadline));
            tracing::info!(
                "no devices connected; stopping in {}s unless one connects",
                timeout.as_secs()
            );
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::info!("no devices for {}s, stopping", timeout.as_secs());
                    control.request_exit(Exit::Idle);
                    return;
                }
                changed = viewers.changed() => if changed.is_err() { return },
            }
            continue;
        }
        if viewers.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([192, 168, 1, 9], port))
    }

    #[tokio::test(start_paused = true)]
    async fn waits_forever_before_first_device() {
        let control = Control::new(String::new());
        let result = tokio::time::timeout(
            Duration::from_secs(3600),
            run_auto_stop(&control, Duration::from_secs(60)),
        )
        .await;
        assert!(
            result.is_err(),
            "auto-stop fired with no device ever connected"
        );
        assert_eq!(*control.exit.borrow(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn stops_after_last_device_leaves() {
        let control = std::sync::Arc::new(Control::new(String::new()));
        let c = control.clone();
        let task = tokio::spawn(async move { run_auto_stop(&c, Duration::from_secs(60)).await });
        {
            let _guard = control.viewer_connected(addr(1)).unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        } // device leaves
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(*control.exit.borrow(), None, "stopped too early");
        assert!(control.countdown.borrow().is_some());
        tokio::time::sleep(Duration::from_secs(31)).await;
        task.await.unwrap();
        assert_eq!(*control.exit.borrow(), Some(Exit::Idle));
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_cancels_countdown() {
        let control = std::sync::Arc::new(Control::new(String::new()));
        let c = control.clone();
        tokio::spawn(async move { run_auto_stop(&c, Duration::from_secs(60)).await });
        drop(control.viewer_connected(addr(1)).unwrap());
        tokio::time::sleep(Duration::from_secs(50)).await;
        let _back = control.viewer_connected(addr(2)).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(control.countdown.borrow().is_none());
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(*control.exit.borrow(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn switching_devices_does_not_start_countdown() {
        let control = std::sync::Arc::new(Control::new(String::new()));
        let c = control.clone();
        tokio::spawn(async move { run_auto_stop(&c, Duration::from_secs(60)).await });
        let phone = control.viewer_connected(addr(1)).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ipad = control.viewer_connected(addr(2)).unwrap();
        drop(phone);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(control.countdown.borrow().is_none());
    }

    #[test]
    fn turns_away_devices_beyond_the_limit() {
        let control = Control::new(String::new());
        let guards: Vec<_> = (0..MAX_VIEWERS as u16)
            .map(|i| {
                control
                    .viewer_connected(addr(i))
                    .expect("room for this one")
            })
            .collect();
        assert!(control.viewer_connected(addr(99)).is_none());
        assert_eq!(control.viewers.borrow().len(), MAX_VIEWERS);
        drop(guards);
        assert!(control.viewer_connected(addr(99)).is_some());
    }

    #[test]
    fn first_viewer_and_names() {
        let control = Control::new(String::new());
        let ipad = control.viewer_connected(addr(1)).unwrap();
        let _phone = control.viewer_connected(addr(2)).unwrap();
        assert!(control.is_first_viewer(addr(1)));
        assert!(!control.is_first_viewer(addr(2)));
        control.set_viewer_name(addr(1), "iPad".into());
        assert_eq!(control.viewers.borrow()[0].label(), "iPad (192.168.1.9)");
        drop(ipad);
        assert!(control.is_first_viewer(addr(2)));
    }

    #[test]
    fn audio_falls_back_to_pc_when_device_leaves() {
        let control = Control::with_audio(String::new(), true);
        let ipad = control.viewer_connected(addr(1)).unwrap();
        let phone = control.viewer_connected(addr(2)).unwrap();
        control.audio_target.send_replace(Some(addr(1)));
        drop(phone); // someone else leaving changes nothing
        assert_eq!(*control.audio_target.borrow(), Some(addr(1)));
        drop(ipad);
        assert_eq!(*control.audio_target.borrow(), None);
    }
}
