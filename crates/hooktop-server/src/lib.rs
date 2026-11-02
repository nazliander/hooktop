//! The hooktop server: ingests Claude Code hook events, keeps a live task
//! graph per session, persists the raw event log, and streams deltas to
//! the terminal UI (or any other client) over WebSocket.

pub mod auth;
pub mod demo;
pub mod hooks;
pub mod http;
pub mod hub;
pub mod procs;
pub mod server;
pub mod store;
pub mod transcripts;
pub mod tui;
mod ws;

pub use http::{AppState, router};
pub use hub::Hub;

/// Default port; the hook URL is `http://127.0.0.1:4317/hook`.
pub const DEFAULT_PORT: u16 = 4317;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// `0.1.0+<git sha>`: the version shown in the UI and `/api/health`.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("HOOKTOP_BUILD"));
