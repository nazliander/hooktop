//! WebSocket protocol.
//!
//! Server → client (`type` field):
//! - `hello`     — version and the session list, on connect
//! - `snapshot`  — the full graph of the subscribed session and its `seq`
//! - `deltas`    — changes since the last message; apply in order
//! - `sessions`  — updated list rows for sessions that changed
//! - `resync`    — the client fell behind; a fresh `snapshot` follows
//!
//! Client → server: `{"type":"subscribe","session_id":"…"}` switches the
//! subscribed session (a snapshot follows), `{"type":"unsubscribe"}` stops
//! deltas but keeps list updates.
//!
//! Updates are batched and flushed every 16 ms, so a burst of parallel tool
//! calls reaches a client as one frame.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use hooktop_core::{Delta, SessionGraph, SessionSummary};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::hub::Hub;

const FLUSH_EVERY: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireFormat {
    #[default]
    Json,
    Msgpack,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg<'a> {
    Hello {
        version: &'a str,
        sessions: Vec<SessionSummary>,
    },
    Snapshot {
        session_id: &'a str,
        seq: u64,
        graph: &'a SessionGraph,
    },
    Deltas {
        session_id: &'a str,
        seq: u64,
        deltas: &'a [Delta],
    },
    Sessions {
        sessions: Vec<&'a SessionSummary>,
    },
    /// Sessions that were deleted.
    Removed {
        session_ids: Vec<String>,
    },
    Resync,
    Error {
        message: &'a str,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Subscribe {
        session_id: String,
    },
    Unsubscribe,
    /// Delete these sessions everywhere.
    Remove {
        session_ids: Vec<String>,
    },
    /// Delete closed sessions, and open ones quiet for `quiet_ms` with no
    /// known live process.
    RemoveStale {
        quiet_ms: i64,
    },
}

/// The subscribed session and the last seq the client has seen for it
/// (`None` until a snapshot was sent, e.g. the session doesn't exist yet).
struct Sub {
    session_id: String,
    seq: Option<u64>,
}

/// `auth` is the server's tokens and the one this client presented: once
/// that one stops being valid (a rotation), the socket is closed.
pub async fn run(
    socket: WebSocket,
    hub: Arc<Hub>,
    initial: Option<String>,
    fmt: WireFormat,
    auth: Option<(Arc<crate::auth::Tokens>, String)>,
) {
    let (mut sink, mut stream) = socket.split();
    // Subscribe before taking any snapshot so nothing falls in between.
    let mut rx = hub.subscribe();

    macro_rules! send {
        ($msg:expr) => {
            if sink.send(encode(fmt, &$msg)).await.is_err() {
                return;
            }
        };
    }

    send!(ServerMsg::Hello {
        version: crate::VERSION,
        sessions: hub.sessions()
    });

    let mut sub: Option<Sub> = None;
    if let Some(id) = initial {
        sub = Some(Sub {
            session_id: id,
            seq: None,
        });
    }
    // Snapshot for the initial subscription (if the session exists yet).
    if let Some(s) = &mut sub
        && let Some((seq, graph)) = hub.snapshot(&s.session_id)
    {
        s.seq = Some(seq);
        send!(ServerMsg::Snapshot {
            session_id: &s.session_id,
            seq,
            graph: &graph
        });
    }

    let mut deltas: Vec<Delta> = Vec::new();
    let mut deltas_seq = 0;
    let mut summaries: HashMap<String, SessionSummary> = HashMap::new();
    let mut removed: Vec<String> = Vec::new();
    let mut recheck = tokio::time::interval(Duration::from_secs(1));
    let mut tick = tokio::time::interval(FLUSH_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            update = rx.recv() => match update {
                Ok(u) if u.removed => {
                    summaries.remove(&u.session_id);
                    removed.push(u.session_id.clone());
                    if let Some(s) = &mut sub && s.session_id == u.session_id {
                        deltas.clear();
                        s.seq = None;
                    }
                }
                Ok(u) => {
                    summaries.insert(u.session_id.clone(), u.summary.clone());
                    if let Some(s) = &sub && s.session_id == u.session_id && s.seq.is_some_and(|seen| u.seq > seen) {
                        deltas.extend(u.deltas.iter().cloned());
                        deltas_seq = u.seq;
                    }
                }
                Err(RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "websocket client lagged; resyncing");
                    deltas.clear();
                    send!(ServerMsg::Resync);
                    if let Some(s) = &mut sub && let Some((seq, graph)) = hub.snapshot(&s.session_id) {
                        s.seq = Some(seq);
                        send!(ServerMsg::Snapshot { session_id: &s.session_id, seq, graph: &graph });
                    }
                    send!(ServerMsg::Hello { version: crate::VERSION, sessions: hub.sessions() });
                }
                Err(RecvError::Closed) => return,
            },

            _ = recheck.tick() => {
                if let Some((tokens, presented)) = &auth && !tokens.accepts(presented, false) {
                    tracing::info!("closing a WebSocket opened with a token that was rotated");
                    send!(ServerMsg::Error { message: "hooktop's token was rotated; reconnect with the new one" });
                    let _ = sink.close().await;
                    return;
                }
            },

            _ = tick.tick() => {
                if !removed.is_empty() {
                    send!(ServerMsg::Removed { session_ids: std::mem::take(&mut removed) });
                }
                if !summaries.is_empty() {
                    send!(ServerMsg::Sessions { sessions: summaries.values().collect() });
                    summaries.clear();
                }
                if let Some(s) = &mut sub && !deltas.is_empty() {
                    send!(ServerMsg::Deltas { session_id: &s.session_id, seq: deltas_seq, deltas: &deltas });
                    s.seq = Some(deltas_seq);
                    deltas.clear();
                } else if let Some(s) = &mut sub && s.seq.is_none() {
                    // Subscribed before the session existed: snapshot once it does.
                    if let Some((seq, graph)) = hub.snapshot(&s.session_id) {
                        s.seq = Some(seq);
                        send!(ServerMsg::Snapshot { session_id: &s.session_id, seq, graph: &graph });
                    }
                }
            },

            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMsg>(&text) {
                    Ok(ClientMsg::Subscribe { session_id }) => {
                        deltas.clear();
                        let mut s = Sub { session_id, seq: None };
                        if let Some((seq, graph)) = hub.snapshot(&s.session_id) {
                            s.seq = Some(seq);
                            send!(ServerMsg::Snapshot { session_id: &s.session_id, seq, graph: &graph });
                        }
                        sub = Some(s);
                    }
                    Ok(ClientMsg::Remove { session_ids }) => {
                        let gone = hub.remove(&session_ids);
                        tracing::info!(count = gone.len(), "removed sessions");
                    }
                    Ok(ClientMsg::RemoveStale { quiet_ms }) => {
                        let ids = hub.stale(crate::now_ms(), quiet_ms.max(0));
                        let gone = hub.remove(&ids);
                        tracing::info!(count = gone.len(), "removed stale sessions");
                    }
                    Ok(ClientMsg::Unsubscribe) => {
                        deltas.clear();
                        sub = None;
                    }
                    Err(e) => send!(ServerMsg::Error { message: &format!("bad message: {e}") }),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {} // pings are answered by axum; binary is ignored
            },
        }
    }
}

fn encode(fmt: WireFormat, msg: &ServerMsg<'_>) -> Message {
    match fmt {
        WireFormat::Json => Message::Text(serde_json::to_string(msg).unwrap_or_default().into()),
        WireFormat::Msgpack => Message::Binary(rmp_serde::to_vec_named(msg).unwrap_or_default().into()),
    }
}
