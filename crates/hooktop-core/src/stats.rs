//! Numbers about one session: what ran, what took longest, and what cost
//! the most CPU and memory.

use std::collections::HashMap;

use crate::graph::{SessionGraph, Status, TaskId, TaskKind};
use crate::pricing::Pricing;
use crate::view;

/// One tool call, with how long it took and what the machine did meanwhile.
#[derive(Debug, Clone, PartialEq)]
pub struct CallStat {
    pub task: TaskId,
    pub tool: usize,
    pub name: String,
    pub target: String,
    /// `None` for Watcher; the Follower's type otherwise.
    pub agent_type: Option<String>,
    pub status: Status,
    pub took_ms: i64,
    /// Highest memory and average CPU seen while it ran (needs samples).
    pub peak_rss: Option<u64>,
    pub avg_cpu: Option<f32>,
    /// How much those readings really belong to this call.
    pub reading: Reading,
    /// In a ranking, how many calls with the same tool and target this row
    /// stands for (the one shown is the biggest).
    pub repeats: usize,
}

/// Readings are per session, taken about once a second, so a call's CPU
/// and RAM are a lead, not an exact per-tool measurement. This says how
/// loose the lead is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Reading {
    /// Every reading was taken while only this call ran.
    #[default]
    Own,
    /// Another call was running when a reading was taken, so the numbers
    /// cover both.
    Shared,
    /// The call was shorter than the sampling interval: no reading fell
    /// inside it, so it borrows the first one after it started.
    Borrowed,
}

impl Reading {
    /// A one-character marker for tight columns, or `None` for
    /// [`Reading::Own`].
    pub fn mark(self) -> Option<&'static str> {
        match self {
            Reading::Own => None,
            Reading::Shared => Some("≈"),
            Reading::Borrowed => Some("↳"),
        }
    }

    /// A short label for the UI, or `None` for [`Reading::Own`].
    pub fn label(self) -> Option<&'static str> {
        match self {
            Reading::Own => None,
            Reading::Shared => Some("shared"),
            Reading::Borrowed => Some("borrowed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromptStat {
    pub task: TaskId,
    pub title: String,
    pub status: Status,
    pub took_ms: i64,
    pub turns: usize,
    pub tools: usize,
    /// The prompt and every turn answering it, at API prices.
    pub cost: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolMix {
    pub name: String,
    pub count: usize,
    pub failed: usize,
    pub total_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessStat {
    pub name: String,
    pub peak_cpu: f32,
    /// Readings where it was the busiest process under Claude Code.
    pub seconds: usize,
    /// About how long it was the busiest, in milliseconds.
    pub ms_on_top: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FollowerStat {
    pub task: TaskId,
    pub agent_type: Option<String>,
    pub title: String,
    pub status: Status,
    pub took_ms: i64,
    pub tools: usize,
    pub cost: f64,
}

/// Tokens and cost for one model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCost {
    pub model: String,
    pub calls: usize,
    pub tokens: u64,
    pub output: u64,
    /// `None` when hooktop has no price for the model.
    pub cost: Option<f64>,
}

/// What the session's model calls cost, at API list prices.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Costs {
    /// Model calls with usage recorded.
    pub calls: usize,
    /// Sum over models with a known price.
    pub total: f64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub by_model: Vec<ModelCost>,
    /// Priced cost and tokens per task.
    pub by_task: HashMap<TaskId, f64>,
    pub tokens_by_task: HashMap<TaskId, u64>,
    /// Cost of calls that couldn't be matched to a task.
    pub unattributed: f64,
}

impl Costs {
    pub fn has_usage(&self) -> bool {
        self.calls > 0
    }

    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    /// Models seen without a price.
    pub fn unpriced(&self) -> Vec<&str> {
        self.by_model
            .iter()
            .filter(|m| m.cost.is_none())
            .map(|m| m.model.as_str())
            .collect()
    }

    pub fn of(&self, task: TaskId) -> f64 {
        self.by_task.get(&task).copied().unwrap_or(0.0)
    }

    pub fn tokens_of(&self, task: TaskId) -> u64 {
        self.tokens_by_task.get(&task).copied().unwrap_or(0)
    }

    /// A prompt together with the turns that answered it.
    pub fn prompt(&self, g: &SessionGraph, prompt: TaskId) -> (f64, u64) {
        let turns = view::prompt_groups(g)
            .into_iter()
            .find(|(p, _)| *p == Some(prompt))
            .map(|(_, t)| t)
            .unwrap_or_default();
        std::iter::once(prompt)
            .chain(turns)
            .fold((0.0, 0), |(c, n), t| (c + self.of(t), n + self.tokens_of(t)))
    }

    /// Everything the main agent did: all but the followers.
    pub fn boss(&self, g: &SessionGraph) -> f64 {
        self.unattributed
            + g.tasks
                .iter()
                .filter(|t| t.header.kind != TaskKind::Subagent)
                .map(|t| self.of(t.header.id))
                .sum::<f64>()
    }
}

/// Price every model call in the session.
pub fn costs(g: &SessionGraph, pricing: &Pricing) -> Costs {
    let mut c = Costs::default();
    let mut models: HashMap<String, ModelCost> = HashMap::new();
    for u in &g.usage {
        c.calls += 1;
        c.input += u.input;
        c.output += u.output;
        c.cache_read += u.cache_read;
        c.cache_write += u.cache_write_5m + u.cache_write_1h;
        let cost = pricing.cost(u);
        let m = models.entry(u.model.clone()).or_insert_with(|| ModelCost {
            model: u.model.clone(),
            calls: 0,
            tokens: 0,
            output: 0,
            cost: Some(0.0),
        });
        m.calls += 1;
        m.tokens += u.total();
        m.output += u.output;
        m.cost = m.cost.zip(cost).map(|(a, b)| a + b);
        let cost = cost.unwrap_or(0.0);
        c.total += cost;
        match u.task {
            Some(t) => {
                *c.by_task.entry(t).or_default() += cost;
                *c.tokens_by_task.entry(t).or_default() += u.total();
            }
            None => c.unattributed += cost,
        }
    }
    c.by_model = models.into_values().collect();
    c.by_model.sort_by(|a, b| {
        b.cost
            .unwrap_or(-1.0)
            .total_cmp(&a.cost.unwrap_or(-1.0))
            .then(b.tokens.cmp(&a.tokens))
    });
    c
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub prompts: usize,
    pub turns: usize,
    pub calls: usize,
    pub failed: usize,
    pub followers: usize,
    pub wall_ms: i64,
    /// Sum of every tool call's duration (parallel calls add up).
    pub tool_ms: i64,
    /// Where the wall time went.
    pub time: TimeSplit,
    /// The longest stretches of thinking (Claude working, no tool running)
    /// and of waiting for you, longest first.
    pub thinking: Vec<Stretch>,
    pub waiting: Vec<Stretch>,
    /// Every tool call, in the order they started (the rankings are drawn
    /// from these).
    pub all_calls: Vec<CallStat>,
    /// Prompts that matched an automated prefix: not counted in `prompts`
    /// or `longest_prompts`.
    pub automated_prompts: usize,
    pub peak_cpu: Option<f32>,
    pub peak_cpu_process: Option<String>,
    pub peak_rss: Option<u64>,
    pub avg_cpu: Option<f32>,
    pub mix: Vec<ToolMix>,
    pub slowest: Vec<CallStat>,
    pub longest_prompts: Vec<PromptStat>,
    pub ram_eaters: Vec<CallStat>,
    pub cpu_hogs: Vec<CallStat>,
    pub processes: Vec<ProcessStat>,
    pub follower_runs: Vec<FollowerStat>,
    pub costs: Costs,
}

/// Where a session's wall time went. The three add up to the wall time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimeSplit {
    /// Claude working with no tool running: the model thinking and writing.
    pub thinking_ms: i64,
    /// At least one tool call running (parallel calls count once).
    pub tools_ms: i64,
    /// Between prompts: waiting for you.
    pub waiting_ms: i64,
}

/// A stretch of time in a session, with what was around it.
#[derive(Debug, Clone, PartialEq)]
pub struct Stretch {
    pub start_ms: i64,
    pub end_ms: i64,
    /// For thinking, the turn or follower that was working; for waiting,
    /// the prompt you sent next.
    pub task: Option<TaskId>,
    /// The tool call (or, for waiting, the prompt) just before and after.
    pub before: Option<(TaskId, Option<usize>)>,
    pub after: Option<(TaskId, Option<usize>)>,
    /// Model calls that finished inside the stretch, and what they cost.
    pub model_calls: usize,
    pub tokens: u64,
    pub cost: f64,
}

impl Stretch {
    pub fn ms(&self) -> i64 {
        self.end_ms - self.start_ms
    }
}

/// Options for [`stats_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    /// Prompts starting with one of these are automated messages (from your
    /// own tooling, say), not prompts you typed.
    pub automated_prompts: &'a [String],
}

/// Is this prompt an automated one?
pub fn is_automated(title: &str, prefixes: &[String]) -> bool {
    let t = title.trim_start();
    prefixes.iter().any(|p| !p.is_empty() && t.starts_with(p.as_str()))
}

/// `spans` merged into sorted, non-overlapping spans.
fn merge(mut spans: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    spans.retain(|(a, b)| b > a);
    spans.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (a, b) in spans {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Total length of the union of `spans`.
#[cfg(test)]
fn union_ms(spans: Vec<(i64, i64)>) -> i64 {
    merge(spans).iter().map(|(a, b)| b - a).sum()
}

/// The parts of merged spans `a` that merged spans `b` don't cover.
fn subtract(a: &[(i64, i64)], b: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    for &(start, e) in a {
        let mut s = start;
        for &(bs, be) in b.iter().filter(|(bs, be)| *be > start && *bs < e) {
            if bs > s {
                out.push((s, bs));
            }
            s = s.max(be);
        }
        if e > s {
            out.push((s, e));
        }
    }
    out
}

/// Is this the call that starts a follower? Its time is the follower's
/// whole run, which the Followers ranking already shows.
fn spawns_follower(name: &str) -> bool {
    matches!(name, "Agent" | "Task")
}

impl Stats {
    pub fn failure_rate(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.failed as f64 / self.calls as f64
        }
    }
}

/// Work out the stats; `now` closes anything still running, `top` caps
/// each ranking.
pub fn stats(g: &SessionGraph, now: i64, top: usize, pricing: &Pricing) -> Stats {
    stats_with(g, now, top, pricing, Options::default())
}

/// [`stats`] with [`Options`].
pub fn stats_with(g: &SessionGraph, now: i64, top: usize, pricing: &Pricing, opts: Options) -> Stats {
    let costs = costs(g, pricing);
    // Something still running counts up to now only while Claude is working;
    // in a session that's waiting or closed it stopped at the last activity.
    let live_until = if g.status == crate::graph::SessionStatus::Running {
        now
    } else {
        g.updated_ms
    };
    let end_of = |status: Status, start: i64, end: Option<i64>| {
        end.unwrap_or(if status == Status::Running {
            live_until.max(start)
        } else {
            start
        })
    };
    let mut s = Stats::default();

    // Calls, with what the samples saw while each ran.
    let spans: Vec<(i64, i64)> = g
        .tasks
        .iter()
        .flat_map(|t| t.tools.iter())
        .map(|c| (c.started_ms, end_of(c.status, c.started_ms, c.ended_ms)))
        .collect();
    let mut calls = Vec::new();
    let mut n = 0;
    for t in &g.tasks {
        for (i, c) in t.tools.iter().enumerate() {
            let (start, end) = spans[n];
            let me = n;
            n += 1;
            let (peak_rss, avg_cpu, picked, borrowed) = during(g, start, end);
            let shared = picked.iter().any(|at| {
                spans
                    .iter()
                    .enumerate()
                    .any(|(j, (s, e))| j != me && s <= at && at <= e)
            });
            let reading = if borrowed {
                Reading::Borrowed
            } else if shared {
                Reading::Shared
            } else {
                Reading::Own
            };
            calls.push(CallStat {
                task: t.header.id,
                tool: i,
                name: view::tool_name(&c.name).to_string(),
                target: view::first_line(&c.target).to_string(),
                agent_type: t.header.agent_type.clone(),
                status: c.status,
                took_ms: (end - c.started_ms).max(0),
                peak_rss,
                avg_cpu,
                reading,
                repeats: 1,
            });
        }
    }
    s.calls = calls.len();
    s.failed = calls.iter().filter(|c| c.status == Status::Failed).count();
    s.tool_ms = calls.iter().map(|c| c.took_ms).sum();

    let mut mix: HashMap<&str, ToolMix> = HashMap::new();
    for c in &calls {
        let m = mix.entry(c.name.as_str()).or_insert_with(|| ToolMix {
            name: c.name.clone(),
            count: 0,
            failed: 0,
            total_ms: 0,
        });
        m.count += 1;
        m.failed += usize::from(c.status == Status::Failed);
        m.total_ms += c.took_ms;
    }
    s.mix = mix.into_values().collect();
    s.mix.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then(b.total_ms.cmp(&a.total_ms))
            .then(a.name.cmp(&b.name))
    });

    // Rankings: the same command run again is one row with a count, so a
    // test suite run five times doesn't fill the list.
    let by = |key: fn(&CallStat) -> Option<f64>, keep: fn(&CallStat) -> bool| {
        let mut v: Vec<CallStat> = calls.iter().filter(|c| key(c).is_some() && keep(c)).cloned().collect();
        v.sort_by(|a, b| key(b).unwrap().total_cmp(&key(a).unwrap()));
        let mut out: Vec<CallStat> = Vec::new();
        for c in v {
            match out.iter_mut().find(|o| o.name == c.name && o.target == c.target) {
                Some(o) => o.repeats += 1,
                None => out.push(c),
            }
        }
        out.truncate(top);
        out
    };
    s.slowest = by(|c| Some(c.took_ms as f64), |c| !spawns_follower(&c.name));
    s.ram_eaters = by(|c| c.peak_rss.map(|r| r as f64), |_| true);
    s.cpu_hogs = by(|c| c.avg_cpu.map(f64::from), |_| true);

    // Prompts: from the prompt to the end of its last turn.
    let mut active: Vec<(i64, i64)> = Vec::new();
    for (p, turns) in view::prompt_groups(g) {
        let span_of = |id: TaskId| {
            let h = &g.tasks[id as usize].header;
            (h.started_ms, end_of(h.status, h.started_ms, h.ended_ms))
        };
        active.extend(turns.iter().map(|t| span_of(*t)));
        let Some(pid) = p else { continue };
        let h = &g.tasks[pid as usize].header;
        let last = turns.last().and_then(|t| g.tasks[*t as usize].header.ended_ms);
        let end = h.ended_ms.or(last);
        active.push((h.started_ms, end_of(h.status, h.started_ms, end)));
        if !matches!(h.label.as_str(), "Prompt" | "Command") {
            continue;
        }
        if is_automated(&h.title, opts.automated_prompts) {
            s.automated_prompts += 1;
            continue;
        }
        s.prompts += 1;
        s.longest_prompts.push(PromptStat {
            task: pid,
            title: view::first_line(&h.title).to_string(),
            status: h.status,
            took_ms: (end_of(h.status, h.started_ms, end) - h.started_ms).max(0),
            turns: turns.len(),
            tools: turns.iter().map(|t| g.tasks[*t as usize].tools.len()).sum(),
            cost: costs.prompt(g, pid).0,
        });
    }
    s.longest_prompts.sort_by_key(|p| std::cmp::Reverse(p.took_ms));
    s.longest_prompts.truncate(top);

    s.turns = g.tasks.iter().filter(|t| t.header.kind == TaskKind::Turn).count();
    for t in g.tasks.iter().filter(|t| t.header.kind == TaskKind::Subagent) {
        let h = &t.header;
        s.follower_runs.push(FollowerStat {
            task: h.id,
            agent_type: h.agent_type.clone(),
            title: view::first_line(&h.title).to_string(),
            status: h.status,
            took_ms: (end_of(h.status, h.started_ms, h.ended_ms) - h.started_ms).max(0),
            tools: t.tools.len(),
            cost: costs.of(h.id),
        });
    }
    s.followers = s.follower_runs.len();
    s.follower_runs.sort_by_key(|r| std::cmp::Reverse(r.took_ms));
    s.follower_runs.truncate(top);

    let end = if g.status == crate::graph::SessionStatus::Ended {
        g.updated_ms
    } else {
        now.max(g.updated_ms)
    };
    s.wall_ms = (end - g.started_ms).max(0);

    // Where the time went: working (any prompt, turn or follower running),
    // and within that, tools running; the rest is waiting for you.
    for t in g.tasks.iter().filter(|t| t.header.kind == TaskKind::Subagent) {
        let h = &t.header;
        active.push((h.started_ms, end_of(h.status, h.started_ms, h.ended_ms)));
    }
    let clip = |(a, b): (i64, i64)| (a.max(g.started_ms), b.min(end));
    let working_spans = merge(active.into_iter().map(clip).collect());
    let working: i64 = working_spans.iter().map(|(a, b)| b - a).sum();
    // A follower's run isn't a tool running: its own calls count, and the
    // rest of it is thinking.
    let tool_spans = merge(
        spans
            .iter()
            .zip(&calls)
            .filter(|(_, c)| !spawns_follower(&c.name))
            .map(|(sp, _)| clip(*sp))
            .collect(),
    );
    let tools: i64 = tool_spans.iter().map(|(a, b)| b - a).sum::<i64>().min(working);
    s.time = TimeSplit {
        thinking_ms: working - tools,
        tools_ms: tools,
        waiting_ms: (s.wall_ms - working).max(0),
    };

    // The stretches behind those numbers.
    let positions: Vec<(TaskId, usize)> = g
        .tasks
        .iter()
        .flat_map(|t| (0..t.tools.len()).map(move |i| (t.header.id, i)))
        .collect();
    let model_calls_in = |a: i64, b: i64| {
        let in_it: Vec<_> = g.usage.iter().filter(|u| u.at_ms >= a && u.at_ms <= b).collect();
        (
            in_it.len(),
            in_it.iter().map(|u| u.total()).sum::<u64>(),
            in_it.iter().filter_map(|u| pricing.cost(u)).sum::<f64>(),
        )
    };
    let real = |i: &usize| !spawns_follower(&calls[*i].name);
    // Which agent a task belongs to: a follower's own id, or `None` for Watcher.
    let agent_of = |t: TaskId| (g.tasks[t as usize].header.kind == TaskKind::Subagent).then_some(t);
    for (a, b) in subtract(&working_spans, &tool_spans) {
        let mid = a + (b - a) / 2;
        let task = g
            .tasks
            .iter()
            .filter(|t| matches!(t.header.kind, TaskKind::Turn | TaskKind::Subagent))
            .filter(|t| {
                let h = &t.header;
                h.started_ms <= mid && end_of(h.status, h.started_ms, h.ended_ms) >= mid
            })
            .max_by_key(|t| t.header.started_ms)
            .map(|t| t.header.id);
        // The same agent's last call before it and first call after it.
        let agent = task.and_then(agent_of);
        let mine = |i: &usize| real(i) && agent_of(positions[*i].0) == agent;
        let before = (0..spans.len())
            .filter(mine)
            .filter(|i| spans[*i].1 <= a)
            .max_by_key(|i| spans[*i].1)
            .map(|i| (positions[i].0, Some(positions[i].1)));
        let after = (0..spans.len())
            .filter(mine)
            .filter(|i| spans[*i].0 >= b)
            .min_by_key(|i| spans[*i].0)
            .map(|i| (positions[i].0, Some(positions[i].1)));
        let (model_calls, tokens, cost) = model_calls_in(a, b);
        s.thinking.push(Stretch {
            start_ms: a,
            end_ms: b,
            task,
            before,
            after,
            model_calls,
            tokens,
            cost,
        });
    }
    let prompts: Vec<&crate::graph::Task> = g.tasks.iter().filter(|t| t.header.kind == TaskKind::Prompt).collect();
    for (a, b) in subtract(&[(g.started_ms, end)], &working_spans) {
        let before = prompts
            .iter()
            .rev()
            .find(|t| t.header.started_ms < a)
            .map(|t| (t.header.id, None));
        let next = prompts.iter().find(|t| t.header.started_ms >= b).map(|t| t.header.id);
        s.waiting.push(Stretch {
            start_ms: a,
            end_ms: b,
            task: next,
            before,
            after: next.map(|id| (id, None)),
            model_calls: 0,
            tokens: 0,
            cost: 0.0,
        });
    }
    for list in [&mut s.thinking, &mut s.waiting] {
        list.sort_by_key(|x| std::cmp::Reverse(x.ms()));
        list.truncate(top * 2);
    }
    s.all_calls = calls.clone();

    // Machine-wide numbers from the samples.
    if let Some(peak) = g.samples.iter().max_by(|a, b| a.cpu.total_cmp(&b.cpu)) {
        s.peak_cpu = Some(peak.cpu);
        s.peak_cpu_process = peak.top.clone();
        s.peak_rss = g.samples.iter().map(|x| x.rss).max();
        s.avg_cpu = Some(g.samples.iter().map(|x| x.cpu).sum::<f32>() / g.samples.len() as f32);
    }
    // Time on top: each reading counts until the next one (at most 2 s,
    // so gaps while Claude waited don't count).
    let mut procs: HashMap<&str, (ProcessStat, i64)> = HashMap::new();
    for (i, x) in g.samples.iter().enumerate() {
        if let Some(name) = &x.top {
            let span = g
                .samples
                .get(i + 1)
                .map_or(1_000, |n| (n.at_ms - x.at_ms).clamp(0, 2_000));
            let p = procs.entry(name).or_insert_with(|| {
                (
                    ProcessStat {
                        name: name.clone(),
                        peak_cpu: 0.0,
                        seconds: 0,
                        ms_on_top: 0,
                    },
                    0,
                )
            });
            p.0.peak_cpu = p.0.peak_cpu.max(x.cpu);
            p.0.seconds += 1;
            p.1 += span;
        }
    }
    let procs: HashMap<&str, ProcessStat> = procs
        .into_iter()
        .map(|(k, (mut p, ms))| {
            p.ms_on_top = ms;
            (k, p)
        })
        .collect();
    s.processes = procs.into_values().collect();
    s.processes
        .sort_by(|a, b| b.peak_cpu.total_cmp(&a.peak_cpu).then(a.name.cmp(&b.name)));
    s.processes.truncate(top);
    s.costs = costs;
    s
}

/// Peak memory and average CPU over `[start, end]`, the times of the
/// readings used, and whether they were borrowed: a call shorter than the
/// sampling interval borrows the first reading after it started.
fn during(g: &SessionGraph, start: i64, end: i64) -> (Option<u64>, Option<f32>, Vec<i64>, bool) {
    let inside: Vec<_> = g
        .samples
        .iter()
        .filter(|x| x.at_ms >= start && x.at_ms <= end)
        .collect();
    let borrowed = inside.is_empty();
    let picked: Vec<_> = if borrowed {
        g.samples
            .iter()
            .find(|x| x.at_ms >= start && x.at_ms - start <= 1_500)
            .into_iter()
            .collect()
    } else {
        inside
    };
    if picked.is_empty() {
        return (None, None, Vec::new(), false);
    }
    let peak = picked.iter().map(|x| x.rss).max();
    let avg = picked.iter().map(|x| x.cpu).sum::<f32>() / picked.len() as f32;
    (peak, Some(avg), picked.iter().map(|x| x.at_ms).collect(), borrowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Sample;
    use crate::graph::{Delta, TaskHeader, ToolCall};
    use serde_json::Value;

    fn graph() -> SessionGraph {
        let mut g = SessionGraph::new("s", 0);
        let task = |id, kind, label: &str, start, end| Delta::Task {
            task: TaskHeader {
                id,
                kind,
                label: label.into(),
                title: format!("task {id}"),
                status: Status::Done,
                started_ms: start,
                ended_ms: Some(end),
                agent_id: None,
                agent_type: (kind == TaskKind::Subagent).then(|| "Explore".into()),
                summary: None,
                reply: None,
                tool_count: 0,
                failed_count: 0,
            },
        };
        let tool = |task_id, id: &str, name: &str, start, end, status| Delta::Tool {
            task_id,
            tool: ToolCall {
                id: id.into(),
                name: name.into(),
                target: format!("{name} target"),
                status,
                started_ms: start,
                ended_ms: Some(end),
                input: Value::Null,
                output: None,
                error: None,
                spawned_task: None,
            },
        };
        for d in [
            task(0, TaskKind::Prompt, "Prompt", 0, 9_000),
            task(1, TaskKind::Turn, "Turn 1", 100, 5_000),
            task(2, TaskKind::Subagent, "Subagent · Explore", 200, 2_000),
            tool(1, "a", "Read", 100, 150, Status::Done),
            tool(1, "b", "Bash", 1_000, 4_000, Status::Failed),
            tool(2, "c", "Grep", 300, 900, Status::Done),
        ] {
            g.apply_delta(&d);
        }
        for (at, cpu, rss) in [
            (1_000, 10.0, 100),
            (2_000, 300.0, 900),
            (3_000, 250.0, 700),
            (6_000, 5.0, 100),
        ] {
            g.apply_delta(&Delta::Sample(Sample {
                at_ms: at,
                cpu,
                rss,
                procs: 3,
                top: Some(if cpu > 100.0 { "cargo" } else { "node" }.into()),
            }));
        }
        g.status = crate::graph::SessionStatus::Ended;
        g.updated_ms = 9_000;
        g
    }

    #[test]
    fn ranks_calls_prompts_and_resources() {
        let s = stats(&graph(), 10_000, 5, &Pricing::builtin());
        assert_eq!((s.prompts, s.turns, s.calls, s.failed, s.followers), (1, 1, 3, 1, 1));
        assert_eq!(s.wall_ms, 9_000);
        assert_eq!(s.slowest[0].name, "Bash");
        assert_eq!(s.slowest[0].took_ms, 3_000);
        assert_eq!(s.ram_eaters[0].name, "Bash");
        assert_eq!(s.ram_eaters[0].peak_rss, Some(900));
        assert_eq!(s.cpu_hogs[0].name, "Bash");
        assert_eq!(s.peak_cpu, Some(300.0));
        assert_eq!(s.peak_cpu_process.as_deref(), Some("cargo"));
        assert_eq!(s.processes[0].name, "cargo");
        assert_eq!(s.processes[0].seconds, 2);
        assert_eq!(s.processes[0].ms_on_top, 1_000 + 2_000); // the 3 s gap is capped at 2 s
        assert_eq!(s.mix.iter().map(|m| m.count).sum::<usize>(), 3);
        assert_eq!(s.follower_runs[0].tools, 1);
        assert_eq!(s.longest_prompts[0].tools, 2);
        // A 50 ms Read borrows the next reading within 1.5 s.
        let read = s.slowest.iter().find(|c| c.name == "Read").unwrap();
        assert_eq!(read.peak_rss, Some(100));
        assert_eq!(read.reading, Reading::Borrowed);
        assert_eq!(s.ram_eaters[0].reading, Reading::Own, "Bash ran alone");
        assert!((s.failure_rate() - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn time_split_adds_up_and_counts_parallel_tools_once() {
        let s = stats(&graph(), 10_000, 5, &Pricing::builtin());
        // Tools: Read 50 ms + Grep 600 ms + Bash 3 s, none overlapping.
        assert_eq!(s.time.tools_ms, 3_650);
        assert_eq!(s.time.waiting_ms, 0, "the prompt spans the whole session");
        assert_eq!(s.time.thinking_ms + s.time.tools_ms + s.time.waiting_ms, s.wall_ms);
        assert_eq!(union_ms(vec![(0, 10), (5, 20), (30, 40), (35, 36)]), 30);
        assert_eq!(subtract(&[(0, 100)], &[(10, 20), (50, 120)]), vec![(0, 10), (20, 50)]);
        // The longest thinking stretch: after the Bash call, until the end.
        let t = &s.thinking[0];
        assert_eq!((t.start_ms, t.end_ms), (4_000, 9_000));
        assert_eq!(t.before, Some((1, Some(1))));
        assert_eq!(t.after, None);
        assert_eq!(s.thinking.iter().map(Stretch::ms).sum::<i64>(), s.time.thinking_ms);
        assert!(s.waiting.is_empty());
        assert_eq!(s.all_calls.len(), 3);
    }

    #[test]
    fn repeats_group_agent_calls_stay_out_and_automated_prompts_are_counted_apart() {
        let mut g = graph();
        let call = |id: &str, name: &str, target: &str, start, end| Delta::Tool {
            task_id: 1,
            tool: ToolCall {
                id: id.into(),
                name: name.into(),
                target: target.into(),
                status: Status::Done,
                started_ms: start,
                ended_ms: Some(end),
                input: Value::Null,
                output: None,
                error: None,
                spawned_task: None,
            },
        };
        g.apply_delta(&call("again", "Bash", "Bash target", 4_100, 4_900));
        g.apply_delta(&call("spawn", "Agent", "Find it", 100, 8_000));
        let auto = ["task 0".to_string()];
        let s = stats_with(
            &g,
            10_000,
            5,
            &Pricing::builtin(),
            Options {
                automated_prompts: &auto,
            },
        );
        let bash: Vec<_> = s.slowest.iter().filter(|c| c.name == "Bash").collect();
        assert_eq!(bash.len(), 1, "one row for the same command");
        assert_eq!((bash[0].repeats, bash[0].took_ms), (2, 3_000));
        assert!(s.slowest.iter().all(|c| c.name != "Agent"));
        assert_eq!((s.prompts, s.automated_prompts), (0, 1));
        assert!(s.longest_prompts.is_empty());
        assert!(is_automated(
            "  [Subagent hand-back] ok",
            &["[Subagent hand-back]".into()]
        ));
        assert!(!is_automated("anything", &["".into()]));
    }

    #[test]
    fn parallel_calls_share_readings() {
        let mut g = graph();
        // A second Bash that overlaps the first one's readings.
        g.apply_delta(&Delta::Tool {
            task_id: 1,
            tool: ToolCall {
                id: "d".into(),
                name: "Bash".into(),
                target: "npm test".into(),
                status: Status::Done,
                started_ms: 1_500,
                ended_ms: Some(2_500),
                input: Value::Null,
                output: None,
                error: None,
                spawned_task: None,
            },
        });
        let s = stats(&g, 10_000, 5, &Pricing::builtin());
        let of = |target: &str| s.ram_eaters.iter().find(|c| c.target == target).unwrap().reading;
        assert_eq!(of("npm test"), Reading::Shared);
        assert_eq!(of("Bash target"), Reading::Shared);
        assert_eq!(Reading::Shared.label(), Some("shared"));
        assert_eq!(Reading::Own.label(), None);
        assert_eq!(Reading::Borrowed.mark(), Some("↳"));
    }

    #[test]
    fn costs_by_task_model_and_prompt() {
        use crate::usage::Usage;
        let mut g = graph();
        let u = |id: &str, model: &str, task: Option<TaskId>, input: u64, output: u64| {
            Delta::Usage(Usage {
                message_id: id.into(),
                model: model.into(),
                input,
                output,
                task,
                ..Default::default()
            })
        };
        for d in [
            u("a", "claude-opus-5-5", Some(1), 1_000_000, 0),  // turn: $4
            u("b", "claude-haiku-5-5", Some(2), 0, 1_000_000), // follower: $0.50
            u("c", "claude-opus-5-5", Some(0), 0, 100_000),    // the reply: $2
            u("d", "claude-mystery-1", Some(1), 10, 10),       // no price
            u("e", "claude-opus-5-5", None, 250_000, 0),       // unmatched: $1
        ] {
            g.apply_delta(&d);
        }
        let s = stats(&g, 10_000, 5, &Pricing::builtin());
        let c = &s.costs;
        assert_eq!(c.calls, 5);
        assert!((c.total - 7.5).abs() < 1e-9);
        assert!((c.of(1) - 4.0).abs() < 1e-9);
        assert!((c.unattributed - 1.0).abs() < 1e-9);
        assert!((c.prompt(&g, 0).0 - 6.0).abs() < 1e-9, "prompt + its turn");
        assert!((c.boss(&g) - 7.0).abs() < 1e-9, "everything but the follower");
        assert_eq!(c.unpriced(), vec!["claude-mystery-1"]);
        assert_eq!(c.by_model[0].model, "claude-opus-5-5");
        assert!((s.follower_runs[0].cost - 0.5).abs() < 1e-9);
        assert!((s.longest_prompts[0].cost - 6.0).abs() < 1e-9);
    }
}
