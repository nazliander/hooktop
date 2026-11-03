//! TUI state and key handling, kept free of terminal I/O so it can be
//! tested directly.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use hooktop_core::view::{self, Member, Row, RowKind};
use hooktop_core::{SessionGraph, SessionSummary, Status, TaskId, TaskKind};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::{ListState, TableState};

use super::client::{Net, ServerMsg, remove_msg, remove_stale_msg, subscribe_msg};
use super::dag;
use super::theme::ThemeName;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Connections,
    Party,
    History,
}

/// What fills the main area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Charts, the party and the history on a shared time axis.
    Timeline,
    /// Tasks as nodes and edges.
    Graph,
    /// Rankings and totals for the session.
    Stats,
}

impl View {
    pub const ALL: [View; 3] = [View::Timeline, View::Graph, View::Stats];

    pub fn label(self) -> &'static str {
        match self {
            View::Timeline => "Timeline",
            View::Graph => "Graph",
            View::Stats => "Statistics",
        }
    }

    fn step(self, forward: bool) -> View {
        let i = View::ALL.iter().position(|v| *v == self).unwrap_or(0);
        let n = View::ALL.len();
        View::ALL[if forward { (i + 1) % n } else { (i + n - 1) % n }]
    }
}

/// How long an open connection must be silent (with no live process hooktop
/// knows of) before `X` counts it as stale.
pub const STALE_AFTER_MS: i64 = 60 * 60 * 1000;

/// A question waiting for y or n in the footer.
#[derive(Debug, Clone, PartialEq)]
pub enum Confirm {
    Remove { session_id: String, title: String },
    RemoveStale { count: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Talking to a `hooktop serve` that was already running.
    Attached,
    /// Running the server inside this process.
    Embedded,
}

pub struct App {
    pub port: u16,
    pub mode: Mode,
    pub theme: ThemeName,
    pub connected: bool,
    pub link_error: Option<String>,
    pub server_version: Option<String>,

    /// Newest first.
    pub sessions: Vec<SessionSummary>,
    pub conn_state: ListState,
    /// The session we're subscribed to.
    pub current: Option<String>,
    pub graph: Option<SessionGraph>,

    pub view: View,
    pub focus: Focus,
    /// The selected node in the graph view.
    pub graph_sel: Option<TaskId>,
    /// Graph nodes showing their subtasks.
    pub graph_open: HashSet<TaskId>,
    /// Top-left canvas cell of the graph view.
    pub graph_scroll: (u16, u16),
    pub party_state: ListState,
    /// Whose history is shown: `None` is the Boss.
    pub agent: Option<TaskId>,

    pub expanded: HashSet<TaskId>,
    auto_opened: HashSet<TaskId>,
    touched: HashSet<TaskId>,
    pub hist_state: TableState,
    hist_key: Option<String>,
    /// Keep the cursor on the newest row as work comes in.
    pub follow: bool,
    pub hide_notices: bool,

    pub show_sidebar: bool,
    pub show_details: bool,
    pub details_scroll: u16,
    pub help: bool,
    pub confirm: Option<Confirm>,
    /// A standing problem to show until it's fixed (outdated hooks).
    pub hook_warning: Option<String>,
    /// Prices for turning token usage into money.
    pub pricing: hooktop_core::pricing::Pricing,
    /// Prompts starting with one of these are automated, not yours.
    pub automated_prompts: Vec<String>,
    flash: Option<(String, Instant)>,

    pub quit: bool,
    /// Messages for the server, sent by the run loop.
    pub outbox: Vec<String>,
}

impl App {
    pub fn new(port: u16, mode: Mode, theme: ThemeName) -> Self {
        Self {
            port,
            mode,
            theme,
            connected: false,
            link_error: None,
            server_version: None,
            sessions: Vec::new(),
            conn_state: ListState::default(),
            current: None,
            graph: None,
            view: View::Timeline,
            focus: Focus::History,
            graph_sel: None,
            graph_open: HashSet::new(),
            graph_scroll: (0, 0),
            party_state: ListState::default().with_selected(Some(0)),
            agent: None,
            expanded: HashSet::new(),
            auto_opened: HashSet::new(),
            touched: HashSet::new(),
            hist_state: TableState::default(),
            hist_key: None,
            follow: true,
            hide_notices: true,
            show_sidebar: true,
            show_details: true,
            details_scroll: 0,
            help: false,
            confirm: None,
            hook_warning: None,
            pricing: hooktop_core::pricing::Pricing::builtin(),
            automated_prompts: Vec::new(),
            flash: None,
            quit: false,
            outbox: Vec::new(),
        }
    }

    pub fn say(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    /// A short message to show in the footer, for a few seconds.
    pub fn flash(&self) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(4))
            .map(|(m, _)| m.as_str())
    }

    // ── Derived views ──────────────────────────────────────────────────

    pub fn members(&self) -> Vec<Member> {
        self.graph.as_ref().map(view::party).unwrap_or_default()
    }

    pub fn rows(&self) -> Vec<Row> {
        match &self.graph {
            Some(g) => view::history_rows(g, self.agent, &self.expanded, self.hide_notices),
            None => Vec::new(),
        }
    }

    pub fn selected_row(&self) -> Option<Row> {
        if self.view == View::Graph {
            return self.graph_row();
        }
        let rows = self.rows();
        self.hist_state.selected().and_then(|i| rows.get(i).cloned())
    }

    pub fn layout(&self) -> dag::Layout {
        match &self.graph {
            Some(g) => dag::layout(g, &self.graph_open, self.hide_notices),
            None => dag::Layout::default(),
        }
    }

    /// The graph selection as a history row, for the details pane.
    fn graph_row(&self) -> Option<Row> {
        let g = self.graph.as_ref()?;
        let t = g.task(self.graph_sel?)?;
        let h = &t.header;
        let kind = match (h.kind, h.label.as_str()) {
            (TaskKind::Prompt, "Command") => RowKind::Command,
            (TaskKind::Prompt, "Notification") => RowKind::Notification,
            (TaskKind::Prompt, "System") => RowKind::System,
            (TaskKind::Prompt, _) => RowKind::Prompt,
            _ => RowKind::Turn,
        };
        Some(Row {
            kind,
            depth: 0,
            task: h.id,
            tool: None,
            expandable: !t.tools.is_empty(),
            expanded: self.graph_open.contains(&h.id),
            status: h.status,
            started_ms: h.started_ms,
            ended_ms: h.ended_ms,
        })
    }

    pub fn open_count(&self) -> usize {
        self.sessions.iter().filter(|s| view::is_open(s.status)).count()
    }

    // ── Server messages ────────────────────────────────────────────────

    pub fn on_net(&mut self, net: Net) {
        match net {
            Net::Up => {
                self.connected = true;
                self.link_error = None;
                if let Some(id) = &self.current {
                    self.outbox.push(subscribe_msg(id));
                }
            }
            Net::Down(reason) => {
                self.connected = false;
                self.link_error = Some(reason);
            }
            Net::Msg(msg) => self.on_msg(*msg),
        }
    }

    fn on_msg(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Hello { version, sessions } => {
                self.server_version = Some(version);
                self.sessions = sessions;
                self.sort_sessions();
                self.pick_default_session();
            }
            ServerMsg::Sessions { sessions } => {
                for s in sessions {
                    match self.sessions.iter_mut().find(|x| x.session_id == s.session_id) {
                        Some(x) => *x = s,
                        None => self.sessions.push(s),
                    }
                }
                self.sort_sessions();
                self.pick_default_session();
                self.follow_busiest();
            }
            ServerMsg::Snapshot { session_id, graph, .. } => {
                if self.current.as_deref() == Some(session_id.as_str()) {
                    self.graph = Some(*graph);
                    self.graph_changed();
                }
            }
            ServerMsg::Deltas { session_id, deltas, .. } => {
                if self.current.as_deref() == Some(session_id.as_str())
                    && let Some(g) = &mut self.graph
                {
                    for d in &deltas {
                        g.apply_delta(d);
                    }
                    self.graph_changed();
                }
            }
            ServerMsg::Removed { session_ids } => {
                self.sessions.retain(|s| !session_ids.contains(&s.session_id));
                let n = session_ids.len();
                self.say(format!("Removed {n} connection{}", if n == 1 { "" } else { "s" }));
                if self.current.as_ref().is_some_and(|c| session_ids.contains(c)) {
                    self.current = None;
                    self.graph = None;
                }
                self.sort_sessions();
                self.pick_default_session();
            }
            ServerMsg::Resync => {}
            ServerMsg::Error { message } => self.say(format!("server: {message}")),
        }
    }

    fn sort_sessions(&mut self) {
        self.sessions.sort_by(|a, b| {
            b.updated_ms
                .cmp(&a.updated_ms)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        let i = self
            .current
            .as_ref()
            .and_then(|id| self.sessions.iter().position(|s| &s.session_id == id));
        self.conn_state
            .select(i.or(if self.sessions.is_empty() { None } else { Some(0) }));
    }

    /// With follow on, move to where Claude is working: the most recently
    /// active connection that's working, if the current one isn't.
    fn follow_busiest(&mut self) {
        if !self.follow {
            return;
        }
        let current_working = self
            .sessions
            .iter()
            .any(|s| Some(&s.session_id) == self.current.as_ref() && s.status == hooktop_core::SessionStatus::Running);
        if current_working {
            return;
        }
        if let Some(s) = self
            .sessions
            .iter()
            .find(|s| s.status == hooktop_core::SessionStatus::Running)
        {
            let id = s.session_id.clone();
            self.open_session(&id);
        }
    }

    /// Sessions `X` would remove: closed, or quiet for an hour.
    pub fn stale_count(&self, now: i64) -> usize {
        self.sessions
            .iter()
            .filter(|s| s.status == hooktop_core::SessionStatus::Ended || now - s.updated_ms > STALE_AFTER_MS)
            .count()
    }

    fn pick_default_session(&mut self) {
        if self.current.is_none()
            && let Some(first) = self.sessions.first()
        {
            let id = first.session_id.clone();
            self.open_session(&id);
        }
    }

    pub fn open_session(&mut self, id: &str) {
        if self.current.as_deref() == Some(id) {
            return;
        }
        self.current = Some(id.to_string());
        self.graph = None;
        self.agent = None;
        self.party_state.select(Some(0));
        self.expanded.clear();
        self.auto_opened.clear();
        self.touched.clear();
        self.hist_state = TableState::default();
        self.hist_key = None;
        self.details_scroll = 0;
        self.graph_sel = None;
        self.graph_open.clear();
        self.graph_scroll = (0, 0);
        if let Some(i) = self.sessions.iter().position(|s| s.session_id == id) {
            self.conn_state.select(Some(i));
        }
        self.outbox.push(subscribe_msg(id));
    }

    /// Keep the running prompt and turn (and the latest ones) open, unless
    /// the user folded them by hand; fold what we opened once it's done.
    fn sync_auto_expand(&mut self) {
        let Some(g) = &self.graph else { return };
        let last = |kind: TaskKind| {
            g.tasks
                .iter()
                .rev()
                .find(|t| t.header.kind == kind)
                .map(|t| t.header.id)
        };
        let (last_prompt, last_turn) = (last(TaskKind::Prompt), last(TaskKind::Turn));
        for t in &g.tasks {
            let id = t.header.id;
            if t.header.kind == TaskKind::Subagent || self.touched.contains(&id) {
                continue;
            }
            let want = t.header.status == Status::Running || Some(id) == last_prompt || Some(id) == last_turn;
            if want {
                if self.expanded.insert(id) {
                    self.auto_opened.insert(id);
                }
            } else if self.auto_opened.remove(&id) {
                self.expanded.remove(&id);
            }
        }
    }

    fn graph_changed(&mut self) {
        self.sync_auto_expand();
        if let (Some(a), Some(g)) = (self.agent, &self.graph)
            && g.task(a).is_none()
        {
            self.agent = None;
        }
        let members = self.members();
        let pi = members.iter().position(|m| m.key == self.agent).unwrap_or(0);
        self.party_state.select(Some(pi));
        self.restore_cursor();
        self.sync_graph_selection();
    }

    fn sync_graph_selection(&mut self) {
        let lay = self.layout();
        let valid = self.graph_sel.is_some_and(|id| lay.get(id).is_some());
        if self.follow || !valid {
            let newest = lay.newest();
            if newest != self.graph_sel {
                self.details_scroll = 0;
            }
            self.graph_sel = newest;
        }
    }

    /// Put the history cursor back on the same row after rows change.
    fn restore_cursor(&mut self) {
        let rows = self.rows();
        if rows.is_empty() {
            self.hist_state.select(None);
            return;
        }
        let Some(g) = &self.graph else { return };
        let i = if self.follow {
            rows.len() - 1
        } else {
            self.hist_key
                .as_ref()
                .and_then(|k| rows.iter().position(|r| &r.key(g) == k))
                .unwrap_or_else(|| self.hist_state.selected().unwrap_or(0).min(rows.len() - 1))
        };
        self.select_row(&rows, i);
    }

    fn select_row(&mut self, rows: &[Row], i: usize) {
        let Some(g) = &self.graph else { return };
        let i = i.min(rows.len().saturating_sub(1));
        let key = rows.get(i).map(|r| r.key(g));
        if key != self.hist_key {
            self.details_scroll = 0;
        }
        self.hist_key = key;
        self.hist_state.select(if rows.is_empty() { None } else { Some(i) });
    }

    fn show_agent(&mut self, agent: Option<TaskId>) {
        if self.agent == agent {
            return;
        }
        self.agent = agent;
        self.follow = true;
        self.hist_state = TableState::default();
        self.hist_key = None;
        let members = self.members();
        let pi = members.iter().position(|m| m.key == agent).unwrap_or(0);
        self.party_state.select(Some(pi));
        self.restore_cursor();
    }

    // ── Keys ───────────────────────────────────────────────────────────

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
            self.quit = true;
            return;
        }
        if let Some(c) = self.confirm.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                match c {
                    Confirm::Remove { session_id, .. } => self.outbox.push(remove_msg(&session_id)),
                    Confirm::RemoveStale { .. } => self.outbox.push(remove_stale_msg(STALE_AFTER_MS)),
                }
            } else {
                self.say("Kept everything");
            }
            return;
        }
        if self.help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter
            ) {
                self.help = false;
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab => self.cycle_focus(true),
            KeyCode::BackTab => self.cycle_focus(false),
            KeyCode::Char('1') if self.show_sidebar => self.focus = Focus::Connections,
            KeyCode::Char('2') if self.view == View::Timeline => self.focus = Focus::Party,
            KeyCode::Char('o') | KeyCode::Char('O') => {
                self.set_view(self.view.step(key.code == KeyCode::Char('o')));
            }
            KeyCode::Char('s') => self.set_view(if self.view == View::Stats {
                View::Timeline
            } else {
                View::Stats
            }),
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(id) = self.current.clone() {
                    let title = self
                        .sessions
                        .iter()
                        .find(|s| s.session_id == id)
                        .map(|s| s.title.clone())
                        .unwrap_or_else(|| id.clone());
                    self.confirm = Some(Confirm::Remove { session_id: id, title });
                }
            }
            KeyCode::Char('X') => {
                let count = self.stale_count(crate::now_ms());
                if count == 0 {
                    self.say("Nothing to clear: no closed or quiet connections");
                } else {
                    self.confirm = Some(Confirm::RemoveStale { count });
                }
            }
            KeyCode::Char('3') => self.focus = Focus::History,
            KeyCode::Char('[') => {
                self.show_sidebar = !self.show_sidebar;
                if !self.show_sidebar && self.focus == Focus::Connections {
                    self.focus = Focus::History;
                }
            }
            KeyCode::Char(']') | KeyCode::Char('d') => self.show_details = !self.show_details,
            KeyCode::Char('t') => {
                self.theme = self.theme.next();
                let label = self.theme.label();
                self.say(format!("Theme: {label}"));
            }
            KeyCode::Char('n') => {
                self.hide_notices = !self.hide_notices;
                self.say(if self.hide_notices {
                    "Hiding notifications"
                } else {
                    "Showing notifications"
                });
                self.restore_cursor();
                self.sync_graph_selection();
            }
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                self.say(if self.follow {
                    "Following: jumps to the connection Claude is working in, and to the newest row"
                } else {
                    "Follow off: the cursor stays where you put it"
                });
                self.follow_busiest();
                self.restore_cursor();
                self.sync_graph_selection();
            }
            KeyCode::PageDown => self.details_scroll = self.details_scroll.saturating_add(8),
            KeyCode::PageUp => self.details_scroll = self.details_scroll.saturating_sub(8),
            KeyCode::Char('e') if self.view == View::Graph => self.graph_fold_all(true),
            KeyCode::Char('c') if self.view == View::Graph => self.graph_fold_all(false),
            KeyCode::Char('e') => self.fold_all(true),
            KeyCode::Char('c') => self.fold_all(false),
            KeyCode::Esc | KeyCode::Backspace => {
                if self.agent.is_some() {
                    self.show_agent(None);
                    self.say("Back to Watcher");
                }
            }
            _ => match self.focus {
                Focus::Connections => self.key_connections(key.code),
                Focus::Party => self.key_party(key.code),
                Focus::History if self.view == View::Graph => self.key_graph(key.code),
                Focus::History if self.view == View::Stats => {}
                Focus::History => self.key_history(key.code),
            },
        }
    }

    fn set_view(&mut self, v: View) {
        self.view = v;
        if v != View::Timeline && self.focus == Focus::Party {
            self.focus = Focus::History;
        }
        self.details_scroll = 0;
        self.sync_graph_selection();
        self.say(format!("{} · o for the next tab", v.label()));
    }

    fn cycle_focus(&mut self, forward: bool) {
        let mut order = match self.view {
            View::Timeline => vec![Focus::Party, Focus::History],
            View::Graph | View::Stats => vec![Focus::History],
        };
        if self.show_sidebar {
            order.insert(0, Focus::Connections);
        }
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = order.len();
        self.focus = order[if forward { (i + 1) % n } else { (i + n - 1) % n }];
    }

    fn key_connections(&mut self, code: KeyCode) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        let cur = self.conn_state.selected().unwrap_or(0);
        let next = match code {
            KeyCode::Up | KeyCode::Char('k') => cur.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (cur + 1).min(n - 1),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => n - 1,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                self.focus = Focus::History;
                return;
            }
            _ => return,
        };
        let id = self.sessions[next].session_id.clone();
        if self.current.as_deref() != Some(id.as_str()) && self.follow {
            self.follow = false;
            self.say("Follow off: you picked a connection (f to follow again)");
        }
        self.open_session(&id);
    }

    fn key_party(&mut self, code: KeyCode) {
        let members = self.members();
        if members.is_empty() {
            return;
        }
        let cur = self.party_state.selected().unwrap_or(0).min(members.len() - 1);
        let next = match code {
            KeyCode::Up | KeyCode::Char('k') => cur.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (cur + 1).min(members.len() - 1),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => members.len() - 1,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                self.focus = Focus::History;
                return;
            }
            _ => return,
        };
        self.show_agent(members[next].key);
    }

    fn key_history(&mut self, code: KeyCode) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let cur = self.hist_state.selected().unwrap_or(0).min(rows.len() - 1);
        let last = rows.len() - 1;
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.follow = false;
                self.select_row(&rows, cur.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.follow = cur + 1 >= last && self.follow;
                self.select_row(&rows, cur + 1);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.follow = false;
                self.select_row(&rows, 0);
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.follow = true;
                self.select_row(&rows, last);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let r = &rows[cur];
                if r.expandable && !r.expanded {
                    self.set_open(r.task, true);
                } else if cur < last {
                    self.select_row(&rows, cur + 1);
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let r = &rows[cur];
                if r.expanded {
                    self.set_open(r.task, false);
                } else if let Some(p) = rows[..cur].iter().rposition(|p| p.depth < r.depth) {
                    self.follow = false;
                    self.select_row(&rows, p);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let r = rows[cur].clone();
                let spawned = self
                    .graph
                    .as_ref()
                    .and_then(|g| r.tool_call(g))
                    .and_then(|c| c.spawned_task);
                if let Some(t) = spawned {
                    self.show_agent(Some(t));
                    let name = self
                        .graph
                        .as_ref()
                        .and_then(|g| g.task(t))
                        .map(|t| view::agent_name(t.header.agent_type.as_deref()));
                    self.say(format!(
                        "Following {} · Esc goes back to Watcher",
                        name.unwrap_or_default()
                    ));
                } else if r.expandable {
                    self.set_open(r.task, !r.expanded);
                }
            }
            _ => {}
        }
    }

    fn key_graph(&mut self, code: KeyCode) {
        let lay = self.layout();
        let Some(cur) = self.graph_sel.or_else(|| lay.newest()) else {
            return;
        };
        let next = match code {
            KeyCode::Left | KeyCode::Char('h') => lay.beside(cur, -1).or_else(|| lay.vertical(cur, false)),
            KeyCode::Right | KeyCode::Char('l') => lay.beside(cur, 1),
            KeyCode::Up | KeyCode::Char('k') => lay.vertical(cur, false),
            KeyCode::Down | KeyCode::Char('j') => lay.vertical(cur, true),
            KeyCode::Home | KeyCode::Char('g') => lay.first(),
            KeyCode::End | KeyCode::Char('G') => {
                self.follow = true;
                lay.newest()
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let has_tools = self
                    .graph
                    .as_ref()
                    .and_then(|g| g.task(cur))
                    .is_some_and(|t| !t.tools.is_empty());
                if has_tools && !self.graph_open.remove(&cur) {
                    self.graph_open.insert(cur);
                }
                None
            }
            _ => None,
        };
        if let Some(n) = next
            && n != cur
        {
            if !matches!(code, KeyCode::End | KeyCode::Char('G')) {
                self.follow = false;
            }
            self.graph_sel = Some(n);
            self.details_scroll = 0;
        }
    }

    fn graph_fold_all(&mut self, open: bool) {
        self.graph_open.clear();
        if open && let Some(g) = &self.graph {
            self.graph_open
                .extend(g.tasks.iter().filter(|t| !t.tools.is_empty()).map(|t| t.header.id));
        }
    }

    fn set_open(&mut self, task: TaskId, open: bool) {
        self.touched.insert(task);
        self.auto_opened.remove(&task);
        if open {
            self.expanded.insert(task);
        } else {
            self.expanded.remove(&task);
        }
        if !open {
            self.follow = false;
        }
        self.restore_cursor();
    }

    fn fold_all(&mut self, open: bool) {
        let Some(g) = &self.graph else { return };
        let ids: Vec<TaskId> = g
            .tasks
            .iter()
            .filter(|t| t.header.kind != TaskKind::Subagent)
            .map(|t| t.header.id)
            .collect();
        for id in ids {
            self.touched.insert(id);
            self.auto_opened.remove(&id);
            if open {
                self.expanded.insert(id);
            } else {
                self.expanded.remove(&id);
            }
        }
        if !open {
            self.follow = false;
        }
        self.restore_cursor();
    }
}
