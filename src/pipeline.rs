//! Building GStreamer pipeline descriptions: capture, encoder and per-viewer
//! WebRTC, plus small logging helpers shared by the pipelines.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use gst::prelude::*;

use crate::EncoderKind;

/// Where the frames come from.
pub enum Source {
    /// A portal screencast: `fd` is its PipeWire remote, `node_id` the stream.
    PipeWire { fd: i32, node_id: u32 },
    /// Built-in moving test pattern (no screen capture).
    Test,
}

/// Builds `name prop=value …`, silently dropping properties the installed
/// element doesn't have or enum values it doesn't know. Encoder properties
/// vary a lot between GStreamer versions, and one unknown property would
/// otherwise make the whole pipeline fail to parse.
pub(crate) fn element_desc(factory: &str, props: &[(&str, &str)]) -> Option<String> {
    let element = gst::ElementFactory::make(factory).build().ok()?;
    let mut desc = factory.to_owned();
    for (name, value) in props {
        let Some(pspec) = element.find_property(name) else {
            tracing::debug!("{factory}: no property {name}, skipping");
            continue;
        };
        if let Some(enum_spec) = pspec.downcast_ref::<gst::glib::ParamSpecEnum>() {
            let class = enum_spec.enum_class();
            if class.value_by_nick(value).is_none() && class.value_by_name(value).is_none() {
                tracing::debug!("{factory}: {name} has no value {value}, skipping");
                continue;
            }
        }
        desc.push_str(&format!(" {name}={value}"));
    }
    Some(desc)
}

/// Picks an H.264 encoder and returns `(name, pipeline fragment)`.
fn encoder(kind: EncoderKind, bitrate_kbps: u32) -> Result<(String, String)> {
    let br = bitrate_kbps.to_string();
    let candidates: &[&str] = match kind {
        EncoderKind::Auto => &["vah264enc", "nvh264enc", "x264enc"],
        EncoderKind::Vaapi => &["vah264enc"],
        EncoderKind::Nvenc => &["nvh264enc"],
        EncoderKind::X264 => &["x264enc"],
    };

    for &name in candidates {
        let desc = match name {
            "vah264enc" => element_desc(
                name,
                &[
                    ("rate-control", "cbr"),
                    ("bitrate", &br),
                    ("key-int-max", "60"),
                    ("b-frames", "0"),
                    // 4 = balanced; 7 (fastest) looked grainy at high
                    // resolutions for barely any latency gain.
                    ("target-usage", "4"),
                ],
            ),
            "nvh264enc" => element_desc(
                name,
                &[
                    // p4 = balanced quality; p1 (fastest) looked grainy at
                    // 4K. Latency stays low thanks to the tune/zerolatency
                    // settings and no B-frames.
                    ("preset", "p4"),
                    ("tune", "ultra-low-latency"),
                    ("rc-mode", "cbr"),
                    ("bitrate", &br),
                    ("gop-size", "60"),
                    ("bframes", "0"),
                    ("zerolatency", "true"),
                ],
            ),
            "x264enc" => element_desc(
                name,
                &[
                    ("tune", "zerolatency"),
                    ("speed-preset", "ultrafast"),
                    ("bitrate", &br),
                    ("key-int-max", "60"),
                    ("bframes", "0"),
                ],
            )
            // Software H.264 is safest with constrained-baseline, which every
            // browser decodes.
            .map(|d| format!("{d} ! video/x-h264,profile=constrained-baseline")),
            _ => None,
        };
        if let Some(desc) = desc {
            // Named so the bitrate can be changed while streaming.
            let desc = desc.replacen(name, &format!("{name} name=enc"), 1);
            return Ok((name.to_owned(), desc));
        }
        tracing::debug!("encoder {name} not available");
    }
    Err(anyhow!(
        "no usable H.264 encoder found (tried {candidates:?}). \
         Install gstreamer1-plugins-bad (VA-API/NVENC) or gstreamer1-plugins-ugly (x264)."
    ))
}

/// A bitrate that suits the frame size: about 12 Mbit/s at 1080p, rising
/// more slowly than the pixel count (about 34 Mbit/s at 4K), within limits a
/// home Wi-Fi network handles.
pub fn auto_bitrate_kbps(width: u32, height: u32) -> u32 {
    let pixels = (width as f64 * height as f64).max(1.0);
    let ratio = pixels / (1920.0 * 1080.0);
    ((12_000.0 * ratio.powf(0.75)) as u32).clamp(4_000, 50_000)
}

/// Which encoder to use and how; turned into a pipeline per viewer, because
/// the bitrate depends on the frame size at the time.
pub struct EncoderChoice {
    pub kind: EncoderKind,
    /// Fixed bitrate, or `None` for automatic.
    pub bitrate_kbps: Option<u32>,
}

impl EncoderChoice {
    /// Returns `(encoder name, viewer pipeline description, bitrate)`.
    pub fn viewer_pipeline(&self, width: u32, height: u32) -> Result<(String, String, u32)> {
        let kbps = self
            .bitrate_kbps
            .unwrap_or_else(|| auto_bitrate_kbps(width, height));
        let (name, desc) = encoder(self.kind, kbps)?;
        Ok((name, viewer_desc(&desc), kbps))
    }
}

/// The raw audio format passed from the audio capture to viewers.
pub const AUDIO_CAPS: &str = "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2";

/// Pipeline description for one viewer: takes NV12 frames from the capture
/// hub via `appsrc` named `src`, encodes and sends them over WebRTC. Audio
/// comes in through `asrc`; it only receives data while this device is the
/// chosen audio output.
fn viewer_desc(encoder: &str) -> String {
    let opus = element_desc(
        "opusenc",
        &[
            ("bitrate", "128000"),
            ("frame-size", "10"),
            ("audio-type", "restricted-lowdelay"),
        ],
    )
    .unwrap_or_else(|| "opusenc".into());
    format!(
        "appsrc name=src is-live=true format=time do-timestamp=true \
         ! queue max-size-buffers=1 leaky=downstream \
         ! {encoder} \
         ! h264parse name=parse \
         ! rtph264pay name=pay config-interval=-1 aggregate-mode=zero-latency \
         ! application/x-rtp,media=video,encoding-name=H264,payload=96 \
         ! webrtcbin name=webrtc bundle-policy=max-bundle \
         appsrc name=asrc is-live=true format=time caps={AUDIO_CAPS} \
         ! queue max-size-time=100000000 leaky=downstream \
         ! audioconvert ! {opus} \
         ! rtpopuspay pt=97 \
         ! application/x-rtp,media=audio,encoding-name=OPUS,payload=97,clock-rate=48000 \
         ! webrtc."
    )
}

/// Returns `(source, convert)`: `source` ends in raw frames from an element
/// named `capture`; `convert` turns them into NV12 in system memory.
pub(crate) fn capture_desc(source: &Source, fps: u32, dmabuf: bool) -> (String, String) {
    let videoconvert = element_desc("videoconvert", &[("n-threads", "4")])
        .unwrap_or_else(|| "videoconvert".into());

    match source {
        Source::PipeWire { fd, node_id } => {
            let (fd, node) = (fd.to_string(), node_id.to_string());
            let mut props = vec![
                ("fd", fd.as_str()),
                ("path", node.as_str()),
                ("do-timestamp", "true"),
            ];
            // Deliberately no `keepalive-time`: with it set, KWin's screencast
            // dropped to ~1 fps. The compositor sends frames only when the
            // screen changes, and a new viewer gets the last frame anyway.
            if !dmabuf {
                // Copy each frame out of the compositor's buffer straight
                // away, so we never hold on to its small buffer pool.
                props.push(("always-copy", "true"));
            }
            let pw = element_desc("pipewiresrc", &props)
                .unwrap_or_else(|| format!("pipewiresrc fd={fd} path={node}"));

            // Screen capture lets us ask for a maximum frame rate. Left
            // open, GStreamer settles on the lowest the compositor offers
            // (about 1 fps). A list is tried in order, so the requested rate
            // wins and the others are fallbacks for odd refresh rates.
            let rate = if fps == 0 {
                String::new()
            } else {
                let mut rates = vec![fps, 60, 59, 50, 30];
                rates.dedup();
                let list: Vec<String> = rates.iter().map(|r| format!("{r}/1")).collect();
                format!(",max-framerate=(fraction){{ {} }}", list.join(", "))
            };

            // Keep downstream elements (encoder, GL upload) from negotiating
            // the capture's buffer pool, so their pool sizes don't limit how
            // many buffers the compositor can render into.
            let isolate = element_desc("identity", &[("drop-allocation", "true")])
                .map(|i| format!(" ! {i}"))
                .unwrap_or_default();

            if dmabuf {
                // Take frames as GPU buffers (DMA-BUF) and convert colours on
                // the GPU with OpenGL, instead of the compositor copying them
                // into system memory and converting on the CPU.
                (
                    format!("{pw} name=capture ! video/x-raw(memory:DMABuf){rate}{isolate}"),
                    "glupload ! glcolorconvert ! video/x-raw(memory:GLMemory),format=NV12 ! gldownload"
                        .to_owned(),
                )
            } else {
                (
                    format!("{pw} name=capture ! video/x-raw{rate}{isolate}"),
                    videoconvert,
                )
            }
        }
        Source::Test => {
            // `testcaps` is named so resize requests can change it, which
            // exercises the same size-change path as a real monitor resize.
            let base = "videotestsrc name=capture is-live=true pattern=ball \
                 ! capsfilter name=testcaps caps=video/x-raw,width=1280,height=720,framerate=60/1";
            // A running clock makes latency easy to eyeball, if pango is installed.
            let src = match element_desc("timeoverlay", &[]) {
                Some(overlay) => format!("{base} ! {overlay} font-desc=\"Sans 40\""),
                None => base.to_owned(),
            };
            (src, videoconvert)
        }
    }
}

/// Parses `desc` into a pipeline.
pub(crate) fn parse(desc: &str) -> Result<gst::Pipeline> {
    gst::parse::launch(desc)
        .with_context(|| format!("could not build pipeline: {desc}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("pipeline description did not produce a pipeline"))
}

/// Diagnostic mode (`--capture-only`): capture and count frames, nothing
/// else. Shows whether the compositor itself is delivering frames slowly.
pub async fn run_capture_only(source: &Source, fps: u32, dmabuf: bool) -> Result<()> {
    let (src, _) = capture_desc(source, fps, dmabuf);
    let pipeline = parse(&format!("{src} ! fakesink sync=false"))?;
    log_capture_caps(&pipeline);
    spawn_fps_logger(&pipeline, "capture", "capture");
    pipeline.set_state(gst::State::Playing)?;
    println!("\n  Capture-only mode: move windows on the captured screen, Ctrl+C to stop.\n");
    let bus = pipeline.bus().context("no bus")?;
    let mut messages = bus.stream();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            msg = messages.next() => match msg {
                Some(msg) => if let gst::MessageView::Error(e) = msg.view() {
                    tracing::error!("{} ({:?})", e.error(), e.debug());
                    break;
                },
                None => break,
            }
        }
    }
    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

/// Logs, every 5 seconds, how many buffers per second leave the element
/// called `element` in `pipeline`.
pub(crate) fn spawn_fps_logger(pipeline: &gst::Pipeline, element: &str, label: &'static str) {
    let Some(pad) = pipeline.by_name(element).and_then(|e| e.static_pad("src")) else {
        return;
    };
    let counter = Arc::new(AtomicU64::new(0));
    let c = counter.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        c.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    let weak = pipeline.downgrade();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.tick().await;
        loop {
            tick.tick().await;
            if weak.upgrade().is_none() {
                break;
            }
            let fps = counter.swap(0, Ordering::Relaxed) as f64 / 5.0;
            tracing::info!("{label} {fps:.1} fps");
        }
    });
}

/// Logs the capture format whenever it's (re)negotiated.
pub(crate) fn log_capture_caps(pipeline: &gst::Pipeline) {
    if let Some(pad) = pipeline
        .by_name("capture")
        .and_then(|c| c.static_pad("src"))
    {
        pad.connect_notify(Some("caps"), |pad, _| {
            if let Some(caps) = pad.current_caps() {
                tracing::info!("capture format: {caps}");
            }
        });
    }
}

/// Logs pipeline errors and warnings until the pipeline stops or is dropped.
pub(crate) fn spawn_bus_logger(pipeline: &gst::Pipeline, what: &'static str) -> Result<()> {
    let bus = pipeline.bus().context("pipeline has no bus")?;
    let weak = pipeline.downgrade();
    tokio::spawn(async move {
        let mut messages = bus.stream();
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                msg = messages.next() => {
                    let Some(msg) = msg else { break };
                    match msg.view() {
                        gst::MessageView::Error(err) => {
                            tracing::error!(
                                "{what} pipeline error from {:?}: {} ({:?})",
                                err.src().map(|s| s.path_string()),
                                err.error(),
                                err.debug()
                            );
                            break;
                        }
                        gst::MessageView::Warning(w) => {
                            tracing::warn!("{what} pipeline warning: {} ({:?})", w.error(), w.debug());
                        }
                        gst::MessageView::Eos(_) => break,
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    // Stop once the pipeline has been replaced or dropped.
                    match weak.upgrade() {
                        Some(p) if p.current_state() != gst::State::Null => {}
                        _ => break,
                    }
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::auto_bitrate_kbps;

    #[test]
    fn auto_bitrate_scales_with_size() {
        assert_eq!(auto_bitrate_kbps(1920, 1080), 12_000);
        let uhd = auto_bitrate_kbps(3840, 2160);
        assert!((33_000..36_000).contains(&uhd), "{uhd}");
        let phone = auto_bitrate_kbps(3120, 1440);
        assert!(phone > 12_000 && phone < uhd, "{phone}");
        assert_eq!(auto_bitrate_kbps(320, 240), 4_000);
        assert_eq!(auto_bitrate_kbps(7680, 4320), 50_000);
    }
}
