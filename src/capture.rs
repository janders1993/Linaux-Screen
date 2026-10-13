//! Screen capture through the xdg-desktop-portal ScreenCast interface.
//!
//! In `virtual` mode KDE Plasma creates a brand-new virtual monitor that shows
//! up in System Settings > Display, exactly like plugging in a screen. In
//! `monitor` mode the user picks an existing monitor to mirror.
//!
//! The screencast is started inside a RemoteDesktop session, so the same
//! one-time permission also covers mouse and keyboard. That permission alone
//! never lets a device control the PC: each device still has to ask, and be
//! accepted on the PC (see `input.rs`). If RemoteDesktop isn't available we
//! fall back to a plain screencast without input.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream,
};
use ashpd::desktop::{PersistMode, ResponseError, Session};
use ashpd::enumflags2::BitFlags;

use crate::SourceKind;

/// A running portal screencast. Keep it alive for as long as you stream:
/// dropping it closes the session and KDE removes the virtual monitor.
pub struct Capture {
    pub node_id: u32,
    fd: OwnedFd,
    /// Mouse and keyboard on this PC, if the portal granted them.
    pub input: Option<Arc<RemoteInput>>,
    _session: CastSession,
    _proxy: Screencast,
}

enum CastSession {
    Screencast(#[allow(dead_code)] Session<Screencast>),
    Remote(#[allow(dead_code)] Arc<RemoteInput>),
}

/// A portal RemoteDesktop session we can send pointer and keyboard events
/// through.
pub struct RemoteInput {
    pub proxy: RemoteDesktop,
    pub session: Session<RemoteDesktop>,
    /// The screencast stream that absolute pointer positions refer to.
    pub stream: u32,
    /// The stream's size in logical pixels, when the portal says.
    pub logical_size: Option<(i32, i32)>,
}

impl Capture {
    pub fn pipewire_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

fn token_path(kind: SourceKind, remote: bool) -> Option<PathBuf> {
    let name = match (kind, remote) {
        (SourceKind::Virtual, false) => "restore_token_virtual",
        (SourceKind::Monitor, false) => "restore_token_monitor",
        (SourceKind::Virtual, true) => "restore_token_virtual_remote",
        (SourceKind::Monitor, true) => "restore_token_monitor_remote",
        (SourceKind::Test, _) => return None,
    };
    // KDE grants permission per app, and decides which app we are from how
    // we were launched (the app menu vs. a terminal like Konsole). Keep one
    // token per launcher so switching between them doesn't re-prompt.
    let file = match launcher_id() {
        Some(id) => format!("{name}-{id}"),
        None => name.to_owned(),
    };
    dirs::config_dir().map(|d| d.join("linaux-screen").join(file))
}

/// The app KDE will attribute us to, from our systemd unit, e.g.
/// `app-io.github.janders1993.LinauxScreen@1234.service` or
/// `app-org.kde.konsole@5678.service` → `io.github.janders1993.LinauxScreen`.
fn launcher_id() -> Option<String> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    launcher_id_from_cgroup(&cgroup)
}

fn launcher_id_from_cgroup(cgroup: &str) -> Option<String> {
    let unit = cgroup.lines().last()?.rsplit('/').next()?;
    let unit = unit.strip_prefix("app-")?;
    let unit = unit
        .strip_suffix(".service")
        .or_else(|| unit.strip_suffix(".scope"))
        .unwrap_or(unit);
    let id = unit.split('@').next()?;
    let id: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    (!id.is_empty()).then_some(id)
}

fn read_token(path: Option<&PathBuf>) -> Option<String> {
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

fn save_token(path: Option<PathBuf>, token: Option<&str>) {
    let (Some(path), Some(token)) = (path, token) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, token) {
        tracing::warn!("could not save restore token to {}: {e}", path.display());
    }
}

fn is_cancelled(e: &ashpd::Error) -> bool {
    matches!(e, ashpd::Error::Response(ResponseError::Cancelled))
}

pub async fn start(kind: SourceKind) -> Result<Capture> {
    let source = match kind {
        SourceKind::Virtual => SourceType::Virtual,
        SourceKind::Monitor => SourceType::Monitor,
        SourceKind::Test => bail!("test source does not use the portal"),
    };

    let proxy = Screencast::new()
        .await
        .context("could not reach the ScreenCast portal — is xdg-desktop-portal-kde running?")?;

    let available = proxy.available_source_types().await?;
    if !available.contains(source) {
        bail!(
            "the portal does not offer {source:?} sources (available: {available:?}). \
             Virtual monitors need KDE Plasma 6 on Wayland."
        );
    }

    match start_remote(kind, source).await {
        Ok(capture) => return Ok(capture),
        Err(Fallback::Cancelled) => bail!("screen sharing was cancelled"),
        Err(Fallback::Unavailable(e)) => {
            tracing::warn!(
                "mouse and keyboard control unavailable ({e:#}); sharing the screen only"
            );
        }
    }
    start_screencast(kind, source, proxy).await
}

enum Fallback {
    /// The user said no; don't ask again.
    Cancelled,
    /// RemoteDesktop didn't work; try a plain screencast.
    Unavailable(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Fallback {
    fn from(e: E) -> Self {
        Fallback::Unavailable(e.into())
    }
}

/// Screencast plus pointer and keyboard, in one portal session.
async fn start_remote(
    kind: SourceKind,
    source: SourceType,
) -> std::result::Result<Capture, Fallback> {
    let cast = Screencast::new().await?;
    let remote = RemoteDesktop::new().await?;
    let devices = remote.available_device_types().await?;
    let wanted = DeviceType::Keyboard | DeviceType::Pointer;
    if !devices.contains(wanted) {
        return Err(anyhow::anyhow!("the portal offers only {devices:?}").into());
    }

    let token_file = token_path(kind, true);
    let mut token = read_token(token_file.as_ref());
    let (session, selected) = loop {
        let session = remote.create_session(Default::default()).await?;
        remote
            .select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(wanted)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await?;
        // Remote desktop sessions are restored through SelectDevices, so no
        // persist mode or token here.
        cast.select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(BitFlags::from_flag(source))
                .set_multiple(false),
        )
        .await?;
        match remote
            .start(&session, None, Default::default())
            .await?
            .response()
        {
            Ok(selected) => break (session, selected),
            Err(e) if token.is_some() => {
                tracing::warn!("saved screen-sharing permission was refused ({e}); asking again");
                if let Some(path) = &token_file {
                    let _ = std::fs::remove_file(path);
                }
                token = None;
            }
            Err(e) if is_cancelled(&e) => return Err(Fallback::Cancelled),
            Err(e) => return Err(e.into()),
        }
    };
    save_token(token_file, selected.restore_token());

    let stream = selected
        .streams()
        .first()
        .cloned()
        .context("the portal returned no streams")?;
    let fd = cast
        .open_pipe_wire_remote(&session, Default::default())
        .await?;
    let granted = selected.devices();
    log_stream(&stream);
    tracing::info!("remote desktop granted: {granted:?}");

    let node_id = stream.pipe_wire_node_id();
    let input = Arc::new(RemoteInput {
        proxy: remote,
        session,
        stream: node_id,
        logical_size: stream.size(),
    });
    Ok(Capture {
        node_id,
        fd,
        input: granted.contains(wanted).then(|| input.clone()),
        _session: CastSession::Remote(input),
        _proxy: cast,
    })
}

/// A screencast on its own, without input.
async fn start_screencast(
    kind: SourceKind,
    source: SourceType,
    proxy: Screencast,
) -> Result<Capture> {
    // A saved permission token only works for the app that was granted it.
    // If KDE refuses it (e.g. it was issued when running from a terminal,
    // where KDE sees the app as Konsole), forget it and ask again.
    let token_file = token_path(kind, false);
    let mut token = read_token(token_file.as_ref());
    let (session, streams) = loop {
        let session = proxy.create_session(Default::default()).await?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(BitFlags::from_flag(source))
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await?;
        let result = proxy
            .start(&session, None, Default::default())
            .await?
            .response();
        match result {
            Ok(streams) => break (session, streams),
            Err(e) if token.is_some() => {
                tracing::warn!("saved screen-sharing permission was refused ({e}); asking again");
                if let Some(path) = &token_file {
                    let _ = std::fs::remove_file(path);
                }
                token = None;
            }
            Err(e) => return Err(e).context("screen sharing was cancelled or refused"),
        }
    };
    save_token(token_file, streams.restore_token());

    let stream = streams
        .streams()
        .first()
        .context("the portal returned no streams")?;
    log_stream(stream);
    let node_id = stream.pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;

    Ok(Capture {
        node_id,
        fd,
        input: None,
        _session: CastSession::Screencast(session),
        _proxy: proxy,
    })
}

fn log_stream(stream: &Stream) {
    tracing::info!(
        "screencast started: PipeWire node {}, size {:?}",
        stream.pipe_wire_node_id(),
        stream.size()
    );
}

#[cfg(test)]
mod tests {
    use super::launcher_id_from_cgroup;

    #[test]
    fn app_menu_launch() {
        let cg = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-io.github.janders1993.LinauxScreen@d56ddd55aa8d4f12.service";
        assert_eq!(
            launcher_id_from_cgroup(cg).as_deref(),
            Some("io.github.janders1993.LinauxScreen")
        );
    }

    #[test]
    fn konsole_launch() {
        let cg = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.kde.konsole@1a2b.service";
        assert_eq!(
            launcher_id_from_cgroup(cg).as_deref(),
            Some("org.kde.konsole")
        );
    }

    #[test]
    fn unknown_launcher() {
        assert_eq!(
            launcher_id_from_cgroup("0::/system.slice/sshd.service"),
            None
        );
    }
}
