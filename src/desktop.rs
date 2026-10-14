//! Small desktop integrations over D-Bus: notifications and the clipboard.
//! All best-effort: failures are logged, never fatal.

use std::collections::HashMap;

use zbus::zvariant::Value;

pub const APP_NAME: &str = "Linaux Screen";
/// The app's icon names, installed by `install.sh` into the user's icon
/// theme (see `assets/icons`).
const APP_ICON: &str = "io.github.janders1993.LinauxScreen";
const TRAY_ICON: &str = "io.github.janders1993.LinauxScreen-tray";
const TRAY_ATTENTION_ICON: &str = "io.github.janders1993.LinauxScreen-tray-attention";
/// KDE's generic icon, for a copy run without its icons installed.
const FALLBACK_ICON: &str = "video-display";

/// Whether `name` is installed in any icon theme folder we know of.
fn icon_installed(name: &str) -> bool {
    let mut dirs: Vec<std::path::PathBuf> = dirs::data_dir().into_iter().collect();
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(system.split(':').filter(|d| !d.is_empty()).map(Into::into));
    dirs.iter().any(|d| {
        d.join("icons/hicolor/scalable/apps")
            .join(format!("{name}.svg"))
            .exists()
    })
}

fn icon_or_fallback(name: &'static str) -> &'static str {
    if icon_installed(name) {
        name
    } else {
        FALLBACK_ICON
    }
}

/// The app icon (app menu, notifications).
pub fn app_icon() -> &'static str {
    static ICON: std::sync::OnceLock<&str> = std::sync::OnceLock::new();
    ICON.get_or_init(|| icon_or_fallback(APP_ICON))
}

/// The tray icon, and the one shown while a device is asking for control.
pub fn tray_icons() -> (&'static str, &'static str) {
    static ICONS: std::sync::OnceLock<(&str, &str)> = std::sync::OnceLock::new();
    *ICONS.get_or_init(|| {
        if icon_installed(TRAY_ICON) {
            (TRAY_ICON, icon_or_fallback(TRAY_ATTENTION_ICON))
        } else {
            (FALLBACK_ICON, FALLBACK_ICON)
        }
    })
}

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
            app_icon(),
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
            &(APP_NAME, 0u32, app_icon(), summary, body, flat, hints, 0i32),
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
