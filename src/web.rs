//! Web UI server with WebSocket live feed.
//!
//! Provides a browser-based dashboard for the Twitch translator.
//! Serves a self-contained HTML UI at `/` and accepts WebSocket
//! connections at `/ws` that receive real-time translation events.
//!
//! # Endpoints
//!
//! - `GET /` — Serves the embedded HTML/CSS/JS web UI.
//! - `GET /ws` — WebSocket upgrade. Each connection subscribes to the
//!   broadcast channel and receives all [`WebEvent`]s as JSON text frames.
//! - `GET /api/config` — Returns JSON with the current channel name,
//!   used by the UI to configure the Twitch player embed.
//! - `POST /api/channel` — Switches the active Twitch channel at runtime.

use crate::events::WebEvent;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{info, warn};

/// Maximum number of recent events kept in the replay buffer.
/// New WebSocket clients receive this history on connect.
const REPLAY_BUFFER_SIZE: usize = 100;

/// Command sent from web handlers to the TwitchClient event loop.
#[derive(Debug)]
pub enum ControlCommand {
    /// Switch to a different Twitch channel.
    SwitchChannel(String),
}

/// Shared state for the web server, accessible by all route handlers.
struct WebState {
    /// Broadcast sender that WebSocket handlers subscribe to.
    /// Each new WebSocket connection calls `subscribe()` to get its own receiver.
    event_tx: broadcast::Sender<WebEvent>,
    /// The Twitch channel name (without #), mutable to support runtime switching.
    channel: RwLock<String>,
    /// Ring buffer of recent events for replay on new connections.
    recent_events: RwLock<VecDeque<String>>,
    /// Sender for control commands to the TwitchClient event loop.
    control_tx: mpsc::Sender<ControlCommand>,
}

/// Response from the `/api/config` endpoint.
#[derive(Serialize)]
struct ConfigResponse {
    /// The Twitch channel name for the player embed.
    channel: String,
}

/// Request body for the `POST /api/channel` endpoint.
#[derive(Deserialize)]
struct SwitchChannelRequest {
    /// The new Twitch channel name to switch to (without #).
    channel: String,
}

/// Starts the web UI server as a background task.
///
/// Spawns an Axum HTTP server on the given port that serves the embedded
/// HTML UI and handles WebSocket connections for live event streaming.
///
/// # Arguments
///
/// * `port` - TCP port to listen on (e.g., 3000).
/// * `event_tx` - Broadcast sender that produces [`WebEvent`]s from the main event loop.
/// * `channel` - Twitch channel name for the player embed.
/// * `control_tx` - Sender for forwarding control commands to the TwitchClient event loop.
pub fn start_web_server(
    port: u16,
    event_tx: broadcast::Sender<WebEvent>,
    channel: String,
    control_tx: mpsc::Sender<ControlCommand>,
) {
    let state = Arc::new(WebState {
        event_tx: event_tx.clone(),
        channel: RwLock::new(channel),
        recent_events: RwLock::new(VecDeque::with_capacity(REPLAY_BUFFER_SIZE)),
        control_tx,
    });

    // Background task: buffer recent events for replay on new connections
    let buffer_state = Arc::clone(&state);
    let mut buffer_rx = event_tx.subscribe();
    tokio::spawn(async move {
        loop {
            match buffer_rx.recv().await {
                Ok(event) => {
                    if let Ok(json) = serde_json::to_string(&event) {
                        let mut buf = buffer_state.recent_events.write().await;
                        if buf.len() >= REPLAY_BUFFER_SIZE {
                            buf.pop_front();
                        }
                        buf.push_back(json);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let app = Router::new()
        .route("/", get(serve_ui))
        .route("/ws", get(ws_handler))
        .route("/api/config", get(config_handler))
        .route("/api/channel", post(switch_channel_handler))
        .with_state(state);

    tokio::spawn(async move {
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        info!("Web UI available at http://localhost:{}", port);

        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                if let Err(e) = axum::serve(listener, app).await {
                    warn!("Web server error: {}", e);
                }
            }
            Err(e) => {
                warn!("Failed to bind web server on port {}: {}", port, e);
            }
        }
    });
}

/// Serves the embedded HTML UI.
///
/// The UI is compiled into the binary via `include_str!`, making the
/// binary fully self-contained with no external file dependencies.
async fn serve_ui() -> Html<&'static str> {
    Html(include_str!("ui.html"))
}

/// Returns the current configuration as JSON for the UI.
///
/// The web UI fetches this on page load to configure the Twitch
/// player embed with the correct channel name.
async fn config_handler(State(state): State<Arc<WebState>>) -> Json<ConfigResponse> {
    let channel = state.channel.read().await.clone();
    Json(ConfigResponse { channel })
}

/// Handles channel switch requests from the web UI.
///
/// Validates the channel name, updates the shared state, clears the
/// replay buffer, broadcasts a [`WebEvent::ChannelChanged`] event to
/// all connected WebSocket clients, and sends the switch command to
/// the TwitchClient event loop via the control channel.
async fn switch_channel_handler(
    State(state): State<Arc<WebState>>,
    Json(body): Json<SwitchChannelRequest>,
) -> Result<Json<ConfigResponse>, StatusCode> {
    let new_channel = body.channel.trim().to_lowercase();

    // Validate: non-empty, alphanumeric + underscores, max 25 chars (Twitch rules)
    if new_channel.is_empty()
        || new_channel.len() > 25
        || !new_channel
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Skip if already on this channel
    {
        let current = state.channel.read().await;
        if *current == new_channel {
            return Ok(Json(ConfigResponse {
                channel: new_channel,
            }));
        }
    }

    // Update the shared channel name
    {
        let mut ch = state.channel.write().await;
        *ch = new_channel.clone();
    }

    // Clear the replay buffer so new clients don't see old channel's messages
    {
        let mut buf = state.recent_events.write().await;
        buf.clear();
    }

    // Send the switch command to the TwitchClient event loop.
    // The TwitchClient broadcasts ChannelChanged after completing the PART/JOIN.
    if state
        .control_tx
        .send(ControlCommand::SwitchChannel(new_channel.clone()))
        .await
        .is_err()
    {
        warn!("Failed to send channel switch command to TwitchClient");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    info!("Channel switch requested: #{}", new_channel);
    Ok(Json(ConfigResponse {
        channel: new_channel,
    }))
}

/// Handles WebSocket upgrade requests.
///
/// Subscribes to the broadcast channel, snapshots the replay buffer,
/// and upgrades the connection. The client receives recent history
/// followed by live events.
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<WebState>>) -> impl IntoResponse {
    // Subscribe before snapshotting to avoid a gap between replay and live
    let rx = state.event_tx.subscribe();
    let replay: Vec<String> = state.recent_events.read().await.iter().cloned().collect();
    ws.on_upgrade(move |socket| handle_ws_connection(socket, rx, replay))
}

/// Manages an individual WebSocket connection.
///
/// First sends the replay buffer (recent history) so the client catches
/// up on messages it missed while away. Then reads live events from the
/// broadcast receiver and forwards each one as a JSON text frame. The
/// connection is read-only from the client's perspective; any incoming
/// messages are ignored. The loop exits when the client disconnects or
/// the broadcast channel closes.
async fn handle_ws_connection(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<WebEvent>,
    replay: Vec<String>,
) {
    // Send recent history so the client catches up
    for json in replay {
        if socket.send(Message::Text(json)).await.is_err() {
            return;
        }
    }

    // Stream live events
    loop {
        match rx.recv().await {
            Ok(event) => {
                let json = match serde_json::to_string(&event) {
                    Ok(j) => j,
                    Err(e) => {
                        warn!("Failed to serialize WebEvent: {}", e);
                        continue;
                    }
                };
                if socket.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("WebSocket client lagged, skipped {} events", n);
            }
            Err(broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }
}
