//! What a session looks like on screen, independent of how it's drawn:
//! the party tree and history rows the terminal UI renders.
//!
//! The cast: the main agent is the **Boss**, Watcher the fox; every
//! subagent it spawns is a **Follower**, a knight. Sessions are
//! **Connections**: open while Claude works or waits for you, closed once
//! the session ends.

use std::collections::HashSet;

use crate::graph::{SessionGraph, SessionStatus, Status, Task, TaskId, TaskKind, ToolCall};

/// The Boss's name.
pub const MAIN_AGENT: &str = "Watcher";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Boss,
    Follower,
}

impl Role {
    pub fn of(agent_type: Option<&str>) -> Self {
        if agent_type.is_some() {
            Role::Follower
        } else {
            Role::Boss
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::Boss => "Boss",
            Role::Follower => "Follower",
        }
    }
}

/// Watcher for the Boss; a friendlier name for some follower types.
pub fn agent_name(agent_type: Option<&str>) -> String {
    let Some(t) = agent_type else {
        return MAIN_AGENT.to_string();
    };
    match t.trim().to_ascii_lowercase().as_str() {
        "explore" => "Explorer".into(),
        "plan" => "Planner".into(),
        "general-purpose" => "Squire".into(),
        _ => t.to_string(),
    }
}

/// A session as a connection.
pub fn connection_text(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => "Open · working",
        SessionStatus::Idle => "Open · waiting for you",
        SessionStatus::Ended => "Closed",
    }
}

pub fn is_open(status: SessionStatus) -> bool {
    status != SessionStatus::Ended
}

// ── The party: Boss and followers ─────────────────────────────────────

/// One agent in the party. `key` is `None` for the Boss, else the
/// follower's subagent task id.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub key: Option<TaskId>,
    pub depth: usize,
    pub agent_type: Option<String>,
    pub title: String,
    pub status: Status,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub tools: u32,
    pub failed: u32,
}

impl Member {
    pub fn role(&self) -> Role {
        Role::of(self.agent_type.as_deref())
    }
    pub fn name(&self) -> String {
        agent_name(self.agent_type.as_deref())
    }
}

/// The Boss first, then every follower depth-first under whoever spawned it.
pub fn party(g: &SessionGraph) -> Vec<Member> {
    let (mut tools, mut failed) = (0, 0);
    for t in g.tasks.iter().filter(|t| t.header.kind != TaskKind::Subagent) {
        tools += t.header.tool_count;
        failed += t.header.failed_count;
    }
    let boss = Member {
        key: None,
        depth: 0,
        agent_type: None,
        title: g.model.clone().unwrap_or_else(|| "Claude".into()),
        status: if g.status == SessionStatus::Running {
            Status::Running
        } else {
            Status::Done
        },
        started_ms: g.started_ms,
        ended_ms: (g.status != SessionStatus::Running).then_some(g.updated_ms),
        tools,
        failed,
    };

    // Parent of each follower: the subagent that spawned it, or the Boss.
    let parent_of = |id: TaskId| -> Option<TaskId> {
        let from = g.edges.iter().find(|e| e.to == id)?.from;
        g.task(from)
            .filter(|t| t.header.kind == TaskKind::Subagent)
            .map(|_| from)
    };
    let followers: Vec<&Task> = g.tasks.iter().filter(|t| t.header.kind == TaskKind::Subagent).collect();

    let mut out = vec![boss];
    fn walk(
        parent: Option<TaskId>,
        depth: usize,
        followers: &[&Task],
        parent_of: &dyn Fn(TaskId) -> Option<TaskId>,
        out: &mut Vec<Member>,
    ) {
        for t in followers.iter().filter(|t| parent_of(t.header.id) == parent) {
            let h = &t.header;
            out.push(Member {
                key: Some(h.id),
                depth,
                agent_type: Some(h.agent_type.clone().unwrap_or_else(|| "agent".into())),
                title: h.title.clone(),
                status: h.status,
                started_ms: h.started_ms,
                ended_ms: h.ended_ms,
                tools: h.tool_count,
                failed: h.failed_count,
            });
            walk(Some(h.id), depth + 1, followers, parent_of, out);
        }
    }
    walk(None, 1, &followers, &parent_of, &mut out);
    out
}

// ── History rows ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Prompt,
    Command,
    Notification,
    System,
    Turn,
    Tool,
}

/// One visible line of a history: a prompt, a turn, or a tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: RowKind,
    pub depth: usize,
    pub task: TaskId,
    /// Index into the task's tools, for tool rows.
    pub tool: Option<usize>,
    pub expandable: bool,
    pub expanded: bool,
    pub status: Status,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
}

impl Row {
    /// Stable identity for keeping the cursor on the same row as rows change.
    pub fn key(&self, g: &SessionGraph) -> String {
        match self.tool {
            Some(i) => format!("c{}:{}", self.task, g.tasks[self.task as usize].tools[i].id),
            None => format!("t{}", self.task),
        }
    }

    pub fn tool_call<'a>(&self, g: &'a SessionGraph) -> Option<&'a ToolCall> {
        g.task(self.task)?.tools.get(self.tool?)
    }
}

fn prompt_kind(t: &Task) -> RowKind {
    match t.header.label.as_str() {
        "Command" => RowKind::Command,
        "Notification" => RowKind::Notification,
        "System" => RowKind::System,
        _ => RowKind::Prompt,
    }
}

/// Each prompt with the turns that answered it (turns before any prompt
/// get a group of their own).
pub fn prompt_groups(g: &SessionGraph) -> Vec<(Option<TaskId>, Vec<TaskId>)> {
    let mut groups: Vec<(Option<TaskId>, Vec<TaskId>)> = Vec::new();
    for t in &g.tasks {
        match t.header.kind {
            TaskKind::Prompt => groups.push((Some(t.header.id), Vec::new())),
            TaskKind::Turn => {
                if groups.is_empty() {
                    groups.push((None, Vec::new()));
                }
                groups.last_mut().unwrap().1.push(t.header.id);
            }
            TaskKind::Subagent => {}
        }
    }
    groups
}

/// The prompt a turn belongs to.
pub fn prompt_of_turn(g: &SessionGraph, turn: TaskId) -> Option<TaskId> {
    prompt_groups(g)
        .into_iter()
        .find(|(_, turns)| turns.contains(&turn))
        .and_then(|(p, _)| p)
}

/// The rows to show for one agent (`None` = the Boss), given which tasks
/// are expanded. `hide_notices` drops notification and system prompts.
pub fn history_rows(
    g: &SessionGraph,
    agent: Option<TaskId>,
    expanded: &HashSet<TaskId>,
    hide_notices: bool,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let tool_rows = |rows: &mut Vec<Row>, t: &Task, depth: usize| {
        for (i, c) in t.tools.iter().enumerate() {
            rows.push(Row {
                kind: RowKind::Tool,
                depth,
                task: t.header.id,
                tool: Some(i),
                expandable: false,
                expanded: false,
                status: c.status,
                started_ms: c.started_ms,
                ended_ms: c.ended_ms,
            });
        }
    };

    if let Some(id) = agent {
        if let Some(t) = g.task(id) {
            tool_rows(&mut rows, t, 0);
        }
        return rows;
    }

    for (prompt, turns) in prompt_groups(g) {
        let mut depth = 0;
        if let Some(pid) = prompt {
            let p = &g.tasks[pid as usize];
            let kind = prompt_kind(p);
            if hide_notices && matches!(kind, RowKind::Notification | RowKind::System) {
                continue;
            }
            let open = expanded.contains(&pid);
            let last_end = turns.last().and_then(|t| g.tasks[*t as usize].header.ended_ms);
            rows.push(Row {
                kind,
                depth: 0,
                task: pid,
                tool: None,
                expandable: !turns.is_empty(),
                expanded: open,
                status: p.header.status,
                started_ms: p.header.started_ms,
                ended_ms: p.header.ended_ms.or(if p.header.status == Status::Running {
                    None
                } else {
                    last_end
                }),
            });
            if !open {
                continue;
            }
            depth = 1;
        }
        for tid in turns {
            let t = &g.tasks[tid as usize];
            let open = expanded.contains(&tid);
            rows.push(Row {
                kind: RowKind::Turn,
                depth,
                task: tid,
                tool: None,
                expandable: !t.tools.is_empty(),
                expanded: open,
                status: t.header.status,
                started_ms: t.header.started_ms,
                ended_ms: t.header.ended_ms,
            });
            if open {
                tool_rows(&mut rows, t, depth + 1);
            }
        }
    }
    rows
}

// ── Formatting ─────────────────────────────────────────────────────────

/// `850 ms`, `4.2 s`, `12 s`, `3m 05s`, `1h 07m`.
pub fn duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        return format!("{ms} ms");
    }
    if ms < 60_000 {
        let s = ms as f64 / 1000.0;
        return if ms < 10_000 {
            format!("{s:.1} s")
        } else {
            format!("{:.0} s", s.floor())
        };
    }
    let m = ms / 60_000;
    let s = (ms % 60_000) / 1000;
    if m < 60 {
        return format!("{m}m {s:02}s");
    }
    format!("{}h {:02}m", m / 60, m % 60)
}

/// `mcp__server__do_thing` → `do_thing`.
pub fn tool_name(name: &str) -> &str {
    crate::summary::short_tool_name(name)
}

pub fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Delta, Edge, TaskHeader};
    use serde_json::json;

    fn header(id: TaskId, kind: TaskKind, label: &str) -> TaskHeader {
        TaskHeader {
            id,
            kind,
            label: label.into(),
            title: format!("t{id}"),
            status: Status::Done,
            started_ms: id as i64 * 10,
            ended_ms: Some(id as i64 * 10 + 5),
            agent_id: None,
            agent_type: None,
            summary: None,
            reply: None,
            tool_count: 0,
            failed_count: 0,
        }
    }

    fn tool(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "Read".into(),
            target: "x".into(),
            status: Status::Done,
            started_ms: 1,
            ended_ms: Some(2),
            input: json!({}),
            output: None,
            error: None,
            spawned_task: None,
        }
    }

    /// prompt 0 → turn 1 (spawns follower 2, which spawns follower 3) → turn 4;
    /// notification 5 → turn 6.
    fn session() -> SessionGraph {
        let mut g = SessionGraph::new("s", 0);
        let sub = |id, ty: &str| {
            let mut h = header(id, TaskKind::Subagent, "Subagent");
            h.agent_type = Some(ty.into());
            h
        };
        let deltas = vec![
            Delta::Task {
                task: header(0, TaskKind::Prompt, "Prompt"),
            },
            Delta::Task {
                task: TaskHeader {
                    tool_count: 2,
                    ..header(1, TaskKind::Turn, "Turn 1")
                },
            },
            Delta::Task {
                task: sub(2, "Explore"),
            },
            Delta::Task { task: sub(3, "Plan") },
            Delta::Task {
                task: header(4, TaskKind::Turn, "Turn 2"),
            },
            Delta::Task {
                task: header(5, TaskKind::Prompt, "Notification"),
            },
            Delta::Task {
                task: header(6, TaskKind::Turn, "Turn 3"),
            },
            Delta::Tool {
                task_id: 1,
                tool: tool("a"),
            },
            Delta::Tool {
                task_id: 1,
                tool: tool("b"),
            },
            Delta::Tool {
                task_id: 2,
                tool: tool("c"),
            },
            Delta::Edge(Edge { from: 0, to: 1 }),
            Delta::Edge(Edge { from: 1, to: 2 }),
            Delta::Edge(Edge { from: 2, to: 3 }),
            Delta::Edge(Edge { from: 1, to: 4 }),
            Delta::Edge(Edge { from: 4, to: 5 }),
            Delta::Edge(Edge { from: 5, to: 6 }),
        ];
        for d in &deltas {
            g.apply_delta(d);
            g.apply_delta(d); // idempotent
        }
        g
    }

    #[test]
    fn deltas_rebuild_the_graph() {
        let g = session();
        assert_eq!(g.tasks.len(), 7);
        assert_eq!(g.tasks[1].tools.len(), 2);
        assert_eq!(g.edges.len(), 6);
    }

    #[test]
    fn party_has_the_boss_then_followers_by_spawner() {
        let p = party(&session());
        let names: Vec<(String, usize, Role)> = p.iter().map(|m| (m.name(), m.depth, m.role())).collect();
        assert_eq!(
            names,
            vec![
                ("Watcher".into(), 0, Role::Boss),
                ("Explorer".into(), 1, Role::Follower),
                ("Planner".into(), 2, Role::Follower),
            ]
        );
        assert_eq!(p[0].tools, 2, "the Boss counts only its own turns' tools");
    }

    #[test]
    fn history_rows_fold_and_expand() {
        let g = session();
        let kinds = |open: &[TaskId], hide: bool| -> Vec<(RowKind, usize)> {
            history_rows(&g, None, &open.iter().copied().collect(), hide)
                .iter()
                .map(|r| (r.kind, r.depth))
                .collect()
        };
        use RowKind::*;
        assert_eq!(kinds(&[], false), vec![(Prompt, 0), (Notification, 0)]);
        assert_eq!(
            kinds(&[0], false),
            vec![(Prompt, 0), (Turn, 1), (Turn, 1), (Notification, 0)]
        );
        assert_eq!(
            kinds(&[0, 1], false),
            vec![
                (Prompt, 0),
                (Turn, 1),
                (Tool, 2),
                (Tool, 2),
                (Turn, 1),
                (Notification, 0)
            ]
        );
        assert_eq!(kinds(&[0], true), vec![(Prompt, 0), (Turn, 1), (Turn, 1)]);
        // A follower's history is its own tool calls.
        let sub = history_rows(&g, Some(2), &HashSet::new(), false);
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].tool_call(&g).unwrap().id, "c");
        assert_eq!(prompt_of_turn(&g, 4), Some(0));
        assert_eq!(prompt_of_turn(&g, 6), Some(5));
    }

    #[test]
    fn names_and_formatting() {
        assert_eq!(agent_name(None), "Watcher");
        assert_eq!(agent_name(Some("custom")), "custom");
        assert_eq!(connection_text(SessionStatus::Ended), "Closed");
        assert_eq!(duration(850), "850 ms");
        assert_eq!(duration(4200), "4.2 s");
        assert_eq!(duration(185_000), "3m 05s");
        assert_eq!(tool_name("mcp__ccd__update_plan"), "update_plan");
    }
}
