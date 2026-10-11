//! Small desktop integrations over D-Bus: notifications and the clipboard.
//! All best-effort: failures are logged, never fatal.

use std::collections::HashMap;

use zbus::zvariant::Value;

pub const APP_NAME: &str = "Linaux Screen";
pub const ICON: &str = "video-display";

/// Shows a desktop notification.
pub async fn notify(summary: &str, body: &str) {
    if let Err(e) = try_notify(summary, body).await {
        tracing::debug!("couldn't show notification: {e}");
    }
}

async fn try_notify(summary: &str, body: &str) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    let hints: HashMap<&str, Value> = HashMap::new();
    conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            APP_NAME,
            0u32,
            ICON,
            summary,
            body,
            Vec::<&str>::new(),
            hints,
            8000i32,
        ),
    )
    .await?;
    Ok(())
}

/// Shows a notification with buttons and waits for one to be pressed.
/// `actions` are (key, label) pairs; returns the pressed key, or `None` if
/// the notification was closed or `cancel` finished first (the notification
/// is then taken down). If notifications don't work, waits for `cancel`.
pub async fn ask(
    summary: &str,
    body: &str,
    actions: &[(&str, &str)],
    cancel: impl std::future::Future<Output = ()>,
) -> Option<String> {
    tokio::pin!(cancel);
    match try_ask(summary, body, actions, &mut cancel).await {
        Ok(answer) => answer,
        Err(e) => {
            tracing::warn!("couldn't show the notification ({e}); answer from the tray");
            cancel.await;
            None
        }
    }
}

async fn try_ask(
    summary: &str,
    body: &str,
    actions: &[(&str, &str)],
    cancel: &mut std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
) -> zbus::Result<Option<String>> {
    use futures::StreamExt;
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    // Listen before showing it, so a quick answer isn't missed.
    let mut invoked = proxy.receive_signal("ActionInvoked").await?;
    let mut closed = proxy.receive_signal("NotificationClosed").await?;
    let flat: Vec<&str> = actions.iter().flat_map(|(k, l)| [*k, *l]).collect();
    let mut hints: HashMap<&str, Value> = HashMap::new();
    // Critical: stays on screen until answered rather than fading away.
    hints.insert("urgency", Value::U8(2));
    let id: u32 = proxy
        .call(
            "Notify",
            &(APP_NAME, 0u32, ICON, summary, body, flat, hints, 0i32),
        )
        .await?;
    loop {
        tokio::select! {
            Some(msg) = invoked.next() => {
                if let Ok((nid, key)) = msg.body().deserialize::<(u32, String)>() {
                    if nid == id {
                        return Ok(Some(key));
                    }
                }
            }
            Some(msg) = closed.next() => {
                if let Ok((nid, _reason)) = msg.body().deserialize::<(u32, u32)>() {
                    if nid == id {
                        return Ok(None);
                    }
                }
            }
            _ = &mut *cancel => {
                let _ = proxy.call_method("CloseNotification", &(id,)).await;
                return Ok(None);
            }
        }
    }
}

/// Puts `text` on the clipboard: KDE's clipboard service first, then
/// `wl-copy` if it's installed.
pub async fn copy_to_clipboard(text: &str) -> bool {
    let klipper = async {
        let conn = zbus::Connection::session().await?;
        conn.call_method(
            Some("org.kde.klipper"),
            "/klipper",
            Some("org.kde.klipper.klipper"),
            "setClipboardContents",
            &(text,),
        )
        .await?;
        zbus::Result::Ok(())
    };
    match klipper.await {
        Ok(()) => return true,
        Err(e) => tracing::debug!("klipper clipboard failed: {e}"),
    }
    let copied = tokio::process::Command::new("wl-copy")
        .arg(text)
        .status()
        .await
        .is_ok_and(|s| s.success());
    if !copied {
        tracing::warn!("couldn't copy to the clipboard (no klipper or wl-copy)");
    }
    copied
}
