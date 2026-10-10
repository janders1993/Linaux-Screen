mod capture;
mod control;
mod desktop;
mod display;
mod server;
mod stream;
mod tray;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, ValueEnum};

use control::Exit;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SourceKind {
    /// A new virtual monitor that extends your desktop
    Virtual,
    /// Mirror an existing monitor
    Monitor,
    /// Built-in test pattern (no screen capture; for checking the network path)
    Test,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum EncoderKind {
    /// VA-API, then NVENC, then software x264
    Auto,
    /// AMD / Intel hardware encoding
    Vaapi,
    /// NVIDIA hardware encoding
    Nvenc,
    /// Software encoding (CPU heavy, works everywhere)
    X264,
}

/// Use a phone or tablet as a second monitor, through the browser.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Port the viewer is served on
    #[arg(short, long, default_value_t = 4444)]
    port: u16,

    /// What to show on the device
    #[arg(short, long, value_enum, default_value_t = SourceKind::Virtual)]
    source: SourceKind,

    /// H.264 encoder
    #[arg(short, long, value_enum, default_value_t = EncoderKind::Auto)]
    encoder: EncoderKind,

    /// Video bitrate in kbit/s [default: automatic, from the resolution:
    /// about 12000 at 1080p, 34000 at 4K]
    #[arg(short, long)]
    bitrate: Option<u32>,

    /// Frame rate to ask the compositor for (0 = let it decide)
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Capture frames as GPU buffers (DMA-BUF) and convert on the GPU.
    /// Experimental: on KDE + NVIDIA it measured ~20 fps vs ~55 fps default.
    #[arg(long)]
    dmabuf: bool,

    /// Experimental: resize the virtual monitor to match the viewing device
    /// and follow its rotation. Off by default: on KDE + NVIDIA the resizes
    /// and capture reconnects hurt frame rate badly.
    #[arg(long)]
    fit_device: bool,

    /// Stop this long after the last device disconnects (seconds; 0 = never).
    /// The countdown only starts once a device has connected and left.
    #[arg(long, default_value_t = 60)]
    idle_timeout: u64,

    /// Don't show a system tray icon
    #[arg(long)]
    no_tray: bool,

    /// Debug: only capture and count frames (no encoding or streaming)
    #[arg(long)]
    capture_only: bool,

    /// Debug: stream an existing PipeWire node instead of asking the portal
    #[arg(long, hide = true)]
    pipewire_node: Option<u32>,
}

/// The address other devices on the LAN can reach us at. Connecting a UDP
/// socket sends nothing; it just makes the OS pick the outgoing interface.
fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let args = Args::parse();
    gst::init()?;

    // Claim the port before anything else. If it's taken, the app is almost
    // certainly already running (e.g. launched twice from the app menu).
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), args.port);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && !args.capture_only => {
            tracing::warn!("port {} is in use; is Linaux Screen already running?", args.port);
            desktop::notify(
                "Already running",
                "Linaux Screen is already running. Use its tray icon to restart or stop it.",
            )
            .await;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    // Debug: LINAUX_ENCODER replaces the encoder element and its settings,
    // e.g. LINAUX_ENCODER="nvh264enc" to try driver defaults.
    let custom = std::env::var("LINAUX_ENCODER").ok().filter(|c| !c.trim().is_empty());
    let encoder_choice = stream::EncoderChoice { kind: args.encoder, bitrate_kbps: args.bitrate, custom };
    // Check an encoder is available now rather than when a device connects.
    let (encoder_name, _, _) = encoder_choice.viewer_pipeline(1920, 1080)?;
    tracing::info!("using encoder {encoder_name}");

    // Held until exit: dropping it ends the screencast and removes the
    // virtual monitor.
    let resize = args.source == SourceKind::Virtual && args.pipewire_node.is_none() && args.fit_device;
    // Note the existing outputs, so we can tell which one the portal adds.
    let outputs_before = if resize {
        match display::output_names().await {
            Ok(names) => Some(names),
            Err(e) => {
                tracing::warn!("can't resize the virtual monitor to fit devices: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let capture = match (args.pipewire_node, args.source) {
        (Some(_), _) | (None, SourceKind::Test) => None,
        (None, kind) => Some(capture::start(kind).await?),
    };
    let virtual_display = match outputs_before {
        Some(before) => match display::VirtualDisplay::find_new(&before).await {
            Ok(d) => Some(std::sync::Arc::new(d)),
            Err(e) => {
                tracing::warn!("can't resize the virtual monitor to fit devices: {e:#}");
                None
            }
        },
        None => None,
    };
    let source = match (&capture, args.pipewire_node) {
        (Some(c), _) => stream::Source::PipeWire { fd: Some(c.pipewire_fd()), node_id: c.node_id },
        (None, Some(node_id)) => stream::Source::PipeWire { fd: None, node_id },
        (None, None) => stream::Source::Test,
    };
    if args.capture_only {
        stream::run_capture_only(&source, args.fps, args.dmabuf).await?;
        drop(capture);
        return Ok(());
    }
    let hub = std::sync::Arc::new(stream::CaptureHub::start(&source, args.fps, args.dmabuf)?);

    let ip = lan_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "<this-pc-ip>".into());
    let address = format!("http://{ip}:{}", args.port);
    println!("\n  Open on your phone or tablet:  {address}\n");

    let control = Arc::new(control::Control::new(address.clone()));
    if !args.no_tray {
        tokio::spawn(tray::run(control.clone()));
    }
    if args.idle_timeout > 0 {
        let c = control.clone();
        let timeout = Duration::from_secs(args.idle_timeout);
        tokio::spawn(async move { control::run_auto_stop(&c, timeout).await });
    }
    desktop::notify("Second screen ready", &format!("Open {address} on your phone or tablet")).await;

    let mut exit_rx = control.exit.subscribe();
    let why = tokio::select! {
        result = server::run(listener, control.clone(), encoder_choice, hub.clone(), virtual_display) => {
            result?;
            Exit::Stop
        }
        _ = tokio::signal::ctrl_c() => Exit::Stop,
        _ = exit_rx.wait_for(|e| e.is_some()) => control.exit.borrow().unwrap_or(Exit::Stop),
    };
    tracing::info!("shutting down ({why:?})");
    // Let connected viewers know (so they don't keep retrying), then give the
    // messages a moment to go out before the server disappears.
    control.request_exit(why);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Dropping these ends the screencast, which removes the virtual monitor.
    drop(hub);
    drop(capture);

    match why {
        Exit::Restart => restart(),
        Exit::Idle => {
            desktop::notify("Second screen stopped", "No device was connected for a while.").await;
        }
        Exit::Stop => {}
    }
    Ok(())
}

/// Replaces this process with a fresh copy of the app, same arguments.
/// Picks up a newly installed binary too.
fn restart() {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()
        .map(|p| {
            // After an update the running binary shows as "... (deleted)".
            let s = p.to_string_lossy().trim_end_matches(" (deleted)").to_owned();
            std::path::PathBuf::from(s)
        })
        .unwrap_or_else(|_| "linaux-screen".into());
    tracing::info!("restarting");
    let err = std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec();
    tracing::error!("couldn't restart: {err}");
}

/// Logs to the terminal and to `~/.local/state/linaux-screen/linaux-screen.log`
/// (replaced on each start), so there's a log even when started from the app
/// menu.
fn init_logging() {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "linaux_screen=info".into());
    let file = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .map(|d| d.join("linaux-screen"))
        .and_then(|dir| {
            std::fs::create_dir_all(&dir).ok()?;
            std::fs::File::create(dir.join("linaux-screen.log")).ok()
        });
    let file_layer = file.map(|f| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(file_layer)
        .init();
}
