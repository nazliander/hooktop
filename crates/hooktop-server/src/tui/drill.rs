//! What's behind a row on the Statistics tab.
//!
//! Each panel's rows open a [`Drill`]: a few facts about the row, a short
//! note when the numbers need explaining, and a list of the things that
//! make it up (calls, turns, stretches of time). Each item can point at a
//! task or tool call, which `enter` shows in the Timeline.

use hooktop_core::graph::{SessionGraph, Status, TaskId, TaskKind};
use hooktop_core::pricing::{Pricing, format_tokens, format_usd, model_key};
use hooktop_core::stats::{CallStat, Reading, Stats, Stretch};
use hooktop_core::view;

/// The Statistics panels that can be drilled into, in `tab` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Tools,
    Time,
    Models,
    Slowest,
    Prompts,
    Ram,
    Cpu,
    Followers,
}

impl Panel {
    pub const ALL: [Panel; 8] = [
        Panel::Tools,
        Panel::Time,
        Panel::Models,
        Panel::Slowest,
        Panel::Prompts,
        Panel::Ram,
        Panel::Cpu,
        Panel::Followers,
    ];

    pub fn step(self, forward: bool) -> Option<Panel> {
        let i = Panel::ALL.iter().position(|p| *p == self)?;
        if forward {
            Panel::ALL.get(i + 1).copied()
        } else {
            i.checked_sub(1).map(|j| Panel::ALL[j])
        }
    }

    /// How many rows the panel shows for these stats.
    pub fn len(self, st: &Stats) -> usize {
        match self {
            Panel::Tools => st.mix.len(),
            Panel::Time => 3,
            Panel::Models => st.costs.by_model.len(),
            Panel::Slowest => st.slowest.len(),
            Panel::Prompts => st.longest_prompts.len(),
            Panel::Ram => st.ram_eaters.len(),
            Panel::Cpu => st.cpu_hogs.len(),
            Panel::Followers => st.follower_runs.len(),
        }
    }
}

/// A task, or one tool call in it.
pub type Target = (TaskId, Option<usize>);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Item {
    /// What it is (`Bash`, `Turn 3`, `♞ Explorer`).
    pub label: String,
    /// More about it, dimmed.
    pub detail: String,
    /// The number it's ranked by, right-aligned.
    pub value: String,
    pub failed: bool,
    pub target: Option<Target>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Drill {
    pub title: String,
    pub facts: Vec<(String, String)>,
    pub note: Option<String>,
    pub items_title: String,
    pub items: Vec<Item>,
}

/// The drill for row `row` of `panel`, or `None` if there's no such row.
pub fn build(g: &SessionGraph, st: &Stats, pricing: &Pricing, panel: Panel, row: usize) -> Option<Drill> {
    match panel {
        Panel::Tools => st.mix.get(row).map(|m| tool(g, st, &m.name)),
        Panel::Time => Some(time(g, st, row)),
        Panel::Models => st.costs.by_model.get(row).map(|m| model(g, pricing, &m.model)),
        Panel::Slowest => st.slowest.get(row).map(|c| call(g, st, c)),
        Panel::Ram => st.ram_eaters.get(row).map(|c| call(g, st, c)),
        Panel::Cpu => st.cpu_hogs.get(row).map(|c| call(g, st, c)),
        Panel::Prompts => st.longest_prompts.get(row).map(|p| prompt(g, st, p.task)),
        Panel::Followers => st.follower_runs.get(row).map(|f| follower(g, st, f.task)),
    }
}

// ── Pieces ─────────────────────────────────────────────────────────────

/// `♞ Explorer`, or `🦊 Watcher` for the main agent.
fn who(agent_type: Option<&str>) -> String {
    match agent_type {
        None => format!("{} Watcher", super::ui::BOSS_ICON),
        Some(a) => format!("{} {}", super::ui::FOLLOWER_ICON, view::agent_name(Some(a))),
    }
}

/// A short name for a task: a prompt's first line, `Turn 3`, or the
/// follower's name and job.
fn task_name(g: &SessionGraph, id: TaskId) -> String {
    let Some(t) = g.task(id) else { return "?".into() };
    let h = &t.header;
    match h.kind {
        TaskKind::Prompt => format!("❯ {}", view::first_line(&h.title)),
        TaskKind::Turn => h.label.clone(),
        TaskKind::Subagent => format!("{} · {}", who(h.agent_type.as_deref()), view::first_line(&h.title)),
    }
}

/// `Bash cargo test` for a call.
fn call_name(g: &SessionGraph, (task, tool): Target) -> String {
    match tool.and_then(|i| g.task(task)?.tools.get(i)) {
        Some(c) => format!("{} {}", view::tool_name(&c.name), view::first_line(&c.target)),
        None => task_name(g, task),
    }
}

fn status_word(s: Status) -> &'static str {
    match s {
        Status::Running => "running",
        Status::Done => "done",
        Status::Failed => "failed",
        Status::Interrupted => "stopped",
    }
}

/// Where a call sits: `Turn 3 of ❯ Fix the flaky test`, or a follower.
fn place(g: &SessionGraph, task: TaskId) -> String {
    let Some(t) = g.task(task) else { return String::new() };
    match t.header.kind {
        TaskKind::Turn => match view::prompt_of_turn(g, task) {
            Some(p) => format!("{} of {}", t.header.label, task_name(g, p)),
            None => t.header.label.clone(),
        },
        _ => task_name(g, task),
    }
}

fn offset(g: &SessionGraph, at: i64) -> String {
    format!("+{}", view::duration((at - g.started_ms).max(0)))
}

fn call_item(g: &SessionGraph, c: &CallStat, value: String) -> Item {
    Item {
        label: format!("{} {}", view::tool_name(&c.name), c.target),
        detail: format!("{} · {}", who(c.agent_type.as_deref()), offset(g, call_started(g, c))),
        value,
        failed: c.status == Status::Failed,
        target: Some((c.task, Some(c.tool))),
    }
}

fn call_started(g: &SessionGraph, c: &CallStat) -> i64 {
    g.task(c.task)
        .and_then(|t| t.tools.get(c.tool))
        .map_or(g.started_ms, |x| x.started_ms)
}

// ── Panels ─────────────────────────────────────────────────────────────

fn tool(g: &SessionGraph, st: &Stats, name: &str) -> Drill {
    let mut calls: Vec<&CallStat> = st.all_calls.iter().filter(|c| c.name == name).collect();
    calls.sort_by_key(|c| std::cmp::Reverse(c.took_ms));
    let total: i64 = calls.iter().map(|c| c.took_ms).sum();
    let failed = calls.iter().filter(|c| c.status == Status::Failed).count();
    let n = calls.len().max(1) as i64;
    Drill {
        title: format!("{name} · every call"),
        facts: vec![
            ("Calls".into(), calls.len().to_string()),
            ("Failed".into(), failed.to_string()),
            ("Total time".into(), view::duration(total)),
            ("Average".into(), view::duration(total / n)),
        ],
        note: None,
        items_title: "Calls, slowest first".into(),
        items: calls
            .into_iter()
            .map(|c| {
                call_item(
                    g,
                    c,
                    format!("{} · {}", view::duration(c.took_ms), status_word(c.status)),
                )
            })
            .collect(),
    }
}

fn time(g: &SessionGraph, st: &Stats, row: usize) -> Drill {
    let pct = |ms: i64| {
        if st.wall_ms > 0 {
            format!(
                "{:.0}% of {}",
                ms as f64 * 100.0 / st.wall_ms as f64,
                view::duration(st.wall_ms)
            )
        } else {
            String::new()
        }
    };
    match row {
        0 => Drill {
            title: "Thinking".into(),
            facts: vec![
                ("Total".into(), view::duration(st.time.thinking_ms)),
                ("Share".into(), pct(st.time.thinking_ms)),
                ("Stretches".into(), st.thinking.len().to_string()),
            ],
            note: Some(
                "Claude working with no tool running: the model reading results, thinking and writing. \
                 hooktop sees when and how many tokens, never what it thought."
                    .into(),
            ),
            items_title: "Longest stretches".into(),
            items: st.thinking.iter().map(|x| stretch_item(g, x, true)).collect(),
        },
        1 => Drill {
            title: "Tools".into(),
            facts: vec![
                ("Total".into(), view::duration(st.time.tools_ms)),
                ("Share".into(), pct(st.time.tools_ms)),
                ("Calls".into(), st.calls.to_string()),
            ],
            note: Some(
                "Time with at least one tool call running. Parallel calls count once, and a follower's \
                 run counts as its own calls plus thinking."
                    .into(),
            ),
            items_title: "Slowest calls".into(),
            items: st
                .slowest
                .iter()
                .map(|c| call_item(g, c, view::duration(c.took_ms)))
                .collect(),
        },
        _ => Drill {
            title: "Waiting for you".into(),
            facts: vec![
                ("Total".into(), view::duration(st.time.waiting_ms)),
                ("Share".into(), pct(st.time.waiting_ms)),
                ("Pauses".into(), st.waiting.len().to_string()),
            ],
            note: Some("Time between Claude finishing and your next prompt.".into()),
            items_title: "Longest pauses".into(),
            items: st.waiting.iter().map(|x| stretch_item(g, x, false)).collect(),
        },
    }
}

fn stretch_item(g: &SessionGraph, x: &Stretch, thinking: bool) -> Item {
    let around = match (x.before, x.after) {
        (Some(b), Some(a)) => format!("after {} → before {}", call_name(g, b), call_name(g, a)),
        (Some(b), None) => format!("after {}", call_name(g, b)),
        (None, Some(a)) => format!("before {}", call_name(g, a)),
        (None, None) => String::new(),
    };
    let mut value = view::duration(x.ms());
    if thinking && x.model_calls > 0 {
        value = format!("{value} · {} · {}", format_tokens(x.tokens), format_usd(x.cost));
    }
    Item {
        label: match (thinking, x.task) {
            (true, Some(t)) => task_name(g, t),
            (true, None) => "Watcher".into(),
            (false, _) => offset(g, x.start_ms),
        },
        detail: around,
        value,
        failed: false,
        target: if thinking {
            x.after.or(x.before).or(x.task.map(|t| (t, None)))
        } else {
            x.after
        },
    }
}

fn model(g: &SessionGraph, pricing: &Pricing, model: &str) -> Drill {
    use std::collections::HashMap;
    let key = model_key(model);
    let mut by_task: HashMap<Option<TaskId>, (f64, u64, usize)> = HashMap::new();
    let (mut input, mut output, mut cache, mut calls, mut cost) = (0u64, 0u64, 0u64, 0usize, 0.0);
    for u in g.usage.iter().filter(|u| model_key(&u.model) == key) {
        let c = pricing.cost(u).unwrap_or(0.0);
        let e = by_task.entry(u.task).or_default();
        e.0 += c;
        e.1 += u.total();
        e.2 += 1;
        input += u.input;
        output += u.output;
        cache += u.cache_read + u.cache_write_5m + u.cache_write_1h;
        calls += 1;
        cost += c;
    }
    let mut rows: Vec<_> = by_task.into_iter().collect();
    rows.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
    Drill {
        title: format!("{key} · where it was used"),
        facts: vec![
            ("Model calls".into(), calls.to_string()),
            ("Cost".into(), format_usd(cost)),
            ("Input".into(), format_tokens(input)),
            ("Cache".into(), format_tokens(cache)),
            ("Output".into(), format_tokens(output)),
        ],
        note: Some("At API list prices. Cache reads are the cheap part of most sessions.".into()),
        items_title: "Prompts, turns and followers, most expensive first".into(),
        items: rows
            .into_iter()
            .map(|(task, (c, tokens, n))| Item {
                label: task.map_or("not matched to a task".into(), |t| task_name(g, t)),
                detail: format!("{n} call{} · {}", if n == 1 { "" } else { "s" }, format_tokens(tokens)),
                value: format_usd(c),
                failed: false,
                target: task.map(|t| (t, None)),
            })
            .collect(),
    }
}

fn call(g: &SessionGraph, st: &Stats, c: &CallStat) -> Drill {
    let raw = g.task(c.task).and_then(|t| t.tools.get(c.tool));
    let mut facts = vec![
        ("Who".into(), who(c.agent_type.as_deref())),
        ("In".into(), place(g, c.task)),
        ("Status".into(), status_word(c.status).into()),
        ("Started".into(), offset(g, call_started(g, c))),
        ("Took".into(), view::duration(c.took_ms)),
    ];
    if let Some(r) = c.peak_rss {
        facts.push(("Peak RAM".into(), bytes(r)));
    }
    if let Some(cpu) = c.avg_cpu {
        facts.push(("Avg CPU".into(), view::cores(cpu)));
    }
    let turn_cost = st.costs.of(c.task);
    if turn_cost > 0.0 {
        facts.push(("Turn cost".into(), format_usd(turn_cost)));
    }
    if let Some(e) = raw.and_then(|x| x.error.as_deref()) {
        facts.push(("Error".into(), view::first_line(e).into()));
    } else if let Some(o) = raw.and_then(|x| x.output.as_deref()) {
        facts.push(("Output".into(), view::first_line(o).into()));
    }
    let note = match c.reading {
        Reading::Own => None,
        Reading::Shared => {
            Some("≈ shared: another call was running when CPU/RAM was read, so these numbers cover both.".into())
        }
        Reading::Borrowed => Some(
            "↳ borrowed: the call was shorter than a reading, so it uses the first reading after it started.".into(),
        ),
    };
    let mut runs: Vec<&CallStat> = st
        .all_calls
        .iter()
        .filter(|x| x.name == c.name && x.target == c.target)
        .collect();
    runs.sort_by_key(|x| std::cmp::Reverse(x.took_ms));
    Drill {
        title: format!(
            "{} {}",
            view::tool_name(&c.name),
            raw.map_or(c.target.as_str(), |x| x.target.as_str())
        ),
        facts,
        note,
        items_title: if runs.len() > 1 {
            format!("All {} runs of this command", runs.len())
        } else {
            "This call".into()
        },
        items: runs
            .into_iter()
            .map(|x| {
                let mut v = format!("{} · {}", view::duration(x.took_ms), status_word(x.status));
                if let Some(r) = x.peak_rss {
                    v.push_str(&format!(" · {}", bytes(r)));
                }
                call_item(g, x, v)
            })
            .collect(),
    }
}

fn prompt(g: &SessionGraph, st: &Stats, id: TaskId) -> Drill {
    let turns = view::prompt_groups(g)
        .into_iter()
        .find(|(p, _)| *p == Some(id))
        .map(|(_, t)| t)
        .unwrap_or_default();
    let (cost, tokens) = st.costs.prompt(g, id);
    let p = st.longest_prompts.iter().find(|p| p.task == id);
    let h = g.task(id).map(|t| &t.header);
    Drill {
        title: task_name(g, id),
        facts: vec![
            ("Took".into(), p.map_or(String::new(), |p| view::duration(p.took_ms))),
            ("Turns".into(), turns.len().to_string()),
            ("Tools".into(), p.map_or(0, |p| p.tools).to_string()),
            (
                "Cost".into(),
                format!("{} · {}", format_usd(cost), format_tokens(tokens)),
            ),
            ("Started".into(), h.map_or(String::new(), |h| offset(g, h.started_ms))),
        ],
        note: None,
        items_title: "Turns".into(),
        items: turns
            .iter()
            .filter_map(|t| g.task(*t))
            .map(|t| {
                let h = &t.header;
                let took = h.ended_ms.map_or(0, |e| e - h.started_ms);
                let c = st.costs.of(h.id);
                Item {
                    label: h.label.clone(),
                    detail: view::first_line(&h.title).to_string(),
                    value: if c > 0.0 {
                        format!("{} · {}", view::duration(took), format_usd(c))
                    } else {
                        view::duration(took)
                    },
                    failed: h.failed_count > 0,
                    target: Some((h.id, None)),
                }
            })
            .collect(),
    }
}

fn follower(g: &SessionGraph, st: &Stats, id: TaskId) -> Drill {
    let f = st.follower_runs.iter().find(|f| f.task == id);
    let calls: Vec<&CallStat> = st.all_calls.iter().filter(|c| c.task == id).collect();
    Drill {
        title: task_name(g, id),
        facts: vec![
            ("Took".into(), f.map_or(String::new(), |f| view::duration(f.took_ms))),
            ("Tools".into(), calls.len().to_string()),
            ("Cost".into(), format_usd(st.costs.of(id))),
            ("Status".into(), f.map_or("", |f| status_word(f.status)).into()),
        ],
        note: None,
        items_title: "Its calls, in order".into(),
        items: calls
            .into_iter()
            .map(|c| {
                call_item(
                    g,
                    c,
                    format!("{} · {}", view::duration(c.took_ms), status_word(c.status)),
                )
            })
            .collect(),
    }
}

fn bytes(b: u64) -> String {
    let mb = b as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}
