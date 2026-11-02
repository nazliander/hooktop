//! Synthetic and recorded sessions, fed straight into the hub so the UI can
//! be developed without a live Claude session.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hooktop_core::HookPayload;
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::now_ms;

/// One scripted step: wait `delay_ms`, then send `payload`.
struct Step {
    delay_ms: u64,
    payload: HookPayload,
}

struct Script {
    session_id: String,
    steps: Vec<Step>,
}

impl Script {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            steps: Vec::new(),
        }
    }

    fn hook(&mut self, delay_ms: u64, name: &str, f: impl FnOnce(&mut HookPayload)) -> &mut Self {
        let mut p = HookPayload {
            session_id: self.session_id.clone(),
            hook_event_name: name.into(),
            cwd: Some("/home/dev/code/hooktop-demo".into()),
            ..Default::default()
        };
        f(&mut p);
        self.steps.push(Step { delay_ms, payload: p });
        self
    }

    fn pre(&mut self, delay: u64, agent: Option<(&str, &str)>, id: &str, tool: &str, input: Value) -> &mut Self {
        self.hook(delay, "PreToolUse", |p| {
            set_agent(p, agent);
            p.tool_use_id = Some(id.into());
            p.tool_name = Some(tool.into());
            p.tool_input = Some(input);
        })
    }

    fn post(&mut self, delay: u64, agent: Option<(&str, &str)>, id: &str, tool: &str, response: Value) -> &mut Self {
        self.hook(delay, "PostToolUse", |p| {
            set_agent(p, agent);
            p.tool_use_id = Some(id.into());
            p.tool_name = Some(tool.into());
            p.tool_response = Some(response);
        })
    }

    fn batch(&mut self, delay: u64) -> &mut Self {
        self.hook(delay, "PostToolBatch", |_| {})
    }
}

fn set_agent(p: &mut HookPayload, agent: Option<(&str, &str)>) {
    if let Some((id, ty)) = agent {
        p.agent_id = Some(id.into());
        p.agent_type = Some(ty.into());
    }
}

/// The "fix the flaky auth test" session from the design mockup.
fn flaky_test_script(session_id: String) -> Script {
    let sub = Some(("agent-explore-1", "Explore"));
    let mut s = Script::new(session_id);
    s.hook(0, "SessionStart", |p| {
        p.source = Some("startup".into());
        p.model = Some(json!("claude-opus-5-5"));
    })
    .hook(400, "UserPromptSubmit", |p| {
        p.prompt = Some("Fix the flaky auth test in tests/auth.rs — it fails about 1 in 5 runs.".into());
    })
    // Turn 1: read around and send an Explore subagent in parallel.
    .pre(900, None, "toolu_01", "Read", json!({"file_path": "/home/dev/code/hooktop-demo/src/auth.rs"}))
    .pre(10, None, "toolu_02", "Read", json!({"file_path": "/home/dev/code/hooktop-demo/src/session.rs"}))
    .pre(10, None, "toolu_03", "Read", json!({"file_path": "/home/dev/code/hooktop-demo/tests/mod.rs"}))
    .pre(10, None, "toolu_04", "Grep", json!({"pattern": "expires_at", "path": "src/"}))
    .pre(10, None, "toolu_05", "Agent", json!({"subagent_type": "Explore", "description": "Find expiry checks", "prompt": "Find every place token expiry is compared to now()."}))
    .post(40, None, "toolu_01", "Read", json!({"content": "// 214 lines"}))
    .post(10, None, "toolu_02", "Read", json!({"content": "// 131 lines"}))
    .post(10, None, "toolu_03", "Read", json!({"content": "// 67 lines"}))
    .post(10, None, "toolu_04", "Grep", json!("src/auth.rs:48\nsrc/auth.rs:91\nsrc/session.rs:22"))
    .hook(200, "SubagentStart", |p| set_agent(p, sub))
    .pre(600, sub, "toolu_11", "Glob", json!({"pattern": "**/clock*.rs"}))
    .post(30, sub, "toolu_11", "Glob", json!("src/clock.rs\ntests/fake_clock.rs"))
    .pre(500, sub, "toolu_12", "Read", json!({"file_path": "/home/dev/code/hooktop-demo/src/clock.rs"}))
    .post(30, sub, "toolu_12", "Read", json!({"content": "trait Clock { fn now(&self) -> Instant; }"}))
    .hook(1500, "SubagentStop", |p| {
        set_agent(p, sub);
        p.last_assistant_message =
            Some("Tests mock Instant through a Clock trait; auth.rs bypasses it with SystemTime::now().".into());
    })
    .post(20, None, "toolu_05", "Agent", json!("Tests mock Instant through a Clock trait; auth.rs bypasses it."))
    .batch(10)
    // Turn 2: patch.
    .pre(1200, None, "toolu_21", "Edit", json!({
        "file_path": "/home/dev/code/hooktop-demo/src/auth.rs",
        "old_string": "if SystemTime::now() > tok.expires_at {",
        "new_string": "if self.clock.now() > tok.expires_at {"
    }))
    .post(30, None, "toolu_21", "Edit", json!("1 hunk applied"))
    .batch(10)
    // Turn 3: verify (the first run fails, the second passes).
    .pre(900, None, "toolu_31", "Bash", json!({"command": "cargo test auth:: -- --nocapture"}))
    .hook(2600, "PostToolUseFailure", |p| {
        p.tool_use_id = Some("toolu_31".into());
        p.tool_name = Some("Bash".into());
        p.error = Some(json!("test auth::refresh_race ... FAILED\nerror: test failed, to rerun pass `--test auth`"));
    })
    .batch(10)
    .pre(1000, None, "toolu_41", "Edit", json!({"file_path": "/home/dev/code/hooktop-demo/tests/auth.rs"}))
    .post(30, None, "toolu_41", "Edit", json!("1 hunk applied"))
    .batch(10)
    .pre(700, None, "toolu_51", "Bash", json!({"command": "cargo test auth::"}))
    .post(2400, None, "toolu_51", "Bash", json!({"stdout": "test result: ok. 6 passed; 0 failed"}))
    .batch(10)
    // Turn 6: wrap up.
    .pre(800, None, "toolu_61", "Write", json!({"file_path": "/home/dev/code/hooktop-demo/CHANGELOG.md", "content": "- Fix flaky auth expiry test"}))
    .post(30, None, "toolu_61", "Write", json!("ok"))
    .batch(10)
    .hook(900, "Stop", |p| {
        p.last_assistant_message = Some("Fixed: auth.rs now uses the injected clock, so the test no longer races.".into());
    })
    // A follow-up prompt: a second column in the prompt view.
    .hook(2500, "UserPromptSubmit", |p| {
        p.prompt = Some("Add a regression test that runs the refresh 100 times.".into());
    })
    .pre(900, None, "toolu_71", "Read", json!({"file_path": "/home/dev/code/hooktop-demo/tests/auth.rs"}))
    .post(30, None, "toolu_71", "Read", json!({"content": "// 88 lines"}))
    .batch(10)
    .pre(1100, None, "toolu_72", "Edit", json!({"file_path": "/home/dev/code/hooktop-demo/tests/auth.rs"}))
    .post(30, None, "toolu_72", "Edit", json!("1 hunk applied"))
    .batch(10)
    .pre(700, None, "toolu_73", "Bash", json!({"command": "RUST_BACKTRACE=1 cargo test auth::refresh_repeat"}))
    .post(2200, None, "toolu_73", "Bash", json!({"stdout": "test result: ok. 1 passed; 0 failed"}))
    .batch(10)
    .hook(700, "Stop", |p| {
        p.last_assistant_message = Some("Added `refresh_repeat`, which loops the refresh path 100 times. It passes.".into());
    });
    s
}

/// Play the demo session into the hub. With `looped`, start a fresh demo
/// session every time the previous one finishes. Demo events are not stored.
pub async fn run_demo(hub: Arc<Hub>, speed: f64, looped: bool) {
    let mut n = 0;
    loop {
        n += 1;
        let script = flaky_test_script(format!("demo-{n}"));
        tracing::info!(session = %script.session_id, "playing demo session");
        let mut tick = 0;
        for step in script.steps {
            // Fake CPU and RAM readings every 200 ms of script time.
            let mut left = step.delay_ms;
            while left > 0 {
                let d = left.min(SAMPLE_MS);
                sleep_scaled(d, speed).await;
                left -= d;
                if left > 0 {
                    tick += 1;
                    emit_sample(&hub, &script.session_id, now_ms(), tick);
                }
            }
            let (hook, agent) = (step.payload.hook_event_name.clone(), step.payload.agent_id.clone());
            let at = now_ms();
            hub.ingest(step.payload, at, false);
            emit_usage(&hub, &script.session_id, &hook, agent.as_deref(), at);
        }
        if !looped {
            return;
        }
        sleep_scaled(5000, speed).await;
    }
}

/// Replay a JSONL file of recorded events (lines from `hooktop export`, or bare
/// hook payloads), keeping the original pacing up to 2 s per gap.
pub async fn replay_file(hub: Arc<Hub>, path: &Path, speed: f64) -> Result<()> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let mut last_at: Option<i64> = None;
    for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line).with_context(|| format!("line {}", i + 1))?;
        let (at, payload) = match v.get("payload") {
            Some(p) => (v.get("at_ms").and_then(Value::as_i64), p.clone()),
            None => (None, v),
        };
        let payload: HookPayload = serde_json::from_value(payload).with_context(|| format!("line {}", i + 1))?;
        let gap = match (last_at, at) {
            (Some(prev), Some(now)) => (now - prev).clamp(0, 2000) as u64,
            _ => 300,
        };
        last_at = at.or(last_at);
        sleep_scaled(gap, speed).await;
        hub.ingest(payload, now_ms(), false);
    }
    tracing::info!("replay of {} finished", path.display());
    Ok(())
}

const SAMPLE_MS: u64 = 200;

/// A made-up but plausible reading: Claude Code idles at a few percent,
/// and a running Bash call (the test suite) pins several cores.
fn synthetic_sample(g: &hooktop_core::SessionGraph, at_ms: i64, tick: u32) -> hooktop_core::Sample {
    let running = |name: &str| {
        g.tasks
            .iter()
            .flat_map(|t| &t.tools)
            .any(|c| c.name == name && c.status == hooktop_core::Status::Running)
    };
    let wobble = ((tick * 37) % 11) as f32;
    let (cpu, extra_mb, procs, top) = if running("Bash") {
        (
            280.0 + wobble * 12.0,
            620 + (tick % 5) as u64 * 40,
            9,
            Some("cargo".to_string()),
        )
    } else if running("Agent") || running("Task") {
        (18.0 + wobble, 40, 2, None)
    } else if g.status == hooktop_core::SessionStatus::Running {
        (6.0 + wobble / 2.0, 0, 2, None)
    } else {
        (0.5, 0, 1, None)
    };
    hooktop_core::Sample {
        at_ms,
        cpu,
        rss: (310 + extra_mb) * 1024 * 1024 + (tick as u64 % 7) * 3 * 1024 * 1024,
        procs,
        top,
    }
}

/// Made-up but plausible token usage: Opus for the main agent, Haiku for
/// the Explore follower, with the cache growing turn by turn the way real
/// sessions do. Real sessions get theirs from Claude Code's transcript.
fn emit_usage(hub: &Hub, session_id: &str, hook: &str, agent: Option<&str>, at_ms: i64) {
    let Some((_, g)) = hub.snapshot(session_id) else { return };
    let usage = |id: String, model: &str, task: &hooktop_core::Task, input: u64, read: u64, out: u64| {
        hooktop_core::usage::Usage {
            message_id: id,
            model: model.into(),
            at_ms,
            input,
            output: out,
            cache_read: read,
            cache_write_5m: 1_200 + 400 * task.tools.len() as u64,
            tool_use_ids: task.tools.iter().map(|c| c.id.clone()).collect(),
            ..Default::default()
        }
    };
    let turns = g
        .tasks
        .iter()
        .filter(|t| t.header.kind == hooktop_core::TaskKind::Turn)
        .count() as u64;
    let u = match hook {
        "PostToolBatch" => g
            .tasks
            .iter()
            .rev()
            .find(|t| t.header.kind == hooktop_core::TaskKind::Turn)
            .map(|t| {
                usage(
                    format!("demo-turn-{}", t.header.id),
                    "claude-opus-5-5",
                    t,
                    8,
                    24_000 + 7_000 * turns,
                    160 + 240 * t.tools.len() as u64,
                )
            }),
        "SubagentStop" => g
            .tasks
            .iter()
            .find(|t| t.header.kind == hooktop_core::TaskKind::Subagent && t.header.agent_id.as_deref() == agent)
            .map(|t| {
                usage(
                    format!("demo-agent-{}", t.header.id),
                    "claude-haiku-5-5",
                    t,
                    3_100,
                    18_000,
                    900,
                )
            }),
        "Stop" => {
            let prompt = g
                .tasks
                .iter()
                .rev()
                .find(|t| t.header.kind == hooktop_core::TaskKind::Prompt);
            prompt.map(|p| {
                let mut u = usage(
                    format!("demo-reply-{}", p.header.id),
                    "claude-opus-5-5",
                    p,
                    6,
                    26_000 + 7_000 * turns,
                    420,
                );
                u.tool_use_ids.clear();
                u
            })
        }
        _ => None,
    };
    if let Some(u) = u {
        hub.ingest(u.to_payload(session_id), at_ms, false);
    }
}

fn emit_sample(hub: &Hub, session_id: &str, at_ms: i64, tick: u32) {
    if let Some((_, g)) = hub.snapshot(session_id) {
        hub.ingest(synthetic_sample(&g, at_ms, tick).to_payload(session_id), at_ms, false);
    }
}

async fn sleep_scaled(ms: u64, speed: f64) {
    if ms == 0 {
        return;
    }
    let scaled = (ms as f64 / speed.max(0.01)) as u64;
    tokio::time::sleep(Duration::from_millis(scaled)).await;
}

/// The whole demo session applied at once, with fixed timestamps (for tests
/// and screenshots).
pub fn demo_graph(session_id: &str) -> hooktop_core::SessionGraph {
    let hub = Hub::new(None);
    let script = flaky_test_script(session_id.to_string());
    let mut at = 1_700_000_000_000_i64;
    let mut tick = 0;
    for step in script.steps {
        let mut left = step.delay_ms;
        while left > SAMPLE_MS {
            at += SAMPLE_MS as i64;
            left -= SAMPLE_MS;
            tick += 1;
            emit_sample(&hub, session_id, at, tick);
        }
        at += left as i64;
        let (hook, agent) = (step.payload.hook_event_name.clone(), step.payload.agent_id.clone());
        hub.ingest(step.payload, at, false);
        emit_usage(&hub, session_id, &hook, agent.as_deref(), at);
    }
    hub.snapshot(session_id).map(|(_, g)| g).expect("demo session exists")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hooktop_core::TaskKind;

    #[tokio::test]
    async fn demo_builds_the_mockup_graph() {
        let hub = Arc::new(Hub::new(None));
        run_demo(hub.clone(), 1000.0, false).await;
        let (_, g) = hub.snapshot("demo-1").unwrap();
        let kinds: Vec<_> = g.tasks.iter().map(|t| t.header.kind).collect();
        use TaskKind::*;
        assert_eq!(
            kinds,
            vec![
                Prompt, Turn, Subagent, Turn, Turn, Turn, Turn, Turn, Prompt, Turn, Turn, Turn
            ]
        );
        assert_eq!(g.tasks[11].tools[0].target, "cargo test auth::refresh_repeat");
        assert_eq!(g.tasks[1].header.title, "Read ×3 · Grep · Agent");
        assert_eq!(g.tasks[2].header.title, "Find expiry checks");
        assert_eq!(g.tasks[4].header.failed_count, 1);
    }
}
