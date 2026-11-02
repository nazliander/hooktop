//! HTTP routes.
//!
//! | Method | Path | Purpose |
//! |---|---|---|
//! | POST | `/hook` | Claude Code `http` hook endpoint |
//! | GET | `/api/health` | Liveness + counts |
//! | GET | `/api/sessions` | Session list, newest first |
//! | GET | `/api/sessions/{id}` | Full graph snapshot |
//! | GET | `/api/sessions/{id}/events` | Raw stored events (`?after=&limit=`) |
//! | GET | `/ws` | Live snapshot + deltas (`?session=&format=json\|msgpack`) |

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hooktop_core::HookPayload;
use serde::Deserialize;
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::hub::Hub;
use crate::ws::{self, WireFormat};
use crate::{now_ms, store};

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    /// Where the event log lives; `None` when running without persistence.
    pub db_path: Option<PathBuf>,
    /// When set, every request needs `Authorization: Bearer <token>`
    /// (see [`crate::auth`]). `None` only with `--no-auth` and in tests.
    pub token: Option<Arc<crate::auth::Tokens>>,
    /// Host names accepted besides localhost (the `--host` name, when
    /// listening beyond loopback with `--allow-remote`).
    pub allowed_hosts: Vec<String>,
    /// When hooktop was last used, for `--idle-timeout`.
    pub activity: Arc<Activity>,
}

/// Use of the server: the last accepted request, and open WebSockets.
/// Refused requests don't count, so probing can't keep hooktop running.
#[derive(Debug)]
pub struct Activity {
    last_ms: std::sync::atomic::AtomicI64,
    open_sockets: std::sync::atomic::AtomicUsize,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            last_ms: std::sync::atomic::AtomicI64::new(now_ms()),
            open_sockets: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl Activity {
    pub fn touch(&self) {
        self.last_ms.fetch_max(now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// How long nothing has used hooktop; zero while a WebSocket is open.
    pub fn idle_ms(&self, now: i64) -> i64 {
        use std::sync::atomic::Ordering::Relaxed;
        if self.open_sockets.load(Relaxed) > 0 {
            return 0;
        }
        (now - self.last_ms.load(Relaxed)).max(0)
    }

    /// Count an open WebSocket until the returned guard is dropped.
    pub fn socket(self: &Arc<Self>) -> SocketGuard {
        self.open_sockets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SocketGuard(self.clone())
    }
}

pub struct SocketGuard(Arc<Activity>);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        self.0.open_sockets.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        self.0.touch();
    }
}

impl AppState {
    pub fn new(hub: Arc<Hub>, db_path: Option<PathBuf>, token: Option<Arc<crate::auth::Tokens>>) -> Self {
        Self {
            hub,
            db_path,
            token,
            allowed_hosts: Vec::new(),
            activity: Arc::default(),
        }
    }
}

/// Tool responses (a `Read` of a big file, say) can be large.
const MAX_HOOK_BODY: usize = 32 * 1024 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/hook", post(hook).layer(DefaultBodyLimit::max(MAX_HOOK_BODY)))
        .route("/api/health", get(health))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}", get(get_session))
        .route("/api/sessions/{id}/events", get(get_events))
        .route("/ws", get(websocket))
        .layer(axum::middleware::from_fn_with_state(state.clone(), crate::auth::guard))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// The client's address, when the server was started with connect info
/// (tests calling the router directly have none).
struct Peer(Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Peer(parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0)))
    }
}

/// Claude Code's hook endpoint. [`crate::auth::guard`] has already checked
/// the token, host, origin and content type.
async fn hook(State(st): State<AppState>, Peer(peer): Peer, body: Bytes) -> Response {
    let payload: HookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("rejected hook payload: {e}");
            return (StatusCode::BAD_REQUEST, format!("invalid hook payload: {e}")).into_response();
        }
    };
    tracing::debug!(session = %payload.session_id, event = %payload.hook_event_name, "hook");
    if payload.hook_event_name == hooktop_core::SAMPLE_HOOK {
        return (StatusCode::BAD_REQUEST, "HooktopSample is recorded by hooktop itself").into_response();
    }
    // Find the Claude Code process while its connection is still open, so
    // CPU and RAM can be sampled. Once per session; a few ms on Linux.
    if let Some(peer) = peer
        && payload.hook_event_name != "SessionEnd"
        && st.hub.procs().needs_lookup(&payload.session_id)
    {
        let pid = tokio::task::spawn_blocking(move || crate::procs::pid_for_peer(peer))
            .await
            .ok()
            .flatten();
        tracing::debug!(session = %payload.session_id, ?pid, "looked up the hook sender");
        st.hub.procs().record(&payload.session_id, pid);
    }
    st.hub.ingest(payload, now_ms(), true);
    // `{}` is a valid hook response that changes nothing: hooktop only observes.
    Json(json!({})).into_response()
}

async fn health(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "version": crate::VERSION,
        "sessions": st.hub.session_count(),
        "persistent": st.db_path.is_some(),
    }))
}

async fn list_sessions(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.hub.sessions())
}

async fn get_session(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.hub.snapshot(&id) {
        Some((seq, graph)) => Json(json!({ "seq": seq, "graph": graph })).into_response(),
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: u64,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    500
}

async fn get_events(State(st): State<AppState>, Path(id): Path<String>, Query(q): Query<EventsQuery>) -> Response {
    let Some(path) = st.db_path.clone() else {
        return (StatusCode::NOT_FOUND, "this server runs without an event store").into_response();
    };
    let limit = q.limit.clamp(1, 5000);
    let res = tokio::task::spawn_blocking(move || store::read_events(&path, &id, q.after, limit)).await;
    match res {
        Ok(Ok(rows)) => Json(rows).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct WsQuery {
    session: Option<String>,
    #[serde(default)]
    format: WireFormat,
}

async fn websocket(
    State(st): State<AppState>,
    Query(q): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Remember which token opened the socket, so a rotation can close it.
    let auth = st.token.clone().zip(crate::auth::bearer(&headers).map(str::to_string));
    let open = st.activity.socket();
    upgrade.on_upgrade(move |socket| async move {
        ws::run(socket, st.hub, q.session, q.format, auth).await;
        drop(open);
    })
}
