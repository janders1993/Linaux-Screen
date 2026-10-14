//! HTTP server: serves the viewer page and the WebSocket used for WebRTC
//! signalling.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::audio::AudioRouter;
use crate::control::{Control, Request, MAX_VIEWERS};
use crate::hub::CaptureHub;
use crate::input::{self, DeviceInput, InputSink};
use crate::pipeline::EncoderChoice;
use crate::quality::Preset;
use crate::session::{Session, Signal};

const VIEWER_HTML: &str = include_str!("../web/index.html");

/// A short fingerprint of the viewer page. Pages stay open across app
/// restarts and reconnect by themselves, so a page from an older build would
/// otherwise keep running after an update.
fn page_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        VIEWER_HTML.hash(&mut h);
        format!("{:016x}", h.finish())
    })
}

struct AppState {
    control: Arc<Control>,
    audio: Arc<AudioRouter>,
    encoder: EncoderChoice,
    hub: Arc<CaptureHub>,
    input: Option<InputSink>,
}

pub async fn run(
    listener: tokio::net::TcpListener,
    control: Arc<Control>,
    audio: Arc<AudioRouter>,
    encoder: EncoderChoice,
    hub: Arc<CaptureHub>,
    input: Option<InputSink>,
) -> Result<()> {
    let state = Arc::new(AppState {
        control,
        audio,
        encoder,
        hub,
        input,
    });

    let app = Router::new()
        // no-store: phones otherwise keep showing an old copy of the viewer
        // after the app is updated.
        .route(
            "/",
            get(|| async {
                (
                    [(axum::http::header::CACHE_CONTROL, "no-store")],
                    Html(VIEWER_HTML.replace("__PAGE_VERSION__", page_version())),
                )
            }),
        )
        .route("/ws", get(ws_handler))
        .with_state(state);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_viewer(socket, peer, state))
}

async fn handle_viewer(socket: WebSocket, peer: SocketAddr, state: Arc<AppState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Every device watches the same screen, up to MAX_VIEWERS at once.
    let Some(_registered) = state.control.viewer_connected(peer) else {
        tracing::warn!("turning away {peer}: already {MAX_VIEWERS} devices connected");
        say_bye(&mut ws_tx, "full").await;
        return;
    };
    tracing::info!("viewer connected from {peer}");
    let mut kick_rx = state.control.kick.subscribe();
    let mut exit_rx = state.control.exit.subscribe();
    let mut viewers_rx = state.control.viewers.subscribe();
    viewers_rx.mark_changed(); // send the current count straight away
    let mut audio_rx = state.control.audio_target.subscribe();
    let mut audio_on = false;
    let mut last_count = 0;
    // Mouse and keyboard: nothing from this device reaches the PC unless
    // the PC has accepted it (checked on every event below).
    let mut access_rx = state.control.access.subscribe();
    let mut control_state = "off"; // what the viewer was last told
    let mut withdrew = false; // the viewer cancelled its own request
    let mut device_input = state.input.clone().map(DeviceInput::new);
    let mut last_ignored: Option<std::time::Instant> = None;

    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<Signal>();

    let (width, height) = state.hub.frame_size().unwrap_or((1920, 1080));
    let (encoder_name, desc, kbps) = match state.encoder.viewer_pipeline(width, height) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("no encoder for {peer}: {e:#}");
            return;
        }
    };
    tracing::info!("streaming {width}x{height} to {peer} with {encoder_name} at {kbps} kbit/s");
    tracing::debug!("viewer pipeline: {desc}");
    let _ = signal_tx.send(Signal::Info {
        encoder: encoder_name,
        page: page_version().to_owned(),
        input: state.control.input_available,
    });

    let session = match Session::start(&desc, kbps, signal_tx, state.hub.clone()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("could not start stream for {peer}: {e:#}");
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
    };
    state.audio.register(peer, session.audio_src.clone());

    loop {
        tokio::select! {
            Ok(()) = kick_rx.changed() => {
                tracing::info!("disconnecting {peer} (disconnect all)");
                say_bye(&mut ws_tx, "disconnected").await;
                break;
            }
            Ok(()) = exit_rx.changed() => {
                say_bye(&mut ws_tx, "stopped").await;
                break;
            }
            Ok(()) = audio_rx.changed() => {
                let on = *audio_rx.borrow_and_update() == Some(peer);
                if on == audio_on { continue }
                audio_on = on;
                let Ok(text) = serde_json::to_string(&Signal::Audio { on }) else { continue };
                if ws_tx.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            Ok(()) = access_rx.changed() => {
                let now = {
                    let a = access_rx.borrow_and_update();
                    if a.granted.contains(&peer) {
                        "on"
                    } else if a.pending.contains(&peer) {
                        "pending"
                    } else {
                        "off"
                    }
                };
                if now == control_state { continue }
                if control_state == "on" {
                    // Control ended: let go of anything still held.
                    if let Some(d) = device_input.as_mut() { d.release_all() }
                    tracing::info!("{peer} no longer has control");
                } else if now == "on" {
                    tracing::info!("{peer} has control");
                }
                let shown = if control_state == "pending" && now == "off" && !withdrew {
                    "denied"
                } else {
                    now
                };
                control_state = now;
                withdrew = false;
                if !send(&mut ws_tx, &Signal::Control { state: shown.into(), wait: None }).await {
                    break;
                }
            }
            Ok(()) = viewers_rx.changed() => {
                let count = viewers_rx.borrow_and_update().len();
                // Names changing doesn't change the count.
                if count == last_count { continue }
                last_count = count;
                let Ok(text) = serde_json::to_string(&Signal::Viewers { count }) else { continue };
                if ws_tx.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            Some(signal) = signal_rx.recv() => {
                let Ok(text) = serde_json::to_string(&signal) else { continue };
                if ws_tx.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            incoming = ws_rx.next() => match incoming {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Signal>(&text) {
                    Ok(Signal::Quality { preset, .. }) => {
                        // Only this device's own stream; no PC approval needed.
                        let Some(preset) = Preset::parse(&preset) else { continue };
                        let kbps = session.set_quality(preset);
                        tracing::info!("{peer}: quality {} ({kbps} kbit/s)", preset.name());
                        let reply = Signal::Quality { preset: preset.name().into(), kbps: Some(kbps) };
                        if !send(&mut ws_tx, &reply).await { break }
                    }
                    Ok(Signal::RequestControl) => {
                        let reply = match state.control.request_control(peer) {
                            Request::Pending => {
                                tokio::spawn(input::ask_host(state.control.clone(), peer));
                                None
                            }
                            // Already sorted; remind the viewer where it stands.
                            Request::AlreadyPending | Request::Granted => {
                                Some(Signal::Control { state: control_state.into(), wait: None })
                            }
                            Request::Cooldown(wait) => {
                                Some(Signal::Control { state: "cooldown".into(), wait: Some(wait) })
                            }
                            Request::Unavailable => {
                                Some(Signal::Control { state: "unavailable".into(), wait: None })
                            }
                        };
                        if let Some(reply) = reply {
                            if !send(&mut ws_tx, &reply).await { break }
                        }
                    }
                    Ok(Signal::ReleaseControl) => {
                        withdrew = true;
                        state.control.revoke(peer);
                    }
                    Ok(signal) if signal.is_input() => {
                        match device_input.as_mut() {
                            Some(d) if state.control.has_control(peer) => apply_input(d, signal),
                            _ => {
                                // Not allowed: drop it. Log now and then, not per event.
                                if last_ignored.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
                                    tracing::warn!("ignoring input from {peer}: it doesn't have control");
                                    last_ignored = Some(std::time::Instant::now());
                                }
                            }
                        }
                    }
                    Ok(Signal::Hello { device }) => {
                        let device: String = device.chars().filter(|c| !c.is_control()).take(40).collect();
                        tracing::info!("{peer} is {device}");
                        state.control.set_viewer_name(peer, device);
                    }
                    Ok(signal) => {
                        if let Err(e) = session.handle(signal) {
                            tracing::warn!("{peer}: {e:#}");
                        }
                    }
                    Err(e) => tracing::warn!("{peer}: bad message: {e}"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
        }
    }

    state.audio.unregister(peer);
    drop(device_input); // lets go of any held keys or buttons
    drop(session);
    tracing::info!("viewer {peer} disconnected");
}

/// Sends one message; false if the connection is gone.
async fn send(ws_tx: &mut SplitSink<WebSocket, Message>, signal: &Signal) -> bool {
    match serde_json::to_string(signal) {
        Ok(text) => ws_tx.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => true,
    }
}

fn apply_input(input: &mut DeviceInput, signal: Signal) {
    match signal {
        Signal::Pointer { x, y } => input.pointer(x, y),
        Signal::Button { button, down } => input.button(&button, down),
        Signal::Scroll { dx, dy, end } => input.scroll(dx, dy, end),
        Signal::Key { code, down } => input.key(&code, down),
        _ => {}
    }
}

/// Tells the viewer we're closing on purpose, so it doesn't keep retrying.
async fn say_bye(ws_tx: &mut SplitSink<WebSocket, Message>, reason: &str) {
    if let Ok(text) = serde_json::to_string(&Signal::Bye {
        reason: reason.into(),
    }) {
        let _ = ws_tx.send(Message::Text(text.into())).await;
    }
    let _ = ws_tx.send(Message::Close(None)).await;
}
