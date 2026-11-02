//! The TUI's connection to a hooktop server over `/ws`, with automatic
//! reconnects.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hooktop_core::{Delta, SessionGraph, SessionSummary};
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// Everything the server can send (see `ws.rs`).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello {
        version: String,
        sessions: Vec<SessionSummary>,
    },
    Snapshot {
        session_id: String,
        #[allow(dead_code)]
        seq: u64,
        graph: Box<SessionGraph>,
    },
    Deltas {
        session_id: String,
        #[allow(dead_code)]
        seq: u64,
        deltas: Vec<Delta>,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    Removed {
        session_ids: Vec<String>,
    },
    Resync,
    Error {
        message: String,
    },
}

#[derive(Debug)]
pub enum Net {
    Up,
    Down(String),
    Msg(Box<ServerMsg>),
}

/// The handshake request, with the token.
fn request(
    url: &str,
    token: Option<&str>,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    if let Some(t) = token {
        let v = format!("Bearer {t}")
            .parse()
            .map_err(|_| "the token has characters a header can't hold".to_string())?;
        req.headers_mut().insert("authorization", v);
    }
    Ok(req)
}

pub fn subscribe_msg(session_id: &str) -> String {
    serde_json::json!({ "type": "subscribe", "session_id": session_id }).to_string()
}

pub fn remove_msg(session_id: &str) -> String {
    serde_json::json!({ "type": "remove", "session_ids": [session_id] }).to_string()
}

pub fn remove_stale_msg(quiet_ms: i64) -> String {
    serde_json::json!({ "type": "remove_stale", "quiet_ms": quiet_ms }).to_string()
}

/// Connect (and keep reconnecting) to `url`. Server messages arrive on
/// `events` wrapped by `wrap`; strings sent on the returned channel go to
/// the server. Messages queued while disconnected are dropped: the app
/// resubscribes on [`Net::Up`].
pub fn spawn<E: Send + 'static>(
    url: String,
    token: Option<std::sync::Arc<crate::auth::Tokens>>,
    events: UnboundedSender<E>,
    wrap: fn(Net) -> E,
) -> UnboundedSender<String> {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(200);
        loop {
            // Read the token on every connect, so a rotation is followed.
            let current = token.as_ref().map(|t| t.current());
            let request = match request(&url, current.as_deref()) {
                Ok(r) => r,
                Err(e) => {
                    let _ = events.send(wrap(Net::Down(e)));
                    return;
                }
            };
            let reason = match connect_async(request).await {
                Ok((ws, _)) => {
                    backoff = Duration::from_millis(200);
                    while out_rx.try_recv().is_ok() {}
                    if events.send(wrap(Net::Up)).is_err() {
                        return;
                    }
                    let (mut sink, mut stream) = ws.split();
                    loop {
                        tokio::select! {
                            incoming = stream.next() => match incoming {
                                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMsg>(text.as_str()) {
                                    Ok(msg) => {
                                        if events.send(wrap(Net::Msg(Box::new(msg)))).is_err() {
                                            return;
                                        }
                                    }
                                    Err(e) => tracing::warn!("unreadable server message: {e}"),
                                },
                                Some(Ok(Message::Close(_))) | None => break "the server closed the connection".to_string(),
                                Some(Err(e)) => break e.to_string(),
                                Some(Ok(_)) => {}
                            },
                            outgoing = out_rx.recv() => match outgoing {
                                Some(text) => {
                                    if let Err(e) = sink.send(Message::Text(text.into())).await {
                                        break e.to_string();
                                    }
                                }
                                None => return,
                            },
                        }
                    }
                }
                Err(tokio_tungstenite::tungstenite::Error::Http(r)) if r.status() == 401 => {
                    "hooktop refused the token: the server uses a different one (check HOOKTOP_TOKEN)".to_string()
                }
                Err(e) => e.to_string(),
            };
            tracing::debug!("websocket down: {reason}");
            if events.send(wrap(Net::Down(reason))).is_err() {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    });
    out_tx
}
