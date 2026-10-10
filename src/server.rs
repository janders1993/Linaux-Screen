//! HTTP server: serves the viewer page and the WebSocket used for WebRTC
//! signalling.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};

use crate::display::{self, VirtualDisplay};
use crate::stream::{CaptureHub, Session, Signal};

const VIEWER_HTML: &str = include_str!("../web/index.html");

struct AppState {
    pipeline_desc: String,
    hub: Arc<CaptureHub>,
    display: Option<Arc<VirtualDisplay>>,
    encoder_name: String,
    /// Bumped on every new connection. For the spike only one viewer streams
    /// at a time, so a new viewer takes over from the previous one.
    generation: watch::Sender<u64>,
}

pub async fn run(
    addr: SocketAddr,
    pipeline_desc: String,
    encoder_name: String,
    hub: Arc<CaptureHub>,
    display: Option<Arc<VirtualDisplay>>,
) -> Result<()> {
    let state = Arc::new(AppState {
        pipeline_desc,
        hub,
        display,
        encoder_name,
        generation: watch::Sender::new(0),
    });

    let app = Router::new()
        .route("/", get(|| async { Html(VIEWER_HTML) }))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
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
    tracing::info!("viewer connected from {peer}");

    let mut my_generation = 0;
    state.generation.send_modify(|g| {
        *g += 1;
        my_generation = *g;
    });
    let mut generation_rx = state.generation.subscribe();

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<Signal>();

    let _ = signal_tx.send(Signal::Info { encoder: state.encoder_name.clone() });

    let session = match Session::start(&state.pipeline_desc, signal_tx, state.hub.clone()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("could not start stream for {peer}: {e:#}");
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
    };

    loop {
        tokio::select! {
            Some(signal) = signal_rx.recv() => {
                let Ok(text) = serde_json::to_string(&signal) else { continue };
                if ws_tx.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            incoming = ws_rx.next() => match incoming {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Signal>(&text) {
                    Ok(Signal::Display { width, height, dpr }) => {
                        resize_for(&state, peer, width, height, dpr);
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
            Ok(()) = generation_rx.changed() => {
                if *generation_rx.borrow() != my_generation {
                    tracing::info!("viewer {peer} replaced by a newer connection");
                    let _ = ws_tx.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    }

    drop(session);
    tracing::info!("viewer {peer} disconnected");
}

/// Resizes the virtual monitor to suit the viewer's screen, in the background.
fn resize_for(state: &AppState, peer: SocketAddr, width: u32, height: u32, dpr: f64) {
    let (w, h, scale) = display::plan(width, height, dpr);
    tracing::info!("{peer} screen is {width}x{height} @{dpr}x");
    let Some(display) = state.display.clone() else {
        if state.hub.resize_test_pattern(w, h) {
            tracing::info!("test pattern resized to {w}x{h}");
        }
        return;
    };
    tokio::spawn(async move {
        if let Err(e) = display.apply(w, h, scale).await {
            tracing::warn!("couldn't resize the virtual monitor: {e:#}");
        }
    });
}
