//! Screen capture through the xdg-desktop-portal ScreenCast interface.
//!
//! In `virtual` mode KDE Plasma creates a brand-new virtual monitor that shows
//! up in System Settings > Display, exactly like plugging in a screen. In
//! `monitor` mode the user picks an existing monitor to mirror.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};

use crate::SourceKind;

/// A running portal screencast. Keep it alive for as long as you stream:
/// dropping it closes the session and KDE removes the virtual monitor.
pub struct Capture {
    pub node_id: u32,
    fd: OwnedFd,
    _session: Session<Screencast>,
    _proxy: Screencast,
}

impl Capture {
    pub fn pipewire_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

fn token_path(kind: SourceKind) -> Option<PathBuf> {
    let name = match kind {
        SourceKind::Virtual => "restore_token_virtual",
        SourceKind::Monitor => "restore_token_monitor",
        SourceKind::Test => return None,
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

#[cfg(test)]
mod tests {
    use super::launcher_id_from_cgroup;

    #[test]
    fn app_menu_launch() {
        let cg = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-io.github.janders1993.LinauxScreen@d56ddd55aa8d4f12.service";
        assert_eq!(launcher_id_from_cgroup(cg).as_deref(), Some("io.github.janders1993.LinauxScreen"));
    }

    #[test]
    fn konsole_launch() {
        let cg = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.kde.konsole@1a2b.service";
        assert_eq!(launcher_id_from_cgroup(cg).as_deref(), Some("org.kde.konsole"));
    }

    #[test]
    fn unknown_launcher() {
        assert_eq!(launcher_id_from_cgroup("0::/system.slice/sshd.service"), None);
    }
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

    let token_file = token_path(kind);
    let saved_token = token_file
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty());

    // A saved permission token only works for the app that was granted it.
    // If KDE refuses it (e.g. it was issued when running from a terminal,
    // where KDE sees the app as Konsole), forget it and ask again.
    let mut token = saved_token;
    let (session, streams) = loop {
        let session = proxy.create_session(Default::default()).await?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(ashpd::enumflags2::BitFlags::from_flag(source))
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await?;
        let result = proxy.start(&session, None, Default::default()).await?.response();
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

    if let (Some(path), Some(token)) = (token_file, streams.restore_token()) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, token) {
            tracing::warn!("could not save restore token to {}: {e}", path.display());
        }
    }

    let stream = streams
        .streams()
        .first()
        .context("the portal returned no streams")?;
    let node_id = stream.pipe_wire_node_id();
    let size = stream.size();

    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;

    tracing::info!("screencast started: PipeWire node {node_id}, size {size:?}");

    Ok(Capture {
        node_id,

        fd,
        _session: session,
        _proxy: proxy,
    })
}
