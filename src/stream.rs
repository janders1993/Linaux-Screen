//! GStreamer pipeline: capture → H.264 encode → WebRTC.
//!
//! Each connected viewer gets its own pipeline and `webrtcbin`. The server is
//! always the offerer; the browser answers.

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use gst::prelude::*;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::EncoderKind;

/// Messages exchanged with the browser over the WebSocket.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Signal {
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
    },
    /// Server-side status for the viewer's overlay.
    Info {
        encoder: String,
    },
    /// The viewer's screen in physical pixels, sent on connect and rotation.
    Display {
        width: u32,
        height: u32,
        dpr: f64,
    },
}

/// Where the frames come from.
pub enum Source {
    /// `fd` is the portal's PipeWire remote; `None` connects to the default
    /// PipeWire daemon (used by `--pipewire-node` for debugging).
    PipeWire { fd: Option<i32>, node_id: u32 },
    Test,
}

/// Builds `name prop=value …`, silently dropping properties the installed
/// element doesn't have or enum values it doesn't know. Encoder properties
/// vary a lot between GStreamer versions, and one unknown property would
/// otherwise make the whole pipeline fail to parse.
fn element_desc(factory: &str, props: &[(&str, &str)]) -> Option<String> {
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
pub fn encoder(kind: EncoderKind, bitrate_kbps: u32) -> Result<(String, String)> {
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
                    ("target-usage", "7"),
                ],
            ),
            "nvh264enc" => element_desc(
                name,
                &[
                    ("preset", "p1"),
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
            return Ok((name.to_owned(), desc));
        }
        tracing::debug!("encoder {name} not available");
    }
    Err(anyhow!(
        "no usable H.264 encoder found (tried {candidates:?}). \
         Install gstreamer1-plugins-bad (VA-API/NVENC) or gstreamer1-plugins-ugly (x264)."
    ))
}

/// Pipeline description for one viewer: takes NV12 frames from the capture
/// hub via `appsrc`, encodes and sends them over WebRTC.
pub fn viewer_desc(encoder: &str) -> String {
    format!(
        "appsrc name=src is-live=true format=time do-timestamp=true \
         ! queue max-size-buffers=1 leaky=downstream \
         ! {encoder} \
         ! h264parse name=parse \
         ! rtph264pay name=pay config-interval=-1 aggregate-mode=zero-latency \
         ! application/x-rtp,media=video,encoding-name=H264,payload=96 \
         ! webrtcbin name=webrtc bundle-policy=max-bundle"
    )
}

/// Returns `(source, convert)`: `source` ends in raw frames named `capture`;
/// `convert` turns them into NV12 in system memory.
pub fn capture_desc(source: &Source, fps: u32, dmabuf: bool) -> (String, String) {
    let videoconvert = element_desc("videoconvert", &[("n-threads", "4")])
        .unwrap_or_else(|| "videoconvert".into());

    match source {
        Source::PipeWire { fd, node_id } => {
            let fd_str = fd.map(|f| f.to_string());
            let mut props: Vec<(&str, &str)> = Vec::new();
            if let Some(f) = &fd_str {
                props.push(("fd", f));
            }
            let node = node_id.to_string();
            props.extend_from_slice(&[
                ("path", node.as_str()),
                ("do-timestamp", "true"),
            ]);
            // Deliberately no `keepalive-time`: with it set, KWin's screencast
            // dropped to ~1 fps. The compositor sends frames only when the
            // screen changes, and a new viewer gets an initial frame anyway.
            if !dmabuf {
                // Copy each frame out of the compositor's buffer straight
                // away, so we never hold on to its small buffer pool.
                props.push(("always-copy", "true"));
            }
            let pw = element_desc("pipewiresrc", &props)
                .unwrap_or_else(|| format!("pipewiresrc path={node_id}"));
            // Debug escape hatch: extra pipewiresrc properties, e.g.
            // LINAUX_PW_PROPS="always-copy=false keepalive-time=0".
            let pw = match std::env::var("LINAUX_PW_PROPS") {
                Ok(extra) if !extra.trim().is_empty() => format!("{pw} {extra}"),
                _ => pw,
            };

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

            // Stop downstream elements (encoder, GL upload) from negotiating
            // the capture's buffer pool. Their small pool sizes get passed on
            // to the compositor, which then waits for a free buffer and only
            // manages a frame or two per second.
            let isolate = element_desc("identity", &[("drop-allocation", "true")])
                .map(|i| format!(" ! {i}"))
                .unwrap_or_default();

            if dmabuf {
                // Take frames as GPU buffers (DMA-BUF) instead of having the
                // compositor copy them into system memory, which is very slow
                // on some drivers (notably NVIDIA). Colour conversion then
                // happens on the GPU via OpenGL.
                (
                    format!("{pw} name=capture ! video/x-raw(memory:DMABuf){rate}{isolate}"),
                    "glupload ! glcolorconvert ! video/x-raw(memory:GLMemory),format=NV12 ! gldownload"
                        .to_owned(),
                )
            } else {
                (format!("{pw} name=capture ! video/x-raw{rate}{isolate}"), videoconvert)
            }
        }
        Source::Test => {
            // Named so resize requests can change it, which exercises the
            // same renegotiation path as the real virtual monitor resizing.
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

/// Debug mode: capture and count frames, nothing else. Shows whether the
/// compositor itself is delivering frames slowly.
pub async fn run_capture_only(source: &Source, fps: u32, dmabuf: bool) -> Result<()> {
    let (src, _) = capture_desc(source, fps, dmabuf);
    // Debug: LINAUX_CAPTURE_TAIL replaces the fakesink, to add pipeline
    // stages one at a time and find which one slows capture down.
    let tail = std::env::var("LINAUX_CAPTURE_TAIL").unwrap_or_else(|_| "fakesink sync=false".into());
    let desc = format!("{src} ! {tail}");
    tracing::info!("capture-only pipeline: {desc}");
    let pipeline = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
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
fn spawn_fps_logger(pipeline: &gst::Pipeline, element: &str, label: &'static str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

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
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
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

fn log_capture_caps(pipeline: &gst::Pipeline) {
    if let Some(pad) = pipeline.by_name("capture").and_then(|c| c.static_pad("src")) {
        pad.connect_notify(Some("caps"), |pad, _| {
            if let Some(caps) = pad.current_caps() {
                tracing::info!("capture format: {caps}");
            }
        });
    }
}

fn spawn_bus_logger(pipeline: &gst::Pipeline, what: &'static str) -> Result<()> {
    let bus = pipeline.bus().context("pipeline has no bus")?;
    tokio::spawn(async move {
        let mut messages = bus.stream();
        while let Some(msg) = messages.next().await {
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
    });
    Ok(())
}

/// The screen capture, running for the whole life of the program in its own
/// pipeline. Each viewer subscribes and gets every frame pushed into its own
/// encoding pipeline.
///
/// Keeping capture separate matters: with capture and WebRTC in one pipeline
/// started when a phone connects, KWin delivered ~1 fps, while the same
/// capture on its own ran at 30-75 fps. It also means a viewer joining while
/// the screen is idle immediately gets the last frame.
pub struct CaptureHub {
    pipeline: gst::Pipeline,
    shared: std::sync::Arc<HubShared>,
}

#[derive(Default)]
struct HubShared {
    viewers: std::sync::Mutex<Vec<(u64, gst_app::AppSrc)>>,
    last: std::sync::Mutex<Option<gst::Sample>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl HubShared {
    fn push(appsrc: &gst_app::AppSrc, sample: &gst::Sample) {
        let (Some(buffer), Some(caps)) = (sample.buffer(), sample.caps()) else { return };
        let caps = caps.to_owned();
        if appsrc.caps().as_ref() != Some(&caps) {
            appsrc.set_caps(Some(&caps));
        }
        // Each viewer stamps frames with its own clock (do-timestamp), so
        // drop the capture pipeline's timestamps.
        let mut buffer = buffer.copy();
        {
            let b = buffer.make_mut();
            b.set_pts(gst::ClockTime::NONE);
            b.set_dts(gst::ClockTime::NONE);
            b.set_duration(gst::ClockTime::NONE);
        }
        let _ = appsrc.push_buffer(buffer);
    }
}

impl CaptureHub {
    pub fn start(source: &Source, fps: u32, dmabuf: bool) -> Result<Self> {
        let (src, convert) = capture_desc(source, fps, dmabuf);
        let desc = format!(
            "{src} \
             ! queue max-size-buffers=1 leaky=downstream \
             ! {convert} ! video/x-raw,format=NV12 \
             ! appsink name=sink sync=false max-buffers=1 drop=true"
        );
        tracing::debug!("capture pipeline: {desc}");
        let pipeline = gst::parse::launch(&desc)
            .with_context(|| format!("could not build capture pipeline: {desc}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("capture description did not produce a pipeline"))?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .context("appsink missing from capture pipeline")?;

        let shared = std::sync::Arc::new(HubShared::default());
        let cb_shared = shared.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    for (_, appsrc) in cb_shared.viewers.lock().unwrap().iter() {
                        HubShared::push(appsrc, &sample);
                    }
                    *cb_shared.last.lock().unwrap() = Some(sample);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        log_capture_caps(&pipeline);
        spawn_fps_logger(&pipeline, "capture", "capture");
        spawn_bus_logger(&pipeline, "capture")?;
        pipeline
            .set_state(gst::State::Playing)
            .context("capture pipeline refused to start — run with RUST_LOG=debug GST_DEBUG=3")?;
        Ok(Self { pipeline, shared })
    }

    /// Start feeding frames to `appsrc`; returns an id for `unsubscribe`.
    fn subscribe(&self, appsrc: gst_app::AppSrc) -> u64 {
        let id = self
            .shared
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(sample) = self.shared.last.lock().unwrap().as_ref() {
            HubShared::push(&appsrc, sample);
        }
        self.shared.viewers.lock().unwrap().push((id, appsrc));
        id
    }

    /// Test pattern only: change the frame size, as if the monitor resized.
    /// Returns false if this isn't the test source.
    pub fn resize_test_pattern(&self, width: u32, height: u32) -> bool {
        let Some(filter) = self.pipeline.by_name("testcaps") else { return false };
        let caps = gst::Caps::builder("video/x-raw")
            .field("width", width as i32)
            .field("height", height as i32)
            .field("framerate", gst::Fraction::new(60, 1))
            .build();
        filter.set_property("caps", caps);
        true
    }

    fn unsubscribe(&self, id: u64) {
        self.shared.viewers.lock().unwrap().retain(|(i, _)| *i != id);
    }
}

impl Drop for CaptureHub {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// One viewer's pipeline. Dropping it stops the stream.
pub struct Session {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    hub: std::sync::Arc<CaptureHub>,
    subscription: u64,
}

impl Session {
    pub fn start(
        desc: &str,
        tx: UnboundedSender<Signal>,
        hub: std::sync::Arc<CaptureHub>,
    ) -> Result<Self> {
        let pipeline = gst::parse::launch(desc)
            .with_context(|| format!("could not build pipeline: {desc}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("pipeline description did not produce a pipeline"))?;
        let webrtc = pipeline
            .by_name("webrtc")
            .context("webrtcbin missing from pipeline")?;

        let appsrc = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .context("appsrc missing from viewer pipeline")?;
        spawn_fps_logger(&pipeline, "parse", "encoded");

        // We only send video; tell the browser not to bother sending any.
        if let Some(transceiver) =
            webrtc.emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>("get-transceiver", &[&0i32])
        {
            transceiver.set_property(
                "direction",
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
            );
        }

        let offer_tx = tx.clone();
        webrtc.connect("on-negotiation-needed", false, move |values| {
            let webrtc = values[0].get::<gst::Element>().expect("webrtcbin");
            let tx = offer_tx.clone();
            let webrtc_weak = webrtc.downgrade();
            let promise = gst::Promise::with_change_func(move |reply| {
                let Some(webrtc) = webrtc_weak.upgrade() else { return };
                let offer = match reply {
                    Ok(Some(reply)) => reply
                        .value("offer")
                        .ok()
                        .and_then(|v| v.get::<gst_webrtc::WebRTCSessionDescription>().ok()),
                    _ => None,
                };
                let Some(offer) = offer else {
                    tracing::error!("webrtcbin failed to create an offer: {reply:?}");
                    return;
                };
                webrtc.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
                match offer.sdp().as_text() {
                    Ok(sdp) => {
                        let _ = tx.send(Signal::Offer { sdp });
                    }
                    Err(e) => tracing::error!("could not serialise offer: {e}"),
                }
            });
            webrtc.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
            None
        });

        let ice_tx = tx;
        webrtc.connect("on-ice-candidate", false, move |values| {
            let sdp_mline_index = values[1].get::<u32>().expect("mline index");
            let candidate = values[2].get::<String>().expect("candidate");
            let _ = ice_tx.send(Signal::Ice { candidate, sdp_mline_index });
            None
        });

        webrtc.connect_notify(Some("connection-state"), |webrtc, _| {
            let state = webrtc.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
            tracing::info!("viewer connection: {state:?}");
        });

        spawn_bus_logger(&pipeline, "viewer")?;

        pipeline
            .set_state(gst::State::Playing)
            .context("pipeline refused to start — run with RUST_LOG=debug GST_DEBUG=3 for details")?;

        let subscription = hub.subscribe(appsrc);
        Ok(Self { pipeline, webrtc, hub, subscription })
    }

    pub fn handle(&self, signal: Signal) -> Result<()> {
        match signal {
            Signal::Answer { sdp } => {
                let sdp = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                    .map_err(|_| anyhow!("viewer sent an invalid SDP answer"))?;
                let answer =
                    gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, sdp);
                self.webrtc
                    .emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
            }
            Signal::Ice { candidate, sdp_mline_index } => {
                if !candidate.is_empty() {
                    self.webrtc
                        .emit_by_name::<()>("add-ice-candidate", &[&sdp_mline_index, &candidate]);
                }
            }
            other => tracing::warn!("unexpected message from viewer: {other:?}"),
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.hub.unsubscribe(self.subscription);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
