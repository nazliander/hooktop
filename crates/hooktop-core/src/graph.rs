//! The per-session task graph.
//!
//! A session is a DAG of *parent tasks*:
//!
//! - **Prompt** — a user prompt; starts a run.
//! - **Turn** — one assistant message on the main thread and the tool calls
//!   it made. A turn closes on `PostToolBatch` (or, on Claude Code versions
//!   without that hook, when a new tool starts after every earlier one has
//!   finished).
//! - **Subagent** — a spawned agent and all the tool calls it made.
//!
//! Tool calls are the *subtasks* inside a parent. Edges run prompt → turn →
//! turn, and turn → subagent → next turn when a turn spawns a subagent.
//!
//! [`SessionGraph::apply`] folds one [`Event`] in and returns the [`Delta`]s
//! a client needs to stay in sync, so the server never resends whole graphs.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{Event, EventKind, Sample};
use crate::summary::{self, clip};

pub type TaskId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Prompt,
    Turn,
    Subagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Done,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Claude is working on a prompt.
    Running,
    /// Claude finished responding and is waiting for the user.
    Idle,
    Ended,
}

/// A parent task without its tool calls; what [`Delta::Task`] carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskHeader {
    pub id: TaskId,
    pub kind: TaskKind,
    /// Small caps label: `Prompt`, `Turn 3`, `Subagent · Explore`.
    pub label: String,
    pub title: String,
    pub status: Status,
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Prompts: the prompt text, without injected system reminders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Prompts: Claude's final answer. Subagents: their final message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    pub tool_count: u32,
    pub failed_count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    #[serde(flatten)]
    pub header: TaskHeader,
    pub tools: Vec<ToolCall>,
}

/// One tool call: a subtask of a parent task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Claude's `tool_use_id`, or a synthetic `anon-N` if the hook had none.
    pub id: String,
    pub name: String,
    pub target: String,
    pub status: Status,
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<i64>,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// For `Agent`/`Task` calls: the subagent task it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_task: Option<TaskId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: TaskId,
    pub to: TaskId,
}

/// The list-view row for a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub status: SessionStatus,
    pub started_ms: i64,
    pub updated_ms: i64,
    pub task_count: u32,
    pub tool_count: u32,
    pub running_tools: u32,
}

/// An incremental change to a [`SessionGraph`]. Upserts are keyed by id, so
/// applying the same delta twice is harmless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Delta {
    Session {
        status: SessionStatus,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        updated_ms: i64,
    },
    Task {
        task: TaskHeader,
    },
    Tool {
        task_id: TaskId,
        tool: ToolCall,
    },
    Edge(Edge),
    /// A CPU and memory reading; appended, never changed.
    Sample(Sample),
    /// Tokens one model call used; upserted by message id.
    Usage(crate::usage::Usage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGraph {
    pub session_id: String,
    pub title: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub status: SessionStatus,
    pub started_ms: i64,
    pub updated_ms: i64,
    pub tasks: Vec<Task>,
    pub edges: Vec<Edge>,
    /// CPU and memory over time, oldest first. Only recorded while hooktop
    /// is running and can see the session's process.
    #[serde(default)]
    pub samples: Vec<Sample>,
    /// Tokens per model call, attributed to tasks. Read from Claude Code's
    /// transcript, so it trails the hooks by a moment.
    #[serde(default)]
    pub usage: Vec<crate::usage::Usage>,
    #[serde(skip)]
    cur: Cursor,
}

/// Bookkeeping needed to place the next event; not sent to clients.
#[derive(Debug, Clone, Default)]
struct Cursor {
    open_turn: Option<TaskId>,
    /// The prompt being answered; Claude's reply attaches to it on Stop.
    prompt: Option<TaskId>,
    /// Whether the session title came from something the user typed.
    title_from_user: bool,
    /// Latest main-thread task (prompt or turn); the next one links from it.
    last_main: Option<TaskId>,
    turn_no: u32,
    subagents: HashMap<String, TaskId>,
    pending_spawns: Vec<PendingSpawn>,
    /// Finished subagents not yet linked to the turn that consumed them.
    unlinked: Vec<TaskId>,
    tools: HashMap<String, (TaskId, usize)>,
    anon: u32,
    saw_batch: bool,
}

#[derive(Debug, Clone)]
struct PendingSpawn {
    turn: TaskId,
    tool_use_id: String,
    subagent_type: Option<String>,
    description: Option<String>,
}

const SPAWN_TOOLS: [&str; 2] = ["Agent", "Task"];

impl SessionGraph {
    pub fn new(session_id: impl Into<String>, at_ms: i64) -> Self {
        Self {
            session_id: session_id.into(),
            title: String::new(),
            cwd: None,
            model: None,
            status: SessionStatus::Running,
            started_ms: at_ms,
            updated_ms: at_ms,
            tasks: Vec::new(),
            edges: Vec::new(),
            samples: Vec::new(),
            usage: Vec::new(),
            cur: Cursor::default(),
        }
    }

    pub fn summary(&self) -> SessionSummary {
        let tools = self.tasks.iter().flat_map(|t| &t.tools);
        let (mut tool_count, mut running) = (0, 0);
        for t in tools {
            tool_count += 1;
            running += u32::from(t.status == Status::Running);
        }
        SessionSummary {
            session_id: self.session_id.clone(),
            title: self.display_title(),
            cwd: self.cwd.clone(),
            status: self.status,
            started_ms: self.started_ms,
            updated_ms: self.updated_ms,
            task_count: self.tasks.len() as u32,
            tool_count,
            running_tools: running,
        }
    }

    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.get(id as usize)
    }

    /// Apply one delta from the server to a client-side copy of the graph
    /// (the TUI's view of a session). Upserts are keyed by id, so applying
    /// a delta twice is harmless.
    pub fn apply_delta(&mut self, d: &Delta) {
        match d {
            Delta::Session {
                status,
                title,
                cwd,
                model,
                updated_ms,
            } => {
                self.status = *status;
                self.title.clone_from(title);
                if cwd.is_some() {
                    self.cwd.clone_from(cwd);
                }
                if model.is_some() {
                    self.model.clone_from(model);
                }
                self.updated_ms = *updated_ms;
            }
            Delta::Task { task } => {
                let i = task.id as usize;
                if let Some(t) = self.tasks.get_mut(i) {
                    t.header = task.clone();
                } else {
                    // Ids are dense and assigned in order; fill any gap so
                    // indexing by id stays valid.
                    while self.tasks.len() < i {
                        let mut h = task.clone();
                        h.id = self.tasks.len() as TaskId;
                        self.tasks.push(Task {
                            header: h,
                            tools: Vec::new(),
                        });
                    }
                    self.tasks.push(Task {
                        header: task.clone(),
                        tools: Vec::new(),
                    });
                }
            }
            Delta::Tool { task_id, tool } => {
                let Some(t) = self.tasks.get_mut(*task_id as usize) else {
                    return;
                };
                match t.tools.iter_mut().find(|c| c.id == tool.id) {
                    Some(c) => *c = tool.clone(),
                    None => t.tools.push(tool.clone()),
                }
            }
            Delta::Edge(e) => {
                if !self.edges.contains(e) {
                    self.edges.push(*e);
                }
            }
            Delta::Sample(s) => {
                if self.samples.last().is_none_or(|l| l.at_ms < s.at_ms) {
                    self.samples.push(s.clone());
                }
            }
            Delta::Usage(u) => match self.usage.iter_mut().find(|x| x.message_id == u.message_id) {
                Some(x) => *x = u.clone(),
                None => self.usage.push(u.clone()),
            },
        }
    }

    /// Fold one event into the graph and return what changed.
    pub fn apply(&mut self, ev: &Event) -> Vec<Delta> {
        // Samples don't count as activity: they mustn't reorder sessions or
        // keep an idle one looking busy.
        if let EventKind::Sample(s) = &ev.kind {
            self.samples.push(s.clone());
            return vec![Delta::Sample(s.clone())];
        }
        if let EventKind::Usage(u) = &ev.kind {
            return vec![Delta::Usage(self.add_usage(u))];
        }
        let mut out = Vec::new();
        let at = ev.at_ms;
        // Claude doing anything reopens a closed connection (closed for
        // being quiet, or resumed with `claude --resume`).
        if self.status == SessionStatus::Ended
            && matches!(
                ev.kind,
                EventKind::ToolStarted { .. }
                    | EventKind::ToolFinished { .. }
                    | EventKind::ToolFailed { .. }
                    | EventKind::ToolBatchDone
                    | EventKind::SubagentStarted { .. }
                    | EventKind::SubagentStopped { .. }
            )
        {
            self.status = SessionStatus::Running;
        }
        self.updated_ms = self.updated_ms.max(at);
        if self.cwd.is_none() {
            self.cwd.clone_from(&ev.cwd);
        }

        match &ev.kind {
            EventKind::SessionStarted { model, .. } => {
                self.status = SessionStatus::Running;
                if model.is_some() {
                    self.model.clone_from(model);
                }
            }
            EventKind::PromptSubmitted { prompt } => self.on_prompt(prompt, at, &mut out),
            EventKind::ToolStarted {
                tool_use_id,
                name,
                input,
            } => {
                let task = match &ev.agent_id {
                    Some(aid) => {
                        let atype = ev.agent_type.as_deref().unwrap_or("agent");
                        self.ensure_subagent(aid, atype, at, &mut out)
                    }
                    None => self.current_turn(at, &mut out),
                };
                self.add_tool(task, tool_use_id.as_deref(), name, input, at, &mut out);
            }
            EventKind::ToolFinished {
                tool_use_id,
                name,
                response,
            } => {
                let output = summary::output_preview(response);
                self.finish_tool(ev, tool_use_id.as_deref(), name, Status::Done, output, None, &mut out);
            }
            EventKind::ToolFailed {
                tool_use_id,
                name,
                error,
            } => {
                let err = Some(clip(error, summary::OUTPUT_PREVIEW));
                self.finish_tool(ev, tool_use_id.as_deref(), name, Status::Failed, None, err, &mut out);
            }
            EventKind::ToolBatchDone => {
                if ev.agent_id.is_none() {
                    self.cur.saw_batch = true;
                    self.close_turn(at, &mut out);
                }
            }
            EventKind::SubagentStarted { agent_id, agent_type } => {
                self.ensure_subagent(agent_id, agent_type, at, &mut out);
            }
            EventKind::SubagentStopped {
                agent_id,
                agent_type,
                last_message,
            } => {
                let id = self.ensure_subagent(agent_id, agent_type, at, &mut out);
                let h = &mut self.tasks[id as usize].header;
                if h.status == Status::Running {
                    h.status = Status::Done;
                }
                h.ended_ms = Some(at);
                h.reply = last_message.as_deref().map(|m| clip(m, 2000));
                if !self.cur.unlinked.contains(&id) {
                    self.cur.unlinked.push(id);
                }
                self.emit_task(id, &mut out);
            }
            EventKind::Stopped { last_message } => {
                self.close_turn(at, &mut out);
                self.status = SessionStatus::Idle;
                if let Some(id) = self.cur.prompt.take() {
                    let h = &mut self.tasks[id as usize].header;
                    h.status = Status::Done;
                    h.ended_ms = Some(at);
                    if let Some(msg) = last_message {
                        h.reply = Some(clip(msg, summary::OUTPUT_PREVIEW));
                    }
                    self.emit_task(id, &mut out);
                }
            }
            EventKind::SessionEnded { .. } => {
                self.status = SessionStatus::Ended;
                self.interrupt_running(at, &mut out);
                self.cur.open_turn = None;
            }
            EventKind::Notification { .. } | EventKind::Other { .. } | EventKind::Sample(_) | EventKind::Usage(_) => {}
        }

        out.push(Delta::Session {
            status: self.status,
            title: self.display_title(),
            cwd: self.cwd.clone(),
            model: self.model.clone(),
            updated_ms: self.updated_ms,
        });
        out
    }

    /// The task that owns a tool call.
    pub fn tools_owner(&self, tool_use_id: &str) -> Option<TaskId> {
        self.cur.tools.get(tool_use_id).map(|(t, _)| *t)
    }

    /// Record one model call's usage, merged with earlier lines of the same
    /// message, and work out which task it belongs to: the turn or
    /// follower that owns its tool calls, else the follower by agent id,
    /// else the prompt it was answering (by time).
    fn add_usage(&mut self, u: &crate::usage::Usage) -> crate::usage::Usage {
        let i = match self.usage.iter().position(|x| x.message_id == u.message_id) {
            Some(i) => {
                self.usage[i].merge(u);
                i
            }
            None => {
                self.usage.push(u.clone());
                self.usage.len() - 1
            }
        };
        let x = &self.usage[i];
        let by_tool = x
            .tool_use_ids
            .iter()
            .find_map(|id| self.cur.tools.get(id).map(|(t, _)| *t));
        let by_agent = || x.agent_id.as_ref().and_then(|a| self.cur.subagents.get(a).copied());
        let by_time = || {
            self.tasks
                .iter()
                .rev()
                .find(|t| {
                    t.header.kind == TaskKind::Prompt
                        && matches!(t.header.label.as_str(), "Prompt" | "Command")
                        && t.header.started_ms <= x.at_ms
                })
                .map(|t| t.header.id)
        };
        let task = by_tool.or_else(by_agent).or_else(by_time);
        self.usage[i].task = task;
        self.usage[i].clone()
    }

    pub fn display_title(&self) -> String {
        if self.title.is_empty() {
            let short: String = self.session_id.chars().take(8).collect();
            format!("Session {short}")
        } else {
            self.title.clone()
        }
    }

    fn on_prompt(&mut self, prompt: &str, at: i64, out: &mut Vec<Delta>) {
        self.close_turn(at, out);
        // A new prompt means the previous one is answered, Stop or not.
        if let Some(prev) = self.cur.prompt.take() {
            let h = &mut self.tasks[prev as usize].header;
            h.status = Status::Done;
            h.ended_ms.get_or_insert(at);
            self.emit_task(prev, out);
        }
        self.status = SessionStatus::Running;
        let info = crate::prompt::analyze(prompt);
        let title = clip(&info.title, 80);
        // Name the session after the first thing the user actually typed.
        if self.title.is_empty() || (info.from_user && !self.cur.title_from_user) {
            self.title.clone_from(&title);
            self.cur.title_from_user = info.from_user;
        }
        let id = self.new_task(TaskKind::Prompt, info.label.into(), title, at, None, None, out);
        // The prompt stays running until Claude answers it.
        self.tasks[id as usize].header.summary = Some(clip(&info.body, summary::OUTPUT_PREVIEW));
        self.emit_task(id, out);
        self.cur.prompt = Some(id);
        if let Some(prev) = self.cur.last_main {
            self.link(prev, id, out);
        }
        self.cur.last_main = Some(id);
    }

    /// The open main-thread turn, opening a new one when needed.
    fn current_turn(&mut self, at: i64, out: &mut Vec<Delta>) -> TaskId {
        if let Some(t) = self.cur.open_turn {
            // Without PostToolBatch, a new tool after everything finished
            // means a new assistant message.
            if self.cur.saw_batch || !self.all_tools_finished(t) {
                return t;
            }
            self.close_turn(at, out);
        }
        self.cur.turn_no += 1;
        let label = format!("Turn {}", self.cur.turn_no);
        let id = self.new_task(
            TaskKind::Turn,
            label,
            summary::turn_title(std::iter::empty()),
            at,
            None,
            None,
            out,
        );
        if let Some(prev) = self.cur.last_main {
            self.link(prev, id, out);
        }
        for sub in std::mem::take(&mut self.cur.unlinked) {
            self.link(sub, id, out);
        }
        self.cur.last_main = Some(id);
        self.cur.open_turn = Some(id);
        id
    }

    fn all_tools_finished(&self, id: TaskId) -> bool {
        let tools = &self.tasks[id as usize].tools;
        !tools.is_empty() && tools.iter().all(|t| t.status != Status::Running)
    }

    fn close_turn(&mut self, at: i64, out: &mut Vec<Delta>) {
        let Some(id) = self.cur.open_turn.take() else { return };
        let task = &mut self.tasks[id as usize];
        let end = task.tools.iter().filter_map(|t| t.ended_ms).max().unwrap_or(at);
        task.header.status = Status::Done;
        task.header.ended_ms = Some(end);
        self.emit_task(id, out);
    }

    fn ensure_subagent(&mut self, agent_id: &str, agent_type: &str, at: i64, out: &mut Vec<Delta>) -> TaskId {
        if let Some(&id) = self.cur.subagents.get(agent_id) {
            return id;
        }
        let spawn = self
            .cur
            .pending_spawns
            .iter()
            .position(|p| p.subagent_type.as_deref() == Some(agent_type))
            .or(if self.cur.pending_spawns.is_empty() {
                None
            } else {
                Some(0)
            })
            .map(|i| self.cur.pending_spawns.remove(i));

        let title = spawn
            .as_ref()
            .and_then(|s| s.description.clone())
            .unwrap_or_else(|| agent_type.to_string());
        let label = format!("Subagent · {agent_type}");
        let id = self.new_task(
            TaskKind::Subagent,
            label,
            title,
            at,
            Some(agent_id.to_string()),
            Some(agent_type.to_string()),
            out,
        );
        self.cur.subagents.insert(agent_id.to_string(), id);

        let parent = spawn.as_ref().map(|s| s.turn).or(self.cur.last_main);
        if let Some(p) = parent {
            self.link(p, id, out);
        }
        if let Some(s) = spawn
            && let Some(&(t, i)) = self.cur.tools.get(&s.tool_use_id)
        {
            self.tasks[t as usize].tools[i].spawned_task = Some(id);
            self.emit_tool(t, i, out);
        }
        id
    }

    fn add_tool(
        &mut self,
        task: TaskId,
        tool_use_id: Option<&str>,
        name: &str,
        input: &Value,
        at: i64,
        out: &mut Vec<Delta>,
    ) {
        let id = match tool_use_id {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                self.cur.anon += 1;
                format!("anon-{}", self.cur.anon)
            }
        };
        if self.cur.tools.contains_key(&id) {
            return; // duplicate delivery
        }
        let call = ToolCall {
            id: id.clone(),
            name: name.to_string(),
            target: summary::tool_target(name, input),
            status: Status::Running,
            started_ms: at,
            ended_ms: None,
            input: summary::clip_value(input, summary::OUTPUT_PREVIEW),
            output: None,
            error: None,
            spawned_task: None,
        };
        let t = &mut self.tasks[task as usize];
        t.tools.push(call);
        let idx = t.tools.len() - 1;
        t.header.tool_count += 1;
        if t.header.kind == TaskKind::Turn {
            t.header.title = summary::turn_title(t.tools.iter().map(|c| c.name.as_str()));
        }
        self.cur.tools.insert(id.clone(), (task, idx));

        if SPAWN_TOOLS.contains(&name) {
            self.cur.pending_spawns.push(PendingSpawn {
                turn: task,
                tool_use_id: id,
                subagent_type: input.get("subagent_type").and_then(Value::as_str).map(str::to_string),
                description: input.get("description").and_then(Value::as_str).map(|d| clip(d, 80)),
            });
        }
        self.emit_task(task, out);
        self.emit_tool(task, idx, out);
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_tool(
        &mut self,
        ev: &Event,
        tool_use_id: Option<&str>,
        name: &str,
        status: Status,
        output: Option<String>,
        error: Option<String>,
        out: &mut Vec<Delta>,
    ) {
        let found = match tool_use_id {
            Some(id) if !id.is_empty() => self.cur.tools.get(id).copied(),
            _ => self.find_running_by_name(ev, name),
        };
        let Some((task, idx)) = found else { return };
        let call = &mut self.tasks[task as usize].tools[idx];
        if call.status != Status::Running {
            return; // duplicate delivery
        }
        call.status = status;
        call.ended_ms = Some(ev.at_ms);
        call.output = output;
        call.error = error;
        if status == Status::Failed {
            self.tasks[task as usize].header.failed_count += 1;
            self.emit_task(task, out);
        }
        self.emit_tool(task, idx, out);
    }

    /// Match a finish event that has no `tool_use_id` to the oldest running
    /// call with the same name in the same agent.
    fn find_running_by_name(&self, ev: &Event, name: &str) -> Option<(TaskId, usize)> {
        let task = match &ev.agent_id {
            Some(aid) => *self.cur.subagents.get(aid)?,
            None => self.cur.open_turn.or(self.cur.last_main)?,
        };
        let idx = self.tasks[task as usize]
            .tools
            .iter()
            .position(|c| c.name == name && c.status == Status::Running)?;
        Some((task, idx))
    }

    fn interrupt_running(&mut self, at: i64, out: &mut Vec<Delta>) {
        for tid in 0..self.tasks.len() {
            let mut changed = false;
            for i in 0..self.tasks[tid].tools.len() {
                let c = &mut self.tasks[tid].tools[i];
                if c.status == Status::Running {
                    c.status = Status::Interrupted;
                    c.ended_ms = Some(at);
                    self.emit_tool(tid as TaskId, i, out);
                }
            }
            let h = &mut self.tasks[tid].header;
            if h.status == Status::Running {
                h.status = Status::Interrupted;
                h.ended_ms = Some(at);
                changed = true;
            }
            if changed {
                self.emit_task(tid as TaskId, out);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn new_task(
        &mut self,
        kind: TaskKind,
        label: String,
        title: String,
        at: i64,
        agent_id: Option<String>,
        agent_type: Option<String>,
        out: &mut Vec<Delta>,
    ) -> TaskId {
        let id = self.tasks.len() as TaskId;
        self.tasks.push(Task {
            header: TaskHeader {
                id,
                kind,
                label,
                title,
                status: Status::Running,
                started_ms: at,
                ended_ms: None,
                agent_id,
                agent_type,
                summary: None,
                reply: None,
                tool_count: 0,
                failed_count: 0,
            },
            tools: Vec::new(),
        });
        self.emit_task(id, out);
        id
    }

    fn link(&mut self, from: TaskId, to: TaskId, out: &mut Vec<Delta>) {
        let e = Edge { from, to };
        if from != to && !self.edges.contains(&e) {
            self.edges.push(e);
            out.push(Delta::Edge(e));
        }
    }

    fn emit_task(&self, id: TaskId, out: &mut Vec<Delta>) {
        let header = self.tasks[id as usize].header.clone();
        // Collapse repeated upserts of the same task within one apply().
        if let Some(Delta::Task { task }) = out
            .iter_mut()
            .rev()
            .find(|d| matches!(d, Delta::Task { task } if task.id == id))
        {
            *task = header;
        } else {
            out.push(Delta::Task { task: header });
        }
    }

    fn emit_tool(&self, task_id: TaskId, idx: usize, out: &mut Vec<Delta>) {
        out.push(Delta::Tool {
            task_id,
            tool: self.tasks[task_id as usize].tools[idx].clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Sim {
        g: SessionGraph,
        seq: u64,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                g: SessionGraph::new("sess-1", 0),
                seq: 0,
            }
        }
        fn ev(&mut self, agent: Option<&str>, kind: EventKind) -> Vec<Delta> {
            self.seq += 1;
            let e = Event {
                seq: self.seq,
                at_ms: self.seq as i64 * 100,
                session_id: "sess-1".into(),
                agent_id: agent.map(str::to_string),
                agent_type: agent.map(|_| "Explore".to_string()),
                cwd: Some("/repo".into()),
                kind,
            };
            self.g.apply(&e)
        }
        fn pre(&mut self, agent: Option<&str>, id: &str, name: &str, input: Value) {
            self.ev(
                agent,
                EventKind::ToolStarted {
                    tool_use_id: Some(id.into()),
                    name: name.into(),
                    input,
                },
            );
        }
        fn post(&mut self, agent: Option<&str>, id: &str, name: &str) {
            self.ev(
                agent,
                EventKind::ToolFinished {
                    tool_use_id: Some(id.into()),
                    name: name.into(),
                    response: json!("ok"),
                },
            );
        }
        fn kinds(&self) -> Vec<TaskKind> {
            self.g.tasks.iter().map(|t| t.header.kind).collect()
        }
        fn edges(&self) -> Vec<(TaskId, TaskId)> {
            self.g.edges.iter().map(|e| (e.from, e.to)).collect()
        }
    }

    #[test]
    fn groups_turns_and_subagents_into_a_dag() {
        let mut s = Sim::new();
        s.ev(
            None,
            EventKind::SessionStarted {
                source: Some("startup".into()),
                model: None,
            },
        );
        s.ev(
            None,
            EventKind::PromptSubmitted {
                prompt: "Fix the flaky auth test\nplease".into(),
            },
        );
        s.pre(None, "t1", "Read", json!({"file_path": "/repo/src/auth.rs"}));
        s.pre(None, "t2", "Read", json!({"file_path": "/repo/src/session.rs"}));
        s.pre(
            None,
            "t3",
            "Agent",
            json!({"subagent_type": "Explore", "description": "Find expiry checks"}),
        );
        s.post(None, "t1", "Read");
        s.post(None, "t2", "Read");
        s.ev(
            None,
            EventKind::SubagentStarted {
                agent_id: "a1".into(),
                agent_type: "Explore".into(),
            },
        );
        s.pre(Some("a1"), "t4", "Glob", json!({"pattern": "**/clock*.rs"}));
        s.post(Some("a1"), "t4", "Glob");
        s.ev(
            None,
            EventKind::SubagentStopped {
                agent_id: "a1".into(),
                agent_type: "Explore".into(),
                last_message: Some("Found 2".into()),
            },
        );
        s.post(None, "t3", "Agent");
        s.ev(None, EventKind::ToolBatchDone);
        s.pre(None, "t5", "Edit", json!({"file_path": "/repo/src/auth.rs"}));
        s.post(None, "t5", "Edit");
        s.ev(None, EventKind::ToolBatchDone);
        s.pre(None, "t6", "Bash", json!({"command": "cargo test"}));
        s.ev(
            None,
            EventKind::ToolFailed {
                tool_use_id: Some("t6".into()),
                name: "Bash".into(),
                error: "exit 101".into(),
            },
        );
        s.ev(None, EventKind::ToolBatchDone);
        s.ev(
            None,
            EventKind::Stopped {
                last_message: Some("Fixed.".into()),
            },
        );

        use TaskKind::*;
        assert_eq!(s.kinds(), vec![Prompt, Turn, Subagent, Turn, Turn]);
        assert_eq!(s.edges(), vec![(0, 1), (1, 2), (1, 3), (2, 3), (3, 4)]);

        let g = &s.g;
        assert_eq!(g.title, "Fix the flaky auth test");
        assert_eq!(g.status, SessionStatus::Idle);
        assert_eq!(g.tasks[1].header.title, "Read ×2 · Agent");
        assert_eq!(g.tasks[1].tools[2].spawned_task, Some(2));
        assert_eq!(g.tasks[2].header.title, "Find expiry checks");
        assert_eq!(g.tasks[2].header.label, "Subagent · Explore");
        assert_eq!(g.tasks[2].tools[0].target, "**/clock*.rs");
        assert_eq!(g.tasks[4].header.failed_count, 1);
        assert_eq!(g.tasks[4].tools[0].error.as_deref(), Some("exit 101"));
        assert_eq!(g.tasks[0].header.reply.as_deref(), Some("Fixed."));
        assert_eq!(
            g.tasks[0].header.summary.as_deref(),
            Some("Fix the flaky auth test\nplease")
        );
        assert_eq!(g.tasks[2].header.reply.as_deref(), Some("Found 2"));
        assert!(g.tasks.iter().all(|t| t.header.status == Status::Done));
        assert_eq!(g.summary().tool_count, 6);
    }

    #[test]
    fn session_is_named_after_the_first_real_prompt() {
        let mut s = Sim::new();
        s.ev(
            None,
            EventKind::PromptSubmitted {
                prompt: "<task-notification><summary>Build done</summary></task-notification>".into(),
            },
        );
        assert_eq!(s.g.title, "Build done");
        assert_eq!(s.g.tasks[0].header.label, "Notification");
        assert_eq!(s.g.tasks[0].header.status, Status::Running);
        s.ev(
            None,
            EventKind::PromptSubmitted {
                prompt: "<system-reminder>ctx</system-reminder>\nDeploy it".into(),
            },
        );
        assert_eq!(s.g.title, "Deploy it");
        assert_eq!(
            s.g.tasks[0].header.status,
            Status::Done,
            "a new prompt closes the previous one"
        );
        s.ev(
            None,
            EventKind::PromptSubmitted {
                prompt: "And the docs".into(),
            },
        );
        assert_eq!(s.g.title, "Deploy it", "later prompts don't rename the session");
    }

    #[test]
    fn splits_turns_without_batch_events() {
        let mut s = Sim::new();
        s.ev(None, EventKind::PromptSubmitted { prompt: "go".into() });
        s.pre(None, "a", "Read", json!({}));
        s.pre(None, "b", "Read", json!({}));
        s.post(None, "a", "Read");
        s.post(None, "b", "Read");
        s.pre(None, "c", "Edit", json!({}));
        s.post(None, "c", "Edit");
        use TaskKind::*;
        assert_eq!(s.kinds(), vec![Prompt, Turn, Turn]);
        assert_eq!(s.g.tasks[1].tools.len(), 2);
    }

    #[test]
    fn duplicate_and_anonymous_events_are_handled() {
        let mut s = Sim::new();
        s.pre(None, "x", "Read", json!({}));
        s.pre(None, "x", "Read", json!({}));
        assert_eq!(s.g.tasks[0].tools.len(), 1);
        s.ev(
            None,
            EventKind::ToolStarted {
                tool_use_id: None,
                name: "Grep".into(),
                input: json!({}),
            },
        );
        s.ev(
            None,
            EventKind::ToolFinished {
                tool_use_id: None,
                name: "Grep".into(),
                response: Value::Null,
            },
        );
        assert_eq!(s.g.tasks[0].tools[1].status, Status::Done);
        s.ev(None, EventKind::SessionEnded { reason: None });
        assert_eq!(s.g.tasks[0].tools[0].status, Status::Interrupted);
    }

    #[test]
    fn deltas_round_trip_through_json() {
        let mut s = Sim::new();
        let deltas = s.ev(None, EventKind::PromptSubmitted { prompt: "hi".into() });
        let text = serde_json::to_string(&deltas).unwrap();
        let back: Vec<Delta> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, deltas);
        assert!(text.contains(r#""op":"task""#));
    }

    #[test]
    fn usage_is_attributed_to_turns_followers_and_prompts() {
        use crate::usage::Usage;
        let mut s = Sim::new();
        s.ev(None, EventKind::PromptSubmitted { prompt: "go".into() });
        s.pre(None, "t1", "Read", json!({"file_path": "/a"}));
        s.ev(
            None,
            EventKind::SubagentStarted {
                agent_id: "ag".into(),
                agent_type: "Explore".into(),
            },
        );
        s.pre(Some("ag"), "t2", "Grep", json!({"pattern": "x"}));
        let usage = |s: &mut Sim, id: &str, tools: &[&str], agent: Option<&str>, at: i64, out: u64| {
            let u = Usage {
                message_id: id.into(),
                model: "claude-opus-5-5".into(),
                at_ms: at,
                output: out,
                tool_use_ids: tools.iter().map(|t| t.to_string()).collect(),
                agent_id: agent.map(str::to_string),
                ..Default::default()
            };
            s.ev(None, EventKind::Usage(u))
        };
        let turn = s.g.tools_owner("t1").unwrap();
        let follower = s.g.tools_owner("t2").unwrap();
        let prompt =
            s.g.tasks
                .iter()
                .find(|t| t.header.kind == TaskKind::Prompt)
                .unwrap()
                .header
                .id;

        usage(&mut s, "m1", &["t1"], None, 150, 10);
        usage(&mut s, "m2", &[], Some("ag"), 450, 20);
        usage(&mut s, "m3", &[], None, 9_999, 30); // the final reply
        let d = usage(&mut s, "m1", &["t1"], None, 150, 99); // a later line of m1
        let by_id = |id: &str| s.g.usage.iter().find(|u| u.message_id == id).unwrap().clone();
        assert_eq!(by_id("m1").task, Some(turn));
        assert_eq!(by_id("m2").task, Some(follower));
        assert_eq!(by_id("m3").task, Some(prompt));
        assert_eq!(s.g.usage.len(), 3, "merged by message id");
        assert_eq!(by_id("m1").output, 99);
        assert!(
            matches!(&d[..], [Delta::Usage(u)] if u.output == 99),
            "usage doesn't touch the session"
        );
        let updated = s.g.updated_ms;
        usage(&mut s, "m4", &[], None, 1_000_000, 1);
        assert_eq!(s.g.updated_ms, updated);
    }
}
