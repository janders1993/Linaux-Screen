//! The screen capture, shared by every connected viewer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use gst::prelude::*;

use crate::pipeline::{self, Source};

/// The screen capture, running for the whole life of the app in its own
/// pipeline. Each viewer subscribes and gets every frame pushed into its own
/// encoding pipeline.
///
/// Keeping capture separate matters: with capture and WebRTC in one pipeline
/// started when a phone connects, KWin delivered ~1 fps, while the same
/// capture on its own ran at 30-75 fps. It also means a viewer joining while
/// the screen is idle immediately gets the last frame.
pub struct CaptureHub {
    desc: String,
    pipeline: Mutex<gst::Pipeline>,
    shared: Arc<Shared>,
}

/// State the capture pipeline's callback shares with the hub.
#[derive(Default)]
struct Shared {
    viewers: Mutex<Vec<(u64, gst_app::AppSrc)>>,
    last: Mutex<Option<gst::Sample>>,
    next_id: AtomicU64,
}

/// Hands one captured frame to a viewer's pipeline.
fn push(appsrc: &gst_app::AppSrc, sample: &gst::Sample) {
    let (Some(buffer), Some(caps)) = (sample.buffer(), sample.caps()) else {
        return;
    };
    let caps = caps.to_owned();
    if appsrc.caps().as_ref() != Some(&caps) {
        appsrc.set_caps(Some(&caps));
    }
    // Each viewer stamps frames with its own clock (do-timestamp), so drop
    // the capture pipeline's timestamps.
    let mut buffer = buffer.copy();
    {
        let b = buffer.make_mut();
        b.set_pts(gst::ClockTime::NONE);
        b.set_dts(gst::ClockTime::NONE);
        b.set_duration(gst::ClockTime::NONE);
    }
    let _ = appsrc.push_buffer(buffer);
}

impl CaptureHub {
    pub fn start(source: &Source, fps: u32, dmabuf: bool) -> Result<Self> {
        let (src, convert) = pipeline::capture_desc(source, fps, dmabuf);
        let desc = format!(
            "{src} \
             ! queue max-size-buffers=1 leaky=downstream \
             ! {convert} ! video/x-raw,format=NV12 \
             ! appsink name=sink sync=false max-buffers=1 drop=true"
        );
        tracing::debug!("capture pipeline: {desc}");
        let shared = Arc::new(Shared::default());
        let pipeline = Self::build(&desc, &shared, None)?;
        Ok(Self {
            desc,
            pipeline: Mutex::new(pipeline),
            shared,
        })
    }

    fn build(
        desc: &str,
        shared: &Arc<Shared>,
        test_caps: Option<gst::Caps>,
    ) -> Result<gst::Pipeline> {
        let pipeline = pipeline::parse(desc)?;
        if let (Some(caps), Some(filter)) = (test_caps, pipeline.by_name("testcaps")) {
            filter.set_property("caps", caps);
        }
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .context("appsink missing from capture pipeline")?;

        let shared = shared.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    for (_, appsrc) in shared.viewers.lock().unwrap().iter() {
                        push(appsrc, &sample);
                    }
                    *shared.last.lock().unwrap() = Some(sample);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        pipeline::log_capture_caps(&pipeline);
        pipeline::spawn_fps_logger(&pipeline, "capture", "capture");
        pipeline::spawn_bus_logger(&pipeline, "capture")?;
        pipeline
            .set_state(gst::State::Playing)
            .context("capture pipeline refused to start — run with RUST_LOG=debug GST_DEBUG=3")?;
        Ok(pipeline)
    }

    /// Reconnects to the screen. After KDE resizes the monitor, the existing
    /// PipeWire connection keeps delivering frames at the old size (with the
    /// new-size desktop cropped or padded inside them); a fresh connection
    /// negotiates the new size. Viewers stay subscribed throughout.
    pub fn restart(&self) -> Result<()> {
        let mut pipeline = self.pipeline.lock().unwrap();
        // Test pattern: carry its current size over to the new pipeline.
        let test_caps = pipeline
            .by_name("testcaps")
            .map(|f| f.property::<gst::Caps>("caps"));
        let _ = pipeline.set_state(gst::State::Null);
        *pipeline = Self::build(&self.desc, &self.shared, test_caps)?;
        tracing::info!("capture reconnected");
        Ok(())
    }

    /// Starts feeding frames to `appsrc`; returns an id for `unsubscribe`.
    pub(crate) fn subscribe(&self, appsrc: gst_app::AppSrc) -> u64 {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        if let Some(sample) = self.shared.last.lock().unwrap().as_ref() {
            push(&appsrc, sample);
        }
        self.shared.viewers.lock().unwrap().push((id, appsrc));
        id
    }

    pub(crate) fn unsubscribe(&self, id: u64) {
        self.shared
            .viewers
            .lock()
            .unwrap()
            .retain(|(i, _)| *i != id);
    }

    /// The size of the frames currently being captured, once known.
    pub fn frame_size(&self) -> Option<(u32, u32)> {
        let last = self.shared.last.lock().unwrap();
        let caps = last.as_ref()?.caps()?;
        let s = caps.structure(0)?;
        Some((
            s.get::<i32>("width").ok()? as u32,
            s.get::<i32>("height").ok()? as u32,
        ))
    }

    /// Test pattern only: change the frame size, as if the monitor resized.
    /// Returns false if this isn't the test source.
    pub fn resize_test_pattern(&self, width: u32, height: u32) -> bool {
        let Some(filter) = self.pipeline.lock().unwrap().by_name("testcaps") else {
            return false;
        };
        let caps = gst::Caps::builder("video/x-raw")
            .field("width", width as i32)
            .field("height", height as i32)
            .field("framerate", gst::Fraction::new(60, 1))
            .build();
        filter.set_property("caps", caps);
        true
    }
}

impl Drop for CaptureHub {
    fn drop(&mut self) {
        let _ = self.pipeline.lock().unwrap().set_state(gst::State::Null);
    }
}
