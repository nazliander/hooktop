//! Starting the hooktop server: open the store, restore sessions, bind,
//! serve. Shared by `hooktop serve` and the TUI, which runs an embedded
//! server when none is already listening.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::store::{self, Store};
use crate::{AppState, Hub, demo, router};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// SQLite event log; `None` uses the default location.
    pub db: Option<PathBuf>,
    /// Keep everything in memory only.
    pub no_store: bool,
    /// The token every request must carry; `None` turns auth off
    /// (`--no-auth`).
    pub token: Option<std::sync::Arc<crate::auth::Tokens>>,
    /// Allow listening on a non-loopback address.
    pub allow_remote: bool,
    /// Stop after this long with no accepted requests and no open
    /// WebSockets; `None` runs until stopped.
    pub idle_timeout: Option<Duration>,
    /// Mark connections Closed after this long with no activity; `None`
    /// leaves them open until Claude Code ends them.
    pub close_after: Option<Duration>,
    pub demo: bool,
    pub replay: Option<PathBuf>,
    pub speed: f64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: crate::DEFAULT_PORT,
            db: None,
            no_store: false,
            token: None,
            allow_remote: false,
            idle_timeout: None,
            close_after: Some(Duration::from_secs(3600)),
            demo: false,
            replay: None,
            speed: 1.0,
        }
    }
}

/// A server that is bound and serving in the background.
pub struct RunningServer {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
    idle: Arc<tokio::sync::Notify>,
}

impl RunningServer {
    /// Resolves once the server has gone unused for its idle timeout
    /// (never, without one).
    pub async fn idle(&self) {
        self.idle.notified().await;
    }

    /// Stop accepting connections, finish in-flight requests, flush the store.
    pub async fn stop(mut self) -> Result<()> {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        self.task.await.context("server task panicked")?
    }
}

/// Open the store, restore sessions, bind the port and start serving.
/// Binding happens before this returns, so "port in use" is reported here.
pub async fn start(cfg: ServerConfig) -> Result<RunningServer> {
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port)
        .parse()
        .context("invalid host/port")?;
    let remote = !addr.ip().is_loopback();
    if remote && !cfg.allow_remote {
        anyhow::bail!(
            "refusing to listen on {}: that's reachable from other machines. Use 127.0.0.1, or pass --allow-remote if you mean it",
            addr.ip()
        );
    }
    if remote && cfg.token.is_none() {
        anyhow::bail!("listening beyond this machine needs a token; drop --no-auth");
    }
    if cfg.token.is_none() {
        tracing::warn!(
            "authentication is off (--no-auth): any local process or web page can read and change hooktop's data"
        );
    }

    let (store, db_path) = if cfg.no_store {
        (None, None)
    } else {
        let path = cfg.db.clone().unwrap_or_else(store::default_db_path);
        if let Some(dir) = path.parent() {
            crate::auth::private_dir(dir)?;
        }
        let (store, existing) = Store::open(&path)?;
        // Prompts and tool output can hold secrets: keep them to this user.
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.clone().into_os_string();
            p.push(suffix);
            crate::auth::restrict(std::path::Path::new(&p));
        }
        tracing::info!(events = existing.len(), db = %path.display(), "opened event store");
        (Some((store, existing)), Some(path))
    };

    let hub = Arc::new(Hub::new(store.as_ref().map(|(s, _)| s.writer.clone())));
    let writer = store.map(|(s, existing)| {
        let started = std::time::Instant::now();
        let n = hub.restore(existing);
        if n > 0 {
            tracing::info!(events = n, sessions = hub.session_count(), took = ?started.elapsed(), "restored sessions");
        }
        s
    });

    crate::procs::spawn_sampler(Arc::downgrade(&hub));
    if cfg.demo {
        tokio::spawn(demo::run_demo(hub.clone(), cfg.speed, true));
    }
    if let Some(path) = cfg.replay.clone() {
        let hub = hub.clone();
        let speed = cfg.speed;
        tokio::spawn(async move {
            if let Err(e) = demo::replay_file(hub, &path, speed).await {
                tracing::error!("replay failed: {e:#}");
            }
        });
    }

    if let Some(quiet) = cfg.close_after.filter(|d| !d.is_zero()) {
        let hub = Arc::downgrade(&hub);
        let quiet_ms = quiet.as_millis() as i64;
        tokio::spawn(async move {
            let every = (quiet / 4).clamp(Duration::from_millis(50), Duration::from_secs(60));
            loop {
                // Sweep straight away too: sessions restored from disk may
                // have been quiet for days.
                let Some(hub) = hub.upgrade() else { return };
                let closed = hub.close_quiet(crate::now_ms(), quiet_ms);
                if !closed.is_empty() {
                    tracing::info!(count = closed.len(), "closed quiet connections");
                }
                drop(hub);
                tokio::time::sleep(every).await;
            }
        });
    }

    let mut state = AppState::new(hub, db_path, cfg.token.clone());
    let idle = Arc::new(tokio::sync::Notify::new());
    if let Some(limit) = cfg.idle_timeout.filter(|d| !d.is_zero()) {
        let activity = state.activity.clone();
        let hub = state.hub.clone();
        let idle = idle.clone();
        let limit_ms = limit.as_millis() as i64;
        tokio::spawn(async move {
            let every = (limit / 4).clamp(Duration::from_millis(50), Duration::from_secs(30));
            loop {
                tokio::time::sleep(every).await;
                if hub.any_working() {
                    activity.touch();
                } else if activity.idle_ms(crate::now_ms()) >= limit_ms {
                    tracing::info!("no use for {}; stopping", hooktop_core::view::duration(limit_ms));
                    idle.notify_one();
                    return;
                }
            }
        });
    }
    if remote {
        state.allowed_hosts.push(cfg.host.clone());
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let addr = listener.local_addr()?;
    tracing::info!("hooktop listening on http://{addr}  (hook url: http://{addr}/hook)");

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
            tracing::info!("shutting down");
        })
        .await?;
        if let Some(store) = writer {
            store.writer.shutdown(Duration::from_secs(3));
        }
        Ok(())
    });
    Ok(RunningServer {
        addr,
        stop: Some(stop_tx),
        task,
        idle,
    })
}

/// Is something already listening on this local port?
pub fn port_in_use(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}
