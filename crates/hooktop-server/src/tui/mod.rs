//! `hooktop` in a terminal: Watcher's party, live, in ratatui.
//!
//! The TUI is a client of the server's `/ws` feed. If a hooktop
//! server is already listening on the port it attaches to it; otherwise it
//! runs one in-process for as long as the TUI is open, so hooks keep
//! arriving.

pub mod app;
pub mod client;
pub mod dag;
pub mod theme;
pub mod ui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as TermEvent};
use tokio::sync::mpsc;

use crate::server::{self, ServerConfig};
use app::{App, Mode};
use client::Net;
use theme::ThemeName;

#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub port: u16,
    pub db: Option<PathBuf>,
    pub no_store: bool,
    pub demo: bool,
    pub speed: f64,
    pub theme: Option<ThemeName>,
    /// The token for the server; `None` only with `--no-auth`.
    pub token: Option<std::sync::Arc<crate::auth::Tokens>>,
    /// See [`ServerConfig::close_after`].
    pub close_after: Option<Duration>,
    /// Prefixes of automated prompts, from `--automated-prompt`.
    pub automated_prompts: Vec<String>,
}

enum AppEvent {
    Term(TermEvent),
    Net(Net),
}

pub async fn run(cfg: TuiConfig) -> Result<()> {
    let (mode, embedded) = if server::port_in_use(cfg.port) {
        (Mode::Attached, None)
    } else {
        let s = server::start(ServerConfig {
            port: cfg.port,
            db: cfg.db.clone(),
            no_store: cfg.no_store,
            demo: cfg.demo,
            speed: cfg.speed,
            token: cfg.token.clone(),
            close_after: cfg.close_after,
            ..ServerConfig::default()
        })
        .await?;
        (Mode::Embedded, Some(s))
    };

    let mut app = App::new(cfg.port, mode, cfg.theme.unwrap_or_else(ThemeName::detect));
    if mode == Mode::Attached && cfg.demo {
        app.say(format!(
            "A hooktop server was already running on :{}, so --demo was ignored",
            cfg.port
        ));
    }

    // Prices: the built-in table, plus any in <data dir>/hooktop/prices.json.
    let (pricing, note) = load_pricing();
    app.pricing = pricing;
    if let Some(note) = note {
        app.say(note);
    }
    // Settings: <data dir>/hooktop/config.json, plus the flags.
    let (settings, note) = load_config();
    app.automated_prompts = settings.automated_prompts;
    for p in &cfg.automated_prompts {
        if !app.automated_prompts.contains(p) {
            app.automated_prompts.push(p.clone());
        }
    }
    if let Some(note) = note {
        app.say(note);
    }

    // Hooks without the token would be refused, and sessions would silently
    // stop showing up: say so on screen.
    let url = crate::hooks::hook_url("127.0.0.1", cfg.port);
    let current = cfg.token.as_ref().map(|t| t.current());
    app.hook_warning = match crate::hooks::check(&crate::hooks::default_settings_path(), &url, current.as_deref()) {
        crate::hooks::HookCheck::Ok => None,
        crate::hooks::HookCheck::NotInstalled => {
            Some("Claude Code isn't sending to hooktop yet: run `hooktop hooks install`".into())
        }
        crate::hooks::HookCheck::WrongToken => {
            Some("Claude Code's hooks don't have hooktop's token: run `hooktop hooks install` to update them".into())
        }
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    let out = client::spawn(
        format!("ws://127.0.0.1:{}/ws", cfg.port),
        cfg.token.clone(),
        tx.clone(),
        AppEvent::Net,
    );

    // Terminal input on a plain thread: crossterm's reads block.
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(AppEvent::Term(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) if tx.is_closed() => break,
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &mut rx, &out).await;
    ratatui::restore();
    drop(rx);

    if let Some(s) = embedded {
        s.stop().await?;
    }
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    out: &mpsc::UnboundedSender<String>,
) -> Result<()> {
    // Redraw a few times a second so live durations tick.
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        terminal.draw(|f| ui::draw(f, app, crate::now_ms()))?;
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { return Ok(()) };
                handle(app, ev);
                // Fold everything already queued into one redraw.
                while let Ok(ev) = rx.try_recv() {
                    handle(app, ev);
                }
            }
            _ = tick.tick() => {}
        }
        for msg in app.outbox.drain(..) {
            let _ = out.send(msg);
        }
        if app.quit {
            return Ok(());
        }
    }
}

/// Where price overrides live: next to the event store.
pub fn prices_path() -> PathBuf {
    crate::auth::token_path().with_file_name("prices.json")
}

/// Where settings live: next to the event store.
pub fn config_path() -> PathBuf {
    crate::auth::token_path().with_file_name("config.json")
}

/// What `config.json` can set. Unknown keys are ignored, so a newer file
/// still works with an older hooktop.
#[derive(Debug, Default, serde::Deserialize)]
pub struct Settings {
    /// Prompts starting with one of these are automated, not yours.
    #[serde(default)]
    pub automated_prompts: Vec<String>,
}

/// [`Settings`] from [`config_path`], and a footer message when the file
/// can't be read.
fn load_config() -> (Settings, Option<String>) {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (Settings::default(), None);
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(s) => (s, None),
        Err(e) => (
            Settings::default(),
            Some(format!("{} couldn't be read ({e}); ignoring it", path.display())),
        ),
    }
}

/// The built-in prices, plus overrides from [`prices_path`] if it exists.
/// Returns a message for the footer when the file has problems.
fn load_pricing() -> (hooktop_core::pricing::Pricing, Option<String>) {
    let builtin = hooktop_core::pricing::Pricing::builtin();
    let path = prices_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (builtin, None);
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(json) => {
            let (p, bad) = builtin.with_overrides(&json);
            let note =
                (!bad.is_empty()).then(|| format!("{}: couldn't read prices for {}", path.display(), bad.join(", ")));
            (p, note)
        }
        Err(e) => (
            builtin,
            Some(format!(
                "{} isn't valid JSON ({e}); using built-in prices",
                path.display()
            )),
        ),
    }
}

fn handle(app: &mut App, ev: AppEvent) {
    match ev {
        AppEvent::Term(TermEvent::Key(k)) => app.on_key(k),
        AppEvent::Term(_) => {}
        AppEvent::Net(n) => app.on_net(n),
    }
}

#[cfg(test)]
mod tests;
