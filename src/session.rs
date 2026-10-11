//! One viewer's stream: its encode + WebRTC pipeline, and the messages
//! exchanged with the browser. The server is always the WebRTC offerer; the
//! browser answers.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::hub::CaptureHub;
use crate::pipeline;

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
    /// Server-side status for the viewer's stats overlay.
    Info {
        encoder: String,
    },
    /// How many devices are watching, sent whenever it changes.
    Viewers {
        count: usize,
    },
    /// The viewer says what kind of device it is ("iPad", …), for the tray.
    Hello {
        device: String,
    },
    /// Server is closing this connection on purpose; the viewer shouldn't
    /// reconnect by itself. `reason` is "disconnected", "stopped" or "full".
    Bye {
        reason: String,
    },
    /// The viewer's screen in physical pixels, sent on connect and rotation.
    Display {
        width: u32,
        height: u32,
        dpr: f64,
    },
}

/// One viewer's pipeline. Dropping it stops the stream.
pub struct Session {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    hub: Arc<CaptureHub>,
    subscription: u64,
}

impl Session {
    pub fn start(desc: &str, tx: UnboundedSender<Signal>, hub: Arc<CaptureHub>) -> Result<Self> {
        let pipeline = pipeline::parse(desc)?;
        let webrtc = pipeline
            .by_name("webrtc")
            .context("webrtcbin missing from pipeline")?;
        let appsrc = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .context("appsrc missing from viewer pipeline")?;
        pipeline::spawn_fps_logger(&pipeline, "parse", "encoded");

        // We only send video; tell the browser not to bother sending any.
        if let Some(transceiver) = webrtc
            .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>("get-transceiver", &[&0i32])
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
                let Some(webrtc) = webrtc_weak.upgrade() else {
                    return;
                };
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
                webrtc
                    .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
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

        webrtc.connect("on-ice-candidate", false, move |values| {
            let sdp_mline_index = values[1].get::<u32>().expect("mline index");
            let candidate = values[2].get::<String>().expect("candidate");
            let _ = tx.send(Signal::Ice {
                candidate,
                sdp_mline_index,
            });
            None
        });

        webrtc.connect_notify(Some("connection-state"), |webrtc, _| {
            let state =
                webrtc.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
            tracing::info!("viewer connection: {state:?}");
        });

        pipeline::spawn_bus_logger(&pipeline, "viewer")?;
        pipeline.set_state(gst::State::Playing).context(
            "pipeline refused to start — run with RUST_LOG=debug GST_DEBUG=3 for details",
        )?;

        let subscription = hub.subscribe(appsrc);
        Ok(Self {
            pipeline,
            webrtc,
            hub,
            subscription,
        })
    }

    pub fn handle(&self, signal: Signal) -> Result<()> {
        match signal {
            Signal::Answer { sdp } => {
                let sdp = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                    .map_err(|_| anyhow!("viewer sent an invalid SDP answer"))?;
                let answer = gst_webrtc::WebRTCSessionDescription::new(
                    gst_webrtc::WebRTCSDPType::Answer,
                    sdp,
                );
                self.webrtc.emit_by_name::<()>(
                    "set-remote-description",
                    &[&answer, &None::<gst::Promise>],
                );
            }
            Signal::Ice {
                candidate,
                sdp_mline_index,
            } => {
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
