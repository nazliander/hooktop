use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use hooktop_server::server::{self, ServerConfig};
use hooktop_server::store;
use hooktop_server::tui::{self, TuiConfig, theme::ThemeName};
use hooktop_server::{DEFAULT_PORT, auth, hooks};
use std::io::IsTerminal;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

/// hooktop: see Claude Code's tool calls, subagents and CPU/RAM use, live.
///
/// Run `hooktop` on its own for the terminal UI. It attaches to a running
/// `hooktop serve`, or runs the server itself while it's open.
#[derive(Parser)]
#[command(name = "hooktop", version = hooktop_server::VERSION, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    tui: TuiArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// The terminal UI (the default command).
    Tui(TuiArgs),
    /// Run the server headless, to record sessions with no window open
    /// (`brew services start hooktop` runs this). `hooktop` attaches to it.
    Serve(ServeArgs),
    /// Add or remove hooktop's hooks in Claude Code's settings.json.
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Print a session's stored events as JSONL (replayable with `serve --replay`).
    Export {
        session_id: String,
        #[arg(long, env = "HOOKTOP_DB")]
        db: Option<PathBuf>,
    },
    /// Rotate hooktop's token, or show where it's kept.
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// Delete stored sessions: all of them, or one with --session.
    Reset {
        /// Only delete this session.
        #[arg(long)]
        session: Option<String>,
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
        #[arg(long, env = "HOOKTOP_DB")]
        db: Option<PathBuf>,
        /// Port to check for a running server.
        #[arg(long, default_value_t = DEFAULT_PORT, env = "HOOKTOP_PORT")]
        port: u16,
    },
}

#[derive(Args, Clone)]
struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1", env = "HOOKTOP_HOST")]
    host: String,
    #[arg(long, default_value_t = DEFAULT_PORT, env = "HOOKTOP_PORT")]
    port: u16,
    /// SQLite event log [default: <data dir>/hooktop/events.db]
    #[arg(long, env = "HOOKTOP_DB")]
    db: Option<PathBuf>,
    /// Keep everything in memory only.
    #[arg(long, conflicts_with = "db")]
    no_store: bool,
    /// Read the token from this file [default: <data dir>/hooktop/token, created
    /// on first run]. HOOKTOP_TOKEN in the environment takes precedence. There's no
    /// --token flag on purpose: command lines are visible to every user.
    #[arg(long, env = "HOOKTOP_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// Accept requests without a token. Any local process or web page could
    /// then read and change hooktop's data.
    #[arg(long, conflicts_with = "token_file")]
    no_auth: bool,
    /// Allow --host to be an address other machines can reach.
    #[arg(long)]
    allow_remote: bool,
    /// Stop after this long with no hooks, requests or open hooktop windows:
    /// 30m, 1h, 1d, or `off` to run until stopped.
    #[arg(long, default_value = "1h", env = "HOOKTOP_IDLE_TIMEOUT", value_parser = parse_idle)]
    idle_timeout: i64,
    /// Mark connections Closed after this long with no activity (they reopen
    /// if Claude picks them up): 30m, 1h, 1d, or `off`.
    #[arg(long, default_value = "1h", env = "HOOKTOP_CLOSE_AFTER", value_parser = parse_idle)]
    close_after: i64,
    /// Play a synthetic session on a loop (not stored).
    #[arg(long)]
    demo: bool,
    /// Replay a JSONL file of recorded events (not stored).
    #[arg(long)]
    replay: Option<PathBuf>,
    /// Playback speed for --demo and --replay.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
}

impl Default for ServeArgs {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: DEFAULT_PORT,
            db: None,
            no_store: false,
            token_file: None,
            no_auth: false,
            allow_remote: false,
            idle_timeout: 3_600_000,
            close_after: 3_600_000,
            demo: false,
            replay: None,
            speed: 1.0,
        }
    }
}

#[derive(Args, Clone)]
struct TuiArgs {
    #[arg(long, default_value_t = DEFAULT_PORT, env = "HOOKTOP_PORT")]
    port: u16,
    /// SQLite event log, when hooktop runs the server itself.
    #[arg(long, env = "HOOKTOP_DB")]
    db: Option<PathBuf>,
    /// Keep everything in memory only (when hooktop runs the server itself).
    #[arg(long, conflicts_with = "db")]
    no_store: bool,
    /// Play a synthetic session on a loop (when hooktop runs the server itself).
    #[arg(long)]
    demo: bool,
    /// Playback speed for --demo.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// ayu-dark or ayu-light [default: ayu-light if the terminal says its
    /// background is light, else ayu-dark]
    #[arg(long, env = "HOOKTOP_THEME", value_parser = parse_theme)]
    theme: Option<ThemeName>,
    /// Read the token from this file (see `hooktop serve --help`).
    #[arg(long, env = "HOOKTOP_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// Mark connections Closed after this long with no activity (they reopen
    /// if Claude picks them up): 30m, 1h, 1d, or `off`.
    #[arg(long, default_value = "1h", env = "HOOKTOP_CLOSE_AFTER", value_parser = parse_idle)]
    close_after: i64,
    /// Run the server without a token (see `hooktop serve --help`).
    #[arg(long, conflicts_with = "token_file")]
    no_auth: bool,
    /// Prompts starting with PREFIX come from your own tooling, not from
    /// you: they don't count as prompts in Statistics. Repeat for several;
    /// `automated_prompts` in config.json works too.
    #[arg(long = "automated-prompt", value_name = "PREFIX")]
    automated_prompts: Vec<String>,
}

fn parse_idle(s: &str) -> Result<i64, String> {
    match s.trim() {
        "off" | "never" | "0" => Ok(0),
        other => auth::parse_duration(other),
    }
}

fn parse_theme(s: &str) -> Result<ThemeName, String> {
    ThemeName::parse(s).ok_or_else(|| format!("unknown theme {s:?}; try ayu-dark or ayu-light"))
}

#[derive(Subcommand)]
enum HooksAction {
    /// Add hooktop's http hooks (keeps a backup of the original file).
    Install(HookTarget),
    /// Remove hooktop's hooks, leaving everything else untouched.
    Uninstall(HookTarget),
    /// Print the hooks block instead of writing it.
    Print(HookTarget),
}

#[derive(Args)]
struct HookTarget {
    /// settings.json to edit [default: ~/.claude/settings.json]
    #[arg(long)]
    settings: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Send `Bearer $HOOKTOP_TOKEN` from Claude Code's environment instead of
    /// writing the token into settings.json (for a settings file you share).
    #[arg(long)]
    token_from_env: bool,
}

impl HookTarget {
    fn auth(&self) -> Result<hooks::HookAuth> {
        Ok(if self.token_from_env {
            hooks::HookAuth::FromEnv
        } else {
            hooks::HookAuth::Token(auth::resolve(None)?)
        })
    }
}

#[derive(Subcommand)]
enum TokenAction {
    /// Make a new token. A running hooktop switches to it without a restart,
    /// and Claude Code's hooks are updated.
    Rotate {
        /// Cut the old token off right away. Claude Code sessions that are
        /// already running stop reaching hooktop until restarted.
        #[arg(long, conflicts_with = "grace")]
        now: bool,
        /// How long the old token may still send hooks (never read data),
        /// for sessions that were already running: 30m, 12h, 7d.
        #[arg(long, default_value = "12h", value_parser = auth::parse_duration)]
        grace: i64,
        /// The token file to rotate [default: <data dir>/hooktop/token]
        #[arg(long, env = "HOOKTOP_TOKEN_FILE")]
        token_file: Option<PathBuf>,
        /// settings.json whose hooks to update [default: ~/.claude/settings.json]
        #[arg(long)]
        settings: Option<PathBuf>,
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
    /// Print where the token is kept.
    Path,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let cmd = match cli.cmd {
        Some(c) => c,
        // Piped or under a service manager: there's no screen, just serve.
        None if !std::io::stdout().is_terminal() => Cmd::Serve(ServeArgs::default()),
        None => Cmd::Tui(cli.tui),
    };
    // Commands that only print should stop quietly when piped into
    // `head`, like other CLI tools, instead of panicking on a closed pipe.
    // Not for the server or TUI: there a dropped socket must not kill us.
    #[cfg(unix)]
    if matches!(cmd, Cmd::Export { .. } | Cmd::Token { .. } | Cmd::Hooks { .. }) {
        // SAFETY: restores the default disposition before anything is printed.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| "hooktop=info,hooktop_server=info".into());
    let mut log_note = None;
    if matches!(cmd, Cmd::Tui(_)) {
        // The screen belongs to the TUI: logs go to a file instead.
        match open_log_file() {
            Ok((file, path)) => {
                tracing_subscriber::fmt()
                    .with_env_filter(filter())
                    .with_ansi(false)
                    .with_writer(Mutex::new(file))
                    .init();
                log_note = Some(path);
            }
            Err(_) => tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_writer(std::io::sink)
                .init(),
        }
    } else {
        tracing_subscriber::fmt().with_env_filter(filter()).init();
    }

    let result = match cmd {
        Cmd::Tui(args) => async {
            let token = tui_token(&args)?;
            tui::run(TuiConfig {
                port: args.port,
                db: args.db,
                no_store: args.no_store,
                demo: args.demo,
                speed: args.speed,
                theme: args.theme,
                token,
                close_after: (args.close_after > 0).then(|| Duration::from_millis(args.close_after as u64)),
                automated_prompts: args.automated_prompts,
            })
            .await
        }
        .await
        .map_err(|e| match &log_note {
            Some(p) => e.context(format!("see {} for details", p.display())),
            None => e,
        }),
        Cmd::Serve(args) => serve(args).await,
        Cmd::Hooks { action } => run_hooks(action),
        Cmd::Export { session_id, db } => export(&session_id, db),
        Cmd::Token { action } => run_token(action),
        Cmd::Reset { session, yes, db, port } => reset(session.as_deref(), yes, db, port),
    };
    // One readable line, not a Debug dump with a backtrace.
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn tui_token(args: &TuiArgs) -> Result<Option<std::sync::Arc<auth::Tokens>>> {
    if args.no_auth {
        Ok(None)
    } else {
        auth::Tokens::resolve(args.token_file.as_deref()).map(Some)
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let token = if args.no_auth {
        None
    } else {
        Some(auth::Tokens::resolve(args.token_file.as_deref())?)
    };
    if token.is_some() && args.token_file.is_none() && std::env::var_os("HOOKTOP_TOKEN").is_none() {
        tracing::info!("clients need the token in {}", auth::token_path().display());
    }
    let server = server::start(ServerConfig {
        host: args.host.clone(),
        port: args.port,
        db: args.db.clone(),
        no_store: args.no_store,
        token,
        allow_remote: args.allow_remote,
        idle_timeout: (args.idle_timeout > 0).then(|| Duration::from_millis(args.idle_timeout as u64)),
        close_after: (args.close_after > 0).then(|| Duration::from_millis(args.close_after as u64)),
        demo: args.demo,
        replay: args.replay.clone(),
        speed: args.speed,
    })
    .await?;
    if args.idle_timeout > 0 {
        tracing::info!(
            "stops after {} unused (--idle-timeout off to keep running)",
            hooktop_core::view::duration(args.idle_timeout)
        );
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = server.idle() => {}
    }
    server.stop().await
}

fn open_log_file() -> std::io::Result<(std::fs::File, PathBuf)> {
    let dir = store::default_db_path()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    auth::private_dir(&dir).map_err(std::io::Error::other)?;
    let path = dir.join("hooktop.log");
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    auth::restrict(&path);
    Ok((file, path))
}

fn run_token(action: TokenAction) -> Result<()> {
    match action {
        TokenAction::Path => {
            println!("{}", auth::token_path().display());
        }
        TokenAction::Rotate {
            now,
            grace,
            token_file,
            settings,
            port,
        } => {
            if std::env::var("HOOKTOP_TOKEN").is_ok_and(|t| !t.trim().is_empty()) {
                anyhow::bail!(
                    "HOOKTOP_TOKEN is set, so hooktop isn't using its token file: change HOOKTOP_TOKEN yourself and restart hooktop and Claude Code"
                );
            }
            let path = token_file.unwrap_or_else(auth::token_path);
            let grace = if now { 0 } else { grace };
            let new = auth::rotate(&path, grace, hooktop_server::now_ms())?;
            println!("New token written to {}", path.display());

            let settings = settings.unwrap_or_else(hooks::default_settings_path);
            let url = hooks::hook_url("127.0.0.1", port);
            let updated = hooks::retoken(&settings, &url, &new)?;
            if updated > 0 {
                println!("Updated the token in {updated} hooktop hooks in {}", settings.display());
            }
            if hooks::check(&settings, &url, Some(&new)) == hooks::HookCheck::NotInstalled {
                println!(
                    "No hooktop hooks in {}: run `hooktop hooks install` when you want them",
                    settings.display()
                );
            }
            println!("A running hooktop switches to the new token by itself; open terminal UIs reconnect.");
            if grace > 0 {
                println!(
                    "Claude Code sessions that were already running keep the old token: it can still send hooks (but read nothing) for {}. Restart them to switch, or rotate with --now to cut it off.",
                    hooktop_core::view::duration(grace)
                );
            } else {
                println!("The old token no longer works. Restart Claude Code sessions that were already running.");
            }
        }
    }
    Ok(())
}

fn run_hooks(action: HooksAction) -> Result<()> {
    match action {
        HooksAction::Print(t) => {
            let url = hooks::hook_url(&t.host, t.port);
            println!(
                "{}",
                serde_json::to_string_pretty(&hooks::hooks_block(&url, &t.auth()?))?
            );
        }
        HooksAction::Install(t) => {
            let auth = t.auth()?;
            let path = t.settings.unwrap_or_else(hooks::default_settings_path);
            let url = hooks::hook_url(&t.host, t.port);
            let m = hooks::install(&path, &url, &auth)?;
            match (m.added, m.updated) {
                (0, 0) => println!("hooktop hooks already present in {}", path.display()),
                (0, n) => println!("Updated the token in {n} hooktop hooks in {}", path.display()),
                (a, _) => {
                    println!("Added hooktop hooks for {a} events to {}", path.display());
                    println!("New Claude Code sessions will stream to {url}");
                }
            }
            if !t.token_from_env {
                println!("The hooks carry hooktop's token from {}", auth::token_path().display());
            }
            if m.made_private {
                println!(
                    "Made {} readable only by you, since it now holds the token",
                    path.display()
                );
            }
        }
        HooksAction::Uninstall(t) => {
            let path = t.settings.unwrap_or_else(hooks::default_settings_path);
            let url = hooks::hook_url(&t.host, t.port);
            let removed = hooks::uninstall(&path, &url)?;
            println!("Removed {removed} hooktop hook entries from {}", path.display());
        }
    }
    Ok(())
}

fn export(session_id: &str, db: Option<PathBuf>) -> Result<()> {
    let path = db.unwrap_or_else(store::default_db_path);
    let mut after = 0;
    loop {
        let rows = store::read_events(&path, session_id, after, 1000)?;
        let Some(last) = rows.last() else { break };
        after = last.seq;
        for r in &rows {
            println!("{}", serde_json::to_string(r)?);
        }
    }
    Ok(())
}

fn reset(session: Option<&str>, yes: bool, db: Option<PathBuf>, port: u16) -> Result<()> {
    use std::io::{BufRead, Write};

    let path = db.unwrap_or_else(store::default_db_path);
    // A running server keeps sessions in memory and holds the database
    // open, so deleting underneath it would only half work.
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
        anyhow::bail!("hooktop is running on port {port}; stop `hooktop serve` first, then run `hooktop reset` again");
    }
    let (sessions, events) = store::count(&path, session)?;
    if events == 0 {
        match session {
            Some(id) => println!("No stored events for session {id} in {}", path.display()),
            None => println!("Nothing to delete: {} is empty or missing", path.display()),
        }
        return Ok(());
    }
    let what = match session {
        Some(id) => format!("session {id} ({})", plural(events, "event")),
        None => format!("{} ({})", plural(sessions, "session"), plural(events, "event")),
    };
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("refusing to delete {what} without confirmation; pass --yes");
        }
        print!("Delete {what} from {}? [y/N] ", path.display());
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing deleted.");
            return Ok(());
        }
    }
    store::reset(&path, session)?;
    println!("Deleted {what}.");
    Ok(())
}

fn plural(n: u64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}
