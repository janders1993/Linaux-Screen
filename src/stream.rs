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
}

/// Where the frames come from.
pub enum Source {
    PipeWire { fd: i32, node_id: u32 },
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

/// Full pipeline description for one viewer.
pub fn pipeline_desc(source: &Source, encoder: &str) -> String {
    let src = match source {
        Source::PipeWire { fd, node_id } => {
            let pw = element_desc(
                "pipewiresrc",
                &[
                    ("fd", &fd.to_string()),
                    ("path", &node_id.to_string()),
                    ("do-timestamp", "true"),
                    // Re-send the last frame when the desktop is idle, so a
                    // static screen doesn't stall the stream.
                    ("keepalive-time", "1000"),
                ],
            )
            .unwrap_or_else(|| format!("pipewiresrc fd={fd} path={node_id}"));
            format!("{pw} ! video/x-raw")
        }
        Source::Test => {
            let base = "videotestsrc is-live=true pattern=ball \
                 ! video/x-raw,width=1280,height=720,framerate=60/1";
            // A running clock makes latency easy to eyeball, if pango is installed.
            match element_desc("timeoverlay", &[]) {
                Some(overlay) => format!("{base} ! {overlay} font-desc=\"Sans 40\""),
                None => base.to_owned(),
            }
        }
    };
    let convert = element_desc("videoconvert", &[("n-threads", "4")])
        .unwrap_or_else(|| "videoconvert".into());

    format!(
        "{src} \
         ! queue max-size-buffers=1 leaky=downstream \
         ! {convert} ! video/x-raw,format=NV12 \
         ! {encoder} \
         ! h264parse \
         ! rtph264pay config-interval=-1 aggregate-mode=zero-latency \
         ! application/x-rtp,media=video,encoding-name=H264,payload=96 \
         ! webrtcbin name=webrtc bundle-policy=max-bundle"
    )
}

/// One viewer's pipeline. Dropping it stops the stream.
pub struct Session {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
}

impl Session {
    pub fn start(desc: &str, tx: UnboundedSender<Signal>) -> Result<Self> {
        let pipeline = gst::parse::launch(desc)
            .with_context(|| format!("could not build pipeline: {desc}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("pipeline description did not produce a pipeline"))?;
        let webrtc = pipeline
            .by_name("webrtc")
            .context("webrtcbin missing from pipeline")?;

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

        // Log pipeline errors (e.g. encoder negotiation failures) instead of
        // failing silently.
        let bus = pipeline.bus().context("pipeline has no bus")?;
        tokio::spawn(async move {
            let mut messages = bus.stream();
            while let Some(msg) = messages.next().await {
                match msg.view() {
                    gst::MessageView::Error(err) => {
                        tracing::error!(
                            "pipeline error from {:?}: {} ({:?})",
                            err.src().map(|s| s.path_string()),
                            err.error(),
                            err.debug()
                        );
                        break;
                    }
                    gst::MessageView::Warning(w) => {
                        tracing::warn!("pipeline warning: {} ({:?})", w.error(), w.debug());
                    }
                    gst::MessageView::Eos(_) => break,
                    _ => {}
                }
            }
        });

        pipeline
            .set_state(gst::State::Playing)
            .context("pipeline refused to start — run with RUST_LOG=debug GST_DEBUG=3 for details")?;

        Ok(Self { pipeline, webrtc })
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
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
