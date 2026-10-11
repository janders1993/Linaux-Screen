//! Sending the PC's sound to one chosen device.
//!
//! Nothing happens until a device is picked in the tray. Then the app:
//! 1. creates a "Linaux Screen" speaker in PipeWire, owned by a `pw-cli`
//!    helper process that dies with the app, so the speaker can never outlive
//!    it (PipeWire then falls back to the PC's own output);
//! 2. makes that speaker the default output (remembering the old default);
//! 3. captures what's played into it and sends it to the chosen device.
//!
//! Picking "This PC", the device disconnecting, or the app stopping undoes it.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use gst::prelude::*;

use crate::control::Control;
use crate::pipeline::{self, AUDIO_CAPS};

const SINK_NAME: &str = "linaux-screen";

/// Whether the PipeWire tools audio switching needs are installed.
pub fn available() -> bool {
    let works = |cmd: &str, arg: &str| {
        Command::new(cmd)
            .arg(arg)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    works("pactl", "--version") && works("pw-cli", "--version")
}

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .context("could not run pactl")?;
    if !out.status.success() {
        bail!(
            "pactl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn sink_exists(name: &str) -> bool {
    pactl(&["list", "short", "sinks"])
        .map(|list| list.lines().any(|l| l.split('\t').nth(1) == Some(name)))
        .unwrap_or(false)
}

/// Where the previous default output is noted while ours is in use, so it
/// can be put back even after a crash.
fn restore_file() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .map(|d| d.join("linaux-screen").join("audio-previous-default"))
}

/// Puts back a default output left over from a run that didn't finish
/// cleanly. Called at startup.
pub fn recover() {
    let Some(file) = restore_file() else { return };
    let Ok(previous) = std::fs::read_to_string(&file) else {
        return;
    };
    let previous = previous.trim();
    if !previous.is_empty() && sink_exists(previous) {
        match pactl(&["set-default-sink", previous]) {
            Ok(_) => tracing::info!("restored audio output {previous} after an unclean exit"),
            Err(e) => tracing::warn!("couldn't restore audio output {previous}: {e:#}"),
        }
    }
    let _ = std::fs::remove_file(file);
}

/// The "Linaux Screen" speaker, set as the default output while it exists.
struct VirtualSpeaker {
    /// Dropping this tells the helper's thread to stop it.
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    previous_default: Option<String>,
}

/// Starts the `pw-cli` helper that owns the speaker, on a thread of its own
/// that lives exactly as long as the speaker.
///
/// The helper is set to be killed if its parent dies (so a crashed app can't
/// leave the PC silent). Linux counts the *thread* that started it as the
/// parent, so it must not be a worker thread that might be retired while the
/// speaker is in use: that's what made sound drop back to the PC every so
/// often.
fn spawn_helper() -> Result<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("linaux-speaker".into())
        .spawn(move || {
            let mut cmd = Command::new("pw-cli");
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            unsafe {
                use std::os::unix::process::CommandExt;
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }
            let mut helper = match cmd.spawn() {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(e).context("could not start pw-cli"));
                    return;
                }
            };
            let create = format!(
                "create-node adapter {{ factory.name=support.null-audio-sink node.name={SINK_NAME} \
                 node.description=\"Linaux Screen\" media.class=Audio/Sink object.linger=false \
                 audio.position=[FL,FR] }}\n"
            );
            let written = helper
                .stdin
                .as_mut()
                .context("no stdin for pw-cli")
                .and_then(|stdin| Ok(stdin.write_all(create.as_bytes())?));
            let failed = written.is_err();
            let _ = ready_tx.send(written);
            if !failed {
                // Wait until the speaker is no longer wanted (or the sender
                // is dropped).
                let _ = stop_rx.recv();
            }
            let _ = helper.kill();
            let _ = helper.wait();
        })
        .context("could not start the speaker thread")?;
    ready_rx
        .recv()
        .context("the speaker thread stopped unexpectedly")??;
    Ok((stop_tx, thread))
}

impl VirtualSpeaker {
    fn create() -> Result<Self> {
        let previous_default = pactl(&["get-default-sink"]).ok().filter(|d| d != SINK_NAME);
        let (stop, thread) = spawn_helper()?;
        // From here on, dropping `speaker` cleans up.
        let speaker = Self {
            stop: Some(stop),
            thread: Some(thread),
            previous_default,
        };
        for _ in 0..30 {
            if sink_exists(SINK_NAME) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !sink_exists(SINK_NAME) {
            bail!("the Linaux Screen speaker didn't appear in PipeWire");
        }

        if let (Some(file), Some(previous)) = (restore_file(), &speaker.previous_default) {
            let _ = std::fs::write(file, previous);
        }
        pactl(&["set-default-sink", SINK_NAME])?;
        tracing::info!(
            "sound now goes to the device (was {})",
            speaker.previous_default.as_deref().unwrap_or("unknown")
        );
        Ok(speaker)
    }
}

impl Drop for VirtualSpeaker {
    fn drop(&mut self) {
        // Put the PC's output back first, so apps move straight to it.
        if let Some(previous) = &self.previous_default {
            if let Err(e) = pactl(&["set-default-sink", previous]) {
                tracing::warn!("couldn't restore audio output {previous}: {e:#}");
            }
        }
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(file) = restore_file() {
            let _ = std::fs::remove_file(file);
        }
        tracing::info!("sound back on the PC");
    }
}

/// Which device's pipeline gets audio, and each device's audio input.
#[derive(Default)]
pub struct AudioRouter {
    inputs: Mutex<HashMap<SocketAddr, gst_app::AppSrc>>,
    target: Mutex<Option<SocketAddr>>,
    clock: Mutex<Option<SampleClock>>,
}

/// Timestamps sound by counting samples, so they advance by exactly the
/// length of each chunk. (Stamping chunks by when they happen to arrive made
/// the timing uneven, and the browser played it back choppy and robotic.)
struct SampleClock {
    target: SocketAddr,
    start: gst::ClockTime,
    samples: u64,
}

/// How far the sample count may drift from the viewer pipeline's clock
/// before it's re-synced (the PC's sound clock and the system clock tick at
/// very slightly different rates).
const MAX_DRIFT: gst::ClockTime = gst::ClockTime::from_mseconds(150);

/// The viewer pipeline's current running time.
fn running_time(input: &gst_app::AppSrc) -> Option<gst::ClockTime> {
    let clock = input.clock()?;
    let base = input.base_time()?;
    clock.time().checked_sub(base)
}

impl AudioRouter {
    pub fn register(&self, peer: SocketAddr, input: gst_app::AppSrc) {
        self.inputs.lock().unwrap().insert(peer, input);
    }

    pub fn unregister(&self, peer: SocketAddr) {
        self.inputs.lock().unwrap().remove(&peer);
    }

    fn push(&self, sample: &gst::Sample) {
        let Some(target) = *self.target.lock().unwrap() else {
            return;
        };
        let inputs = self.inputs.lock().unwrap();
        let (Some(input), Some(buffer)) = (inputs.get(&target), sample.buffer()) else {
            return;
        };
        let Some(now) = running_time(input) else {
            return;
        };
        let samples = (buffer.size() / 4) as u64; // 16-bit stereo
        let duration = gst::ClockTime::from_nseconds(samples * 1_000_000_000 / 48_000);

        let mut clock = self.clock.lock().unwrap();
        let pts = match clock.as_mut() {
            Some(c) if c.target == target => {
                let pts =
                    c.start + gst::ClockTime::from_nseconds(c.samples * 1_000_000_000 / 48_000);
                let drift = if pts > now { pts - now } else { now - pts };
                if drift > MAX_DRIFT {
                    tracing::debug!("re-syncing sound timing (drift {drift})");
                    c.start = now;
                    c.samples = 0;
                    now
                } else {
                    pts
                }
            }
            _ => {
                // New target: start counting from its pipeline's current time.
                *clock = Some(SampleClock {
                    target,
                    start: now,
                    samples: 0,
                });
                now
            }
        };
        if let Some(c) = clock.as_mut() {
            c.samples += samples;
        }
        drop(clock);

        let mut buffer = buffer.copy();
        {
            let b = buffer.make_mut();
            b.set_pts(pts);
            b.set_dts(gst::ClockTime::NONE);
            b.set_duration(duration);
        }
        let _ = input.push_buffer(buffer);
    }
}

/// Captures sound: from the Linaux Screen speaker, or a test tone.
fn start_capture(test_tone: bool, router: &Arc<AudioRouter>) -> Result<gst::Pipeline> {
    let src = if test_tone {
        "audiotestsrc is-live=true wave=sine freq=440 volume=0.2".to_owned()
    } else {
        // Small buffers keep the delay down.
        pipeline::element_desc(
            "pulsesrc",
            &[
                ("device", &format!("{SINK_NAME}.monitor")),
                ("buffer-time", "40000"),
                ("latency-time", "10000"),
                ("do-timestamp", "true"),
            ],
        )
        .context("GStreamer's pulsesrc isn't installed (gstreamer1-plugins-good)")?
    };
    let desc = format!(
        "{src} ! audioconvert ! audioresample ! {AUDIO_CAPS} \
         ! appsink name=asink sync=false max-buffers=8 drop=true"
    );
    let pipeline = pipeline::parse(&desc)?;
    let sink = pipeline
        .by_name("asink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .context("audio appsink missing")?;
    let router = router.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                router.push(&sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline::spawn_bus_logger(&pipeline, "audio")?;
    pipeline
        .set_state(gst::State::Playing)
        .context("audio capture refused to start")?;
    Ok(pipeline)
}

/// Follows the tray's audio choice until the app exits, then puts sound back
/// on the PC. `test_tone` (with `--source test`) sends a tone instead and
/// leaves the PC's outputs alone.
pub async fn run(control: Arc<Control>, router: Arc<AudioRouter>, test_tone: bool) {
    let mut choice = control.audio_target.subscribe();
    let mut exit = control.exit.subscribe();
    let mut speaker: Option<VirtualSpeaker> = None;
    let mut capture: Option<gst::Pipeline> = None;

    loop {
        let target = *choice.borrow_and_update();
        *router.target.lock().unwrap() = target;
        match target {
            Some(device) if capture.is_none() => {
                let started = tokio::task::spawn_blocking(move || -> Result<_> {
                    let speaker = if test_tone {
                        None
                    } else {
                        Some(VirtualSpeaker::create()?)
                    };
                    Ok(speaker)
                })
                .await;
                match started {
                    Ok(Ok(s)) => {
                        speaker = s;
                        match start_capture(test_tone, &router) {
                            Ok(p) => {
                                tracing::info!("sending sound to {device}");
                                capture = Some(p);
                            }
                            Err(e) => {
                                tracing::error!("couldn't capture sound: {e:#}");
                                speaker = None;
                                control.audio_target.send_replace(None);
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::error!("couldn't switch sound to the device: {e:#}");
                        control.audio_target.send_replace(None);
                    }
                    Err(_) => {
                        control.audio_target.send_replace(None);
                    }
                }
            }
            Some(device) => tracing::info!("sending sound to {device}"),
            None => {
                if let Some(p) = capture.take() {
                    let _ = p.set_state(gst::State::Null);
                }
                if let Some(s) = speaker.take() {
                    let _ = tokio::task::spawn_blocking(move || drop(s)).await;
                }
            }
        }

        let mut check_speaker = false;
        tokio::select! {
            r = choice.changed() => if r.is_err() { break },
            _ = exit.wait_for(|e| e.is_some()) => break,
            _ = tokio::time::sleep(Duration::from_secs(2)), if speaker.is_some() => check_speaker = true,
        }
        // Safety net: if the speaker vanishes some other way (e.g. PipeWire
        // restarted), PipeWire has already gone back to the PC's output;
        // catch up so the tray doesn't claim otherwise.
        if check_speaker
            && !tokio::task::spawn_blocking(|| sink_exists(SINK_NAME))
                .await
                .unwrap_or(true)
        {
            tracing::warn!("the Linaux Screen speaker disappeared; sound is back on the PC");
            crate::desktop::notify(
                "Sound back on the PC",
                "The connection to the device's sound was lost.",
            )
            .await;
            control.audio_target.send_replace(None);
        }
    }

    *router.target.lock().unwrap() = None;
    if let Some(p) = capture.take() {
        let _ = p.set_state(gst::State::Null);
    }
    if let Some(s) = speaker.take() {
        let _ = tokio::task::spawn_blocking(move || drop(s)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a running PipeWire with pactl and pw-cli:
    /// `cargo test -- --ignored --test-threads=1` (the tests share PipeWire)
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn audio_switches_and_restores() {
        gst::init().unwrap();
        let before = pactl(&["get-default-sink"]).unwrap();
        let control = Arc::new(Control::with_audio(String::new(), true));
        let router = Arc::new(AudioRouter::default());
        let task = tokio::spawn(run(control.clone(), router, false));

        let peer: SocketAddr = "192.168.1.9:5000".parse().unwrap();
        control.audio_target.send_replace(Some(peer));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(pactl(&["get-default-sink"]).unwrap(), SINK_NAME);
        assert!(
            restore_file().unwrap().exists(),
            "previous default should be noted"
        );

        control.audio_target.send_replace(None);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(pactl(&["get-default-sink"]).unwrap(), before);
        assert!(!sink_exists(SINK_NAME), "speaker should be gone");

        // Picked again, then the app exits: sound must come back.
        control.audio_target.send_replace(Some(peer));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(pactl(&["get-default-sink"]).unwrap(), SINK_NAME);
        control.request_exit(crate::control::Exit::Stop);
        task.await.unwrap();
        assert_eq!(pactl(&["get-default-sink"]).unwrap(), before);
        assert!(!sink_exists(SINK_NAME));
        assert!(!restore_file().unwrap().exists());
    }

    /// Helper for the crash test: switches sound to the device, then waits
    /// to be killed.
    #[test]
    #[ignore]
    fn crash_helper_hold_speaker() {
        if std::env::var("LINAUX_CRASH_HELPER").is_err() {
            return;
        }
        let _speaker = VirtualSpeaker::create().unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    /// Needs a running PipeWire (run with `--test-threads=1`).
    #[test]
    #[ignore]
    fn speaker_gone_after_crash() {
        let before = pactl(&["get-default-sink"]).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "audio::tests::crash_helper_hold_speaker",
                "--nocapture",
            ])
            .env("LINAUX_CRASH_HELPER", "1")
            .spawn()
            .unwrap();
        for _ in 0..50 {
            if pactl(&["get-default-sink"]).unwrap() == SINK_NAME {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(pactl(&["get-default-sink"]).unwrap(), SINK_NAME);

        child.kill().unwrap(); // SIGKILL: no clean-up code runs
        child.wait().unwrap();
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !sink_exists(SINK_NAME),
            "speaker should vanish with the app"
        );
        assert_ne!(pactl(&["get-default-sink"]).unwrap(), SINK_NAME);

        // Next start puts the exact previous output back.
        recover();
        assert_eq!(pactl(&["get-default-sink"]).unwrap(), before);
        assert!(!restore_file().unwrap().exists());
    }

    /// Needs PipeWire. The speaker must survive the worker thread that
    /// created it being retired (tokio retires idle blocking threads after
    /// ~10 s).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn speaker_survives_idle_threads() {
        gst::init().unwrap();
        let control = Arc::new(Control::with_audio(String::new(), true));
        let task = tokio::spawn(run(
            control.clone(),
            Arc::new(AudioRouter::default()),
            false,
        ));
        control
            .audio_target
            .send_replace(Some("192.168.1.9:5000".parse().unwrap()));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(sink_exists(SINK_NAME));
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert!(
            sink_exists(SINK_NAME),
            "speaker vanished while still in use"
        );
        assert_eq!(pactl(&["get-default-sink"]).unwrap(), SINK_NAME);
        control.request_exit(crate::control::Exit::Stop);
        task.await.unwrap();
    }

    /// Needs PipeWire. If the speaker is removed behind the app's back, the
    /// app notices and goes back to "This PC".
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn notices_speaker_removed_externally() {
        gst::init().unwrap();
        let control = Arc::new(Control::with_audio(String::new(), true));
        let task = tokio::spawn(run(
            control.clone(),
            Arc::new(AudioRouter::default()),
            false,
        ));
        control
            .audio_target
            .send_replace(Some("192.168.1.9:5000".parse().unwrap()));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(sink_exists(SINK_NAME));
        // Kill the helper as if PipeWire had dropped the speaker.
        let _ = Command::new("pkill").args(["-x", "pw-cli"]).status();
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(
            *control.audio_target.borrow(),
            None,
            "tray should be back on This PC"
        );
        assert_ne!(pactl(&["get-default-sink"]).unwrap(), SINK_NAME);
        control.request_exit(crate::control::Exit::Stop);
        task.await.unwrap();
    }
}
