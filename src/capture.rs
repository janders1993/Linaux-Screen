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
    dirs::config_dir().map(|d| d.join("linaux-screen").join(name))
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

    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(ashpd::enumflags2::BitFlags::from_flag(source))
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(saved_token.as_deref()),
        )
        .await?;

    let streams = proxy
        .start(&session, None, Default::default())
        .await?
        .response()
        .context("screen sharing was cancelled or refused")?;

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
