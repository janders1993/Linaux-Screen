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

    let (encoder_name, encoder_desc) = stream::encoder(args.encoder, args.bitrate)?;
    tracing::info!("using encoder {encoder_name}");

    // Held until exit: dropping it ends the screencast and removes the
    // virtual monitor.
    let capture = match args.source {
        SourceKind::Test => None,
        kind => Some(capture::start(kind).await?),
    };
    let source = match &capture {
        Some(c) => stream::Source::PipeWire { fd: c.pipewire_fd(), node_id: c.node_id },
        None => stream::Source::Test,
    };
    let desc = stream::pipeline_desc(&source, &encoder_desc);
    tracing::debug!("pipeline: {desc}");

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), args.port);
    let ip = lan_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "<this-pc-ip>".into());
    println!("\n  Open on your phone or tablet:  http://{ip}:{}\n", args.port);

    tokio::select! {
        result = server::run(addr, desc, encoder_name) => result?,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }

    drop(capture);
    Ok(())
}
