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
        &(APP_NAME, 0u32, ICON, summary, body, Vec::<&str>::new(), hints, 8000i32),
    )
    .await?;
    Ok(())
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
