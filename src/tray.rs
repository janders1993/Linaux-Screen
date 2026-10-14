//! System tray icon, shown while the app runs.

use std::sync::Arc;
use std::time::Duration;

use ksni::TrayMethods;
use tokio::time::Instant;

use crate::control::{Access, Control, Exit, Viewer};
use crate::desktop;

struct ScreenTray {
    control: Arc<Control>,
    runtime: tokio::runtime::Handle,
    viewers: Vec<Viewer>,
    stop_at: Option<Instant>,
    audio_target: Option<std::net::SocketAddr>,
    access: Access,
}

impl ScreenTray {
    /// Requests waiting for an answer (Accept / Deny), and devices that have
    /// control (Revoke).
    fn control_items(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let mut items = Vec::new();
        for &peer in &self.access.pending {
            items.push(
                StandardItem {
                    label: format!(
                        "{} wants to control this PC",
                        self.control.viewer_label(peer)
                    ),
                    icon_name: "dialog-warning".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "    Accept".into(),
                    icon_name: "dialog-ok".into(),
                    activate: Box::new(move |this: &mut Self| {
                        this.control.answer(peer, true);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "    Deny".into(),
                    icon_name: "dialog-cancel".into(),
                    activate: Box::new(move |this: &mut Self| {
                        this.control.answer(peer, false);
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        for &peer in &self.access.granted {
            items.push(
                StandardItem {
                    label: format!("{} has control — Revoke", self.control.viewer_label(peer)),
                    icon_name: "input-mouse".into(),
                    activate: Box::new(move |this: &mut Self| this.control.revoke(peer)),
                    ..Default::default()
                }
                .into(),
            );
        }
        items
    }

    /// "Sound output" submenu: This PC, then each connected device.
    fn sound_menu(&self) -> ksni::MenuItem<Self> {
        use ksni::menu::*;
        if !self.control.audio_available {
            return StandardItem {
                label: "Sound output: unavailable (needs pactl and pw-cli)".into(),
                enabled: false,
                ..Default::default()
            }
            .into();
        }
        let mut options = vec![RadioItem {
            label: "This PC (default)".into(),
            ..Default::default()
        }];
        options.extend(self.viewers.iter().map(|v| RadioItem {
            label: v.label(),
            ..Default::default()
        }));
        let selected = self
            .audio_target
            .and_then(|t| self.viewers.iter().position(|v| v.addr == t))
            .map_or(0, |i| i + 1);
        let current = if selected == 0 {
            "This PC".to_owned()
        } else {
            self.viewers[selected - 1].label()
        };
        SubMenu {
            label: format!("Sound output: {current}"),
            icon_name: "audio-volume-high".into(),
            submenu: vec![RadioGroup {
                selected,
                select: Box::new(|this: &mut Self, index| {
                    let target = index
                        .checked_sub(1)
                        .and_then(|i| this.viewers.get(i))
                        .map(|v| v.addr);
                    this.control.audio_target.send_replace(target);
                }),
                options,
            }
            .into()],
            ..Default::default()
        }
        .into()
    }

    fn status_line(&self) -> String {
        if !self.viewers.is_empty() {
            let who: Vec<String> = self.viewers.iter().map(Viewer::label).collect();
            format!("Connected: {}", who.join(", "))
        } else if let Some(at) = self.stop_at {
            let secs = at.saturating_duration_since(Instant::now()).as_secs();
            format!("No devices — stopping in {secs}s")
        } else {
            "Waiting for a device".into()
        }
    }
}

impl ksni::Tray for ScreenTray {
    fn id(&self) -> String {
        "linaux-screen".into()
    }

    fn title(&self) -> String {
        desktop::APP_NAME.into()
    }

    fn icon_name(&self) -> String {
        desktop::tray_icons().0.into()
    }

    fn attention_icon_name(&self) -> String {
        desktop::tray_icons().1.into()
    }

    fn status(&self) -> ksni::Status {
        if self.access.pending.is_empty() {
            ksni::Status::Active
        } else {
            ksni::Status::NeedsAttention
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: desktop::APP_NAME.into(),
            description: format!("{}\n{}", self.status_line(), self.control.address),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let mut menu: Vec<ksni::MenuItem<Self>> = vec![StandardItem {
            label: self.status_line(),
            enabled: false,
            ..Default::default()
        }
        .into()];
        menu.extend(self.control_items());
        menu.extend([
            StandardItem {
                label: format!("Copy address ({})", self.control.address),
                icon_name: "edit-copy".into(),
                activate: Box::new(|this: &mut Self| {
                    let address = this.control.address.clone();
                    this.runtime.spawn(async move {
                        if desktop::copy_to_clipboard(&address).await {
                            desktop::notify("Address copied", &address).await;
                        }
                    });
                }),
                ..Default::default()
            }
            .into(),
            self.sound_menu(),
            MenuItem::Separator,
            StandardItem {
                label: "Restart".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|this: &mut Self| this.control.request_exit(Exit::Restart)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Disconnect all devices".into(),
                icon_name: "network-disconnect".into(),
                enabled: !self.viewers.is_empty(),
                activate: Box::new(|this: &mut Self| this.control.disconnect_all()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Stop".into(),
                icon_name: "process-stop".into(),
                activate: Box::new(|this: &mut Self| this.control.request_exit(Exit::Stop)),
                ..Default::default()
            }
            .into(),
        ]);
        menu
    }
}

/// Shows the tray icon and keeps it up to date until the app exits. If
/// there's no tray to show it in, logs a warning and returns; the app keeps
/// working without it.
pub async fn run(control: Arc<Control>) {
    let tray = ScreenTray {
        control: control.clone(),
        runtime: tokio::runtime::Handle::current(),
        viewers: Vec::new(),
        stop_at: None,
        audio_target: None,
        access: Access::default(),
    };
    let handle = match tray.spawn().await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("no tray icon (is a system tray running?): {e}");
            return;
        }
    };

    let mut viewers = control.viewers.subscribe();
    let mut countdown = control.countdown.subscribe();
    let mut audio = control.audio_target.subscribe();
    let mut access = control.access.subscribe();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        let v = viewers.borrow_and_update().clone();
        let c = *countdown.borrow_and_update();
        let a = *audio.borrow_and_update();
        let x = access.borrow_and_update().clone();
        handle
            .update(|t| {
                t.viewers = v;
                t.stop_at = c;
                t.audio_target = a;
                t.access = x;
            })
            .await;
        tokio::select! {
            r = viewers.changed() => if r.is_err() { break },
            r = countdown.changed() => if r.is_err() { break },
            r = audio.changed() => if r.is_err() { break },
            r = access.changed() => if r.is_err() { break },
            // Refresh the "stopping in Ns" text once a second.
            _ = tick.tick(), if c.is_some() => {}
        }
    }
}
