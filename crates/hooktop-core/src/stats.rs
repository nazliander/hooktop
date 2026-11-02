//! Numbers about one session: what ran, what took longest, and what cost
//! the most CPU and memory.

use std::collections::HashMap;

use crate::graph::{SessionGraph, Status, TaskId, TaskKind};
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromptStat {
    pub task: TaskId,
    pub title: String,
    pub status: Status,
    pub took_ms: i64,
    pub turns: usize,
    pub tools: usize,
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
pub fn stats(g: &SessionGraph, now: i64, top: usize) -> Stats {
    let end_of = |status: Status, start: i64, end: Option<i64>| {
        end.unwrap_or(if status == Status::Running { now } else { start })
    };
    let mut s = Stats::default();

    // Calls, with what the samples saw while each ran.
    let mut calls = Vec::new();
    for t in &g.tasks {
        for (i, c) in t.tools.iter().enumerate() {
            let end = end_of(c.status, c.started_ms, c.ended_ms);
            let (peak_rss, avg_cpu) = during(g, c.started_ms, end);
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

    let by = |key: fn(&CallStat) -> Option<f64>| {
        let mut v: Vec<CallStat> = calls.iter().filter(|c| key(c).is_some()).cloned().collect();
        v.sort_by(|a, b| key(b).unwrap().total_cmp(&key(a).unwrap()));
        v.truncate(top);
        v
    };
    s.slowest = by(|c| Some(c.took_ms as f64));
    s.ram_eaters = by(|c| c.peak_rss.map(|r| r as f64));
    s.cpu_hogs = by(|c| c.avg_cpu.map(f64::from));

    // Prompts: from the prompt to the end of its last turn.
    for (p, turns) in view::prompt_groups(g) {
        let Some(pid) = p else { continue };
        let h = &g.tasks[pid as usize].header;
        if !matches!(h.label.as_str(), "Prompt" | "Command") {
            continue;
        }
        s.prompts += 1;
        let last = turns.last().and_then(|t| g.tasks[*t as usize].header.ended_ms);
        let end = h.ended_ms.or(last);
        s.longest_prompts.push(PromptStat {
            task: pid,
            title: view::first_line(&h.title).to_string(),
            status: h.status,
            took_ms: (end_of(h.status, h.started_ms, end) - h.started_ms).max(0),
            turns: turns.len(),
            tools: turns.iter().map(|t| g.tasks[*t as usize].tools.len()).sum(),
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
    s
}

/// Peak memory and average CPU over `[start, end]`. A call shorter than
/// the sampling interval borrows the first reading after it started.
fn during(g: &SessionGraph, start: i64, end: i64) -> (Option<u64>, Option<f32>) {
    let inside: Vec<_> = g
        .samples
        .iter()
        .filter(|x| x.at_ms >= start && x.at_ms <= end)
        .collect();
    let picked = if inside.is_empty() {
        g.samples
            .iter()
            .find(|x| x.at_ms >= start && x.at_ms - start <= 1_500)
            .into_iter()
            .collect()
    } else {
        inside
    };
    if picked.is_empty() {
        return (None, None);
    }
    let peak = picked.iter().map(|x| x.rss).max();
    let avg = picked.iter().map(|x| x.cpu).sum::<f32>() / picked.len() as f32;
    (peak, Some(avg))
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
        let s = stats(&graph(), 10_000, 5);
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
        assert!((s.failure_rate() - 1.0 / 3.0).abs() < 1e-9);
    }
}
