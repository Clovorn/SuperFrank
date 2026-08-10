//! Task change stream — WebSocket endpoint backed by Postgres LISTEN/NOTIFY
//!
//! Clients connect to /api/v1/admin/tasks/stream via WebSocket.
//! The gateway listens on the 'task_change' Postgres channel and relays
//! every INSERT/UPDATE/DELETE notification to all connected clients.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use serde_json::json;
use sqlx::postgres::PgListener;
use tracing::{info, warn};

use crate::AppState;

/// WebSocket upgrade handler — registered at GET /api/v1/admin/tasks/stream
pub async fn task_stream_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Per-connection handler: open a PgListener, relay task_change notifications.
async fn handle_socket(mut socket: WebSocket, state: AppState) {
    info!("Task stream: client connected");

    // Each connection gets its own PgListener (own DB connection)
    let mut listener = match PgListener::connect_with(&state.db).await {
        Ok(l) => l,
        Err(e) => {
            warn!("Task stream: failed to create PgListener: {}", e);
            let _ = socket
                .send(Message::Text(
                    json!({ "error": "failed to connect to notification channel" }).to_string(),
                ))
                .await;
            return;
        }
    };

    if let Err(e) = listener.listen("task_change").await {
        warn!("Task stream: failed to LISTEN on task_change: {}", e);
        let _ = socket
            .send(Message::Text(
                json!({ "error": "failed to subscribe to task_change channel" }).to_string(),
            ))
            .await;
        return;
    }

    // Send a connected acknowledgement
    let _ = socket
        .send(Message::Text(
            json!({ "type": "connected", "channel": "task_change" }).to_string(),
        ))
        .await;

    // Relay loop — receive Postgres notifications and forward to the WebSocket client.
    // Also listens for client close/ping frames so we don't hold the connection open
    // after the browser disconnects.
    loop {
        tokio::select! {
            // Postgres notification arrived
            notify_result = listener.recv() => {
                match notify_result {
                    Ok(notification) => {
                        let payload = notification.payload().to_string();
                        let msg = json!({
                            "type": "task_change",
                            "data": serde_json::from_str::<serde_json::Value>(&payload)
                                .unwrap_or_else(|_| serde_json::Value::String(payload.clone()))
                        });
                        if socket.send(Message::Text(msg.to_string())).await.is_err() {
                            info!("Task stream: client disconnected (send failed)");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Task stream: PgListener error: {}", e);
                        let _ = socket
                            .send(Message::Text(
                                json!({ "type": "error", "message": e.to_string() }).to_string(),
                            ))
                            .await;
                        break;
                    }
                }
            }

            // Client sent a frame (close, ping, pong, or keepalive text)
            client_msg = socket.recv() => {
                match client_msg {
                    Some(Ok(Message::Close(_))) | None => {
                        info!("Task stream: client closed connection");
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        // Respond to pings to keep the connection alive
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    Some(Err(e)) => {
                        warn!("Task stream: client recv error: {}", e);
                        break;
                    }
                    _ => {} // Text/Binary from client — ignore for now
                }
            }
        }
    }

    info!("Task stream: connection closed, PgListener released");
    // listener drops here, releasing the DB connection automatically
}
