//! Slack Socket Mode: interactions arrive over a WebSocket that this process opens outbound,
//! so the machine needs no public URL or inbound port.
//!
//! Protocol: `apps.connections.open` (with the app-level token) returns a one-time `wss://`
//! URL. Slack then sends JSON envelopes; each one carrying an `envelope_id` must be
//! acknowledged by sending `{"envelope_id": …}` back, optionally with a `payload`. Slack
//! periodically asks the client to reconnect by sending `{"type": "disconnect"}`.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::{SlackClient, interactions};
use crate::engine::Engine;

const PING_EVERY: Duration = Duration::from_secs(30);
/// Reconnect if nothing at all (not even a pong) has arrived for this long.
const SILENCE_LIMIT: Duration = Duration::from_secs(90);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
enum SocketError {
    #[error(transparent)]
    Slack(#[from] super::SlackError),
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("connection went silent")]
    Silent,
}

/// Keeps a Socket Mode connection open until shutdown, reconnecting with backoff.
pub async fn run(engine: Engine, client: SlackClient, shutdown: CancellationToken) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let outcome = tokio::select! {
            _ = shutdown.cancelled() => return,
            outcome = connection(&engine, &client, &shutdown) => outcome,
        };
        if shutdown.is_cancelled() {
            return;
        }
        match outcome {
            Ok(()) => tracing::info!("Slack asked for a Socket Mode reconnect"),
            Err(err) => tracing::warn!(error = %err, "Slack Socket Mode connection lost"),
        }
        // A connection that lasted a while was healthy: start the backoff over.
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Runs one connection. `Ok` means Slack closed it deliberately and we should reconnect.
async fn connection(engine: &Engine, client: &SlackClient, shutdown: &CancellationToken) -> Result<(), SocketError> {
    let url = client.open_socket_url().await?;
    let (socket, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let (mut sink, mut stream) = socket.split();
    let (acks, mut pending_acks) = mpsc::unbounded_channel::<String>();

    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_heard = Instant::now();

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = sink.send(Message::Close(None)).await;
                return Ok(());
            }
            Some(ack) = pending_acks.recv() => sink.send(Message::text(ack)).await?,
            _ = ping.tick() => {
                if last_heard.elapsed() > SILENCE_LIMIT {
                    return Err(SocketError::Silent);
                }
                sink.send(Message::Ping(Default::default())).await?;
            }
            frame = stream.next() => {
                let Some(frame) = frame else { return Ok(()) };
                last_heard = Instant::now();
                match frame? {
                    Message::Text(text) => {
                        let Ok(envelope) = serde_json::from_str::<Value>(text.as_str()) else {
                            tracing::warn!("ignoring a Socket Mode frame that is not JSON");
                            continue;
                        };
                        if !dispatch(engine, envelope, &acks) {
                            return Ok(());
                        }
                    }
                    Message::Ping(data) => sink.send(Message::Pong(data)).await?,
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}

/// Handles one envelope. Returns `false` when Slack wants the connection replaced.
fn dispatch(engine: &Engine, envelope: Value, acks: &mpsc::UnboundedSender<String>) -> bool {
    match envelope.get("type").and_then(Value::as_str) {
        Some("hello") => {
            tracing::info!("Slack Socket Mode connected");
            true
        }
        Some("disconnect") => false,
        kind => {
            let Some(envelope_id) = envelope.get("envelope_id").and_then(Value::as_str).map(str::to_string) else {
                return true;
            };
            if kind != Some("interactive") {
                // Events and slash commands are not used, but must still be acknowledged.
                let _ = acks.send(json!({ "envelope_id": envelope_id }).to_string());
                return true;
            }
            // Handle each interaction on its own task so a slow one cannot delay the others'
            // acknowledgements (Slack allows three seconds).
            let (engine, acks) = (engine.clone(), acks.clone());
            tokio::spawn(async move {
                let payload = envelope.get("payload").cloned().unwrap_or(Value::Null);
                let mut ack = json!({ "envelope_id": envelope_id });
                if let Some(reply) = interactions::handle(&engine, &payload).await {
                    ack["payload"] = reply;
                }
                let _ = acks.send(ack.to_string());
            });
            true
        }
    }
}
