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
use crate::quality::Preset;

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
        /// Identifies this build's viewer page. A page left open from an
        /// older build reloads itself when it sees a different value.
        page: String,
        /// Whether mouse and keyboard control can be requested at all.
        input: bool,
    },
    /// How many devices are watching, sent whenever it changes.
    Viewers {
        count: usize,
    },
    /// Whether this device is now the chosen sound output.
    Audio {
        on: bool,
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
    /// Stream quality preset: "saver", "smooth", "sharp" or "ultra". The
    /// viewer asks; the server confirms with the resulting bitrate.
    Quality {
        preset: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kbps: Option<u32>,
    },
    /// The viewer asks to control the PC (from its menu). The PC must accept.
    #[serde(rename = "request-control")]
    RequestControl,
    /// The viewer gives control back.
    #[serde(rename = "release-control")]
    ReleaseControl,
    /// This device's control state: "off", "pending", "on", "denied",
    /// "cooldown" (with `wait` seconds) or "unavailable".
    Control {
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait: Option<u64>,
    },
    /// Pointer position, 0..1 across the video. Only acted on with control.
    Pointer {
        x: f64,
        y: f64,
    },
    /// "left", "right" or "middle" pressed or released.
    Button {
        button: String,
        down: bool,
    },
    /// Smooth scroll in the PC's logical pixels; `end` when fingers lift.
    Scroll {
        dx: f64,
        dy: f64,
        #[serde(default)]
        end: bool,
    },
    /// A physical key by `KeyboardEvent.code`, pressed or released.
    Key {
        code: String,
        down: bool,
    },
}

impl Signal {
    /// Input events, which need the PC's permission.
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            Signal::Pointer { .. }
                | Signal::Button { .. }
                | Signal::Scroll { .. }
                | Signal::Key { .. }
        )
    }
}

/// One viewer's pipeline. Dropping it stops the stream.
pub struct Session {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    hub: Arc<CaptureHub>,
    subscription: u64,
    encoder: Option<gst::Element>,
    /// The automatic (Smooth) bitrate the stream started with.
    base_kbps: u32,
    /// Where this device's audio goes in, while it's the chosen output.
    pub audio_src: gst_app::AppSrc,
}

impl Session {
    pub fn start(
        desc: &str,
        base_kbps: u32,
        tx: UnboundedSender<Signal>,
        hub: Arc<CaptureHub>,
    ) -> Result<Self> {
        let pipeline = pipeline::parse(desc)?;
        let encoder = pipeline.by_name("enc");
        let webrtc = pipeline
            .by_name("webrtc")
            .context("webrtcbin missing from pipeline")?;
        let appsrc = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .context("appsrc missing from viewer pipeline")?;
        let audio_src = pipeline
            .by_name("asrc")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .context("audio appsrc missing from viewer pipeline")?;
        pipeline::spawn_fps_logger(&pipeline, "parse", "encoded");

        // We only send (video and audio); tell the browser not to send any.
        for index in 0..2i32 {
            if let Some(transceiver) = webrtc
                .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
                    "get-transceiver",
                    &[&index],
                )
            {
                transceiver.set_property(
                    "direction",
                    gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
                );
            }
        }

        let offer_tx = tx.clone();
        webrtc.connect("on-negotiation-needed", false, move |values| {
            let webrtc = values[0].get::<gst::Element>().expect("webrtcbin");
            // Only negotiate once. Later "negotiation needed" comes from the
            // video size changing (a monitor mode change), which H.264
            // carries in-band, so the browser copes without a new offer. A
            // re-offer also crashed webrtcbin (GStreamer 1.24) once the
            // audio track was added.
            if webrtc
                .property::<Option<gst_webrtc::WebRTCSessionDescription>>(
                    "current-remote-description",
                )
                .is_some()
            {
                tracing::debug!("not renegotiating: the stream carries size changes itself");
                return None;
            }
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

        // WebRTC won't make an offer until every track's format is known,
        // and the audio track only gets data while this device is the
        // chosen output. One short burst of silence sets it up straight away.
        let mut silence = gst::Buffer::from_mut_slice(vec![0u8; 48_000 / 100 * 2 * 2]); // 10 ms
        silence.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        let _ = audio_src.push_buffer(silence);

        let subscription = hub.subscribe(appsrc);
        Ok(Self {
            pipeline,
            webrtc,
            hub,
            subscription,
            encoder,
            base_kbps,
            audio_src,
        })
    }

    /// Switches this device's stream to `preset` straight away (no
    /// reconnect). Returns the new bitrate.
    pub fn set_quality(&self, preset: Preset) -> u32 {
        let kbps = preset.kbps(self.base_kbps);
        match &self.encoder {
            Some(enc) if enc.find_property("bitrate").is_some() => {
                let value = preset.encoder_kbps(self.base_kbps);
                enc.set_property_from_str("bitrate", &value.to_string());
            }
            _ => tracing::warn!("can't change the bitrate of this encoder"),
        }
        self.hub.set_max_fps(self.subscription, preset.max_fps());
        kbps
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

#[cfg(test)]
mod tests {
    use super::Signal;

    #[test]
    fn input_messages_parse() {
        let s: Signal = serde_json::from_str(r#"{"type":"request-control"}"#).unwrap();
        assert!(matches!(s, Signal::RequestControl));
        let s: Signal =
            serde_json::from_str(r#"{"type":"key","code":"MetaLeft","down":true}"#).unwrap();
        assert!(s.is_input());
        let s: Signal = serde_json::from_str(r#"{"type":"scroll","dx":0,"dy":-12.5}"#).unwrap();
        assert!(matches!(s, Signal::Scroll { end: false, .. }));
        let text = serde_json::to_string(&Signal::Control {
            state: "on".into(),
            wait: None,
        })
        .unwrap();
        assert_eq!(text, r#"{"type":"control","state":"on"}"#);
    }
}
