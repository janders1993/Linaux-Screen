mod capture;
mod server;
mod stream;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use anyhow::Result;
use clap::{Parser, ValueEnum};

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

    /// Video bitrate in kbit/s
    #[arg(short, long, default_value_t = 8000)]
    bitrate: u32,

    /// Frame rate to ask the compositor for (0 = let it decide)
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Capture frames as GPU buffers (DMA-BUF) and convert on the GPU.
    /// Experimental: on KDE + NVIDIA it measured ~20 fps vs ~55 fps default.
    #[arg(long)]
    dmabuf: bool,

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linaux_screen=info".into()),
        )
        .init();

    let args = Args::parse();
    gst::init()?;

    let (mut encoder_name, mut encoder_desc) = stream::encoder(args.encoder, args.bitrate)?;
    // Debug: LINAUX_ENCODER replaces the encoder element and its settings,
    // e.g. LINAUX_ENCODER="nvh264enc" to try driver defaults.
    if let Ok(custom) = std::env::var("LINAUX_ENCODER") {
        if !custom.trim().is_empty() {
            encoder_name = custom.split_whitespace().next().unwrap_or_default().to_owned();
            encoder_desc = custom;
        }
    }
    tracing::info!("using encoder {encoder_name}");

    // Held until exit: dropping it ends the screencast and removes the
    // virtual monitor.
    let capture = match (args.pipewire_node, args.source) {
        (Some(_), _) | (None, SourceKind::Test) => None,
        (None, kind) => Some(capture::start(kind).await?),
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
    let desc = stream::viewer_desc(&encoder_desc);
    tracing::debug!("viewer pipeline: {desc}");

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), args.port);
    let ip = lan_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "<this-pc-ip>".into());
    println!("\n  Open on your phone or tablet:  http://{ip}:{}\n", args.port);

    tokio::select! {
        result = server::run(addr, desc, encoder_name, hub.clone()) => result?,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }

    drop(hub);
    drop(capture);
    Ok(())
}
