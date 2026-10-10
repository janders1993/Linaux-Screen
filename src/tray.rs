//! System tray icon, shown while the app runs.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ksni::TrayMethods;
use tokio::time::Instant;

use crate::control::{Control, Exit};
use crate::desktop;

struct ScreenTray {
    control: Arc<Control>,
    runtime: tokio::runtime::Handle,
    viewers: Vec<SocketAddr>,
    stop_at: Option<Instant>,
}

impl ScreenTray {
    fn status(&self) -> String {
        if !self.viewers.is_empty() {
            let who: Vec<String> = self.viewers.iter().map(|p| p.ip().to_string()).collect();
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
        desktop::ICON.into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: desktop::APP_NAME.into(),
            description: format!("{}\n{}", self.status(), self.control.address),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: self.status(),
                enabled: false,
                ..Default::default()
            }
            .into(),
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
        ]
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
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        let v = viewers.borrow_and_update().clone();
        let c = *countdown.borrow_and_update();
        handle
            .update(|t| {
                t.viewers = v;
                t.stop_at = c;
            })
            .await;
        tokio::select! {
            r = viewers.changed() => if r.is_err() { break },
            r = countdown.changed() => if r.is_err() { break },
            // Refresh the "stopping in Ns" text once a second.
            _ = tick.tick(), if c.is_some() => {}
        }
    }
}
