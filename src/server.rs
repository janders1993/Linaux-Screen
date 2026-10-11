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
use crate::control::{Control, MAX_VIEWERS};
use crate::display::{self, VirtualDisplay};
use crate::hub::CaptureHub;
use crate::pipeline::EncoderChoice;
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
    display: Option<Arc<VirtualDisplay>>,
}

pub async fn run(
    listener: tokio::net::TcpListener,
    control: Arc<Control>,
    audio: Arc<AudioRouter>,
    encoder: EncoderChoice,
    hub: Arc<CaptureHub>,
    display: Option<Arc<VirtualDisplay>>,
) -> Result<()> {
    let state = Arc::new(AppState {
        control,
        audio,
        encoder,
        hub,
        display,
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
    });

    let session = match Session::start(&desc, signal_tx, state.hub.clone()) {
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
                    Ok(Signal::Display { width, height, dpr }) => {
                        // Only the first device steers the size, so two
                        // devices don't fight over it.
                        if state.control.is_first_viewer(peer) {
                            resize_for(&state, peer, width, height, dpr);
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
    drop(session);
    tracing::info!("viewer {peer} disconnected");
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

/// With `--fit-device`, resizes the virtual monitor to suit the viewer's
/// screen, in the background. In test mode it resizes the test pattern.
fn resize_for(state: &AppState, peer: SocketAddr, width: u32, height: u32, dpr: f64) {
    let (w, h, scale) = display::plan(width, height, dpr);
    tracing::debug!("{peer} screen is {width}x{height} @{dpr}x");
    let Some(display) = state.display.clone() else {
        if state.hub.resize_test_pattern(w, h) {
            tracing::info!("test pattern resized to {w}x{h}");
            // Reconnect like the real monitor does, so tests cover it.
            let hub = state.hub.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = hub.restart() {
                    tracing::error!("couldn't reconnect capture after resize: {e:#}");
                }
            });
        }
        return;
    };
    let hub = state.hub.clone();
    tokio::spawn(async move {
        match display.apply(w, h, scale).await {
            Ok(true) => {
                // Give KWin a moment to finish the mode change, then reconnect
                // so the capture picks up the new size.
                tokio::time::sleep(Duration::from_millis(300)).await;
                let result = tokio::task::spawn_blocking(move || hub.restart()).await;
                if let Ok(Err(e)) = result {
                    tracing::error!("couldn't reconnect capture after resize: {e:#}");
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("couldn't resize the virtual monitor: {e:#}"),
        }
    });
}
