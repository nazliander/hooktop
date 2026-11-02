//! Hook payloads as Claude Code sends them, and the normalized [`Event`]
//! the rest of hooktop works with.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The JSON body Claude Code POSTs to an `http` hook.
///
/// Only the fields hooktop uses are typed; everything else is kept in
/// `extra` so stored payloads can be re-normalized later without loss.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookPayload {
    pub session_id: String,
    pub hook_event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    /// Set when the hook fired inside a subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_response: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One normalized event in a session's stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Global, monotonically increasing sequence number (assigned on ingest).
    pub seq: u64,
    /// Wall-clock receive time, milliseconds since the Unix epoch.
    pub at_ms: i64,
    pub session_id: String,
    /// The subagent this event came from; `None` for the main thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted {
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    PromptSubmitted {
        prompt: String,
    },
    ToolStarted {
        tool_use_id: Option<String>,
        name: String,
        input: Value,
    },
    ToolFinished {
        tool_use_id: Option<String>,
        name: String,
        response: Value,
    },
    ToolFailed {
        tool_use_id: Option<String>,
        name: String,
        error: String,
    },
    /// All tool calls from one assistant message have completed.
    ToolBatchDone,
    SubagentStarted {
        agent_id: String,
        agent_type: String,
    },
    SubagentStopped {
        agent_id: String,
        agent_type: String,
        last_message: Option<String>,
    },
    Stopped {
        last_message: Option<String>,
    },
    SessionEnded {
        reason: Option<String>,
    },
    Notification {
        message: String,
    },
    /// CPU and memory of the session's Claude Code process tree, recorded
    /// by hooktop itself (see [`SAMPLE_HOOK`]).
    Sample(Sample),
    /// A hook event hooktop stores but does not visualize.
    Other {
        hook_event_name: String,
    },
}

/// The `hook_event_name` hooktop uses for its own resource samples, stored
/// alongside real hook events so they persist, export and replay the same way.
pub const SAMPLE_HOOK: &str = "HooktopSample";

/// One reading of a session's Claude Code process and everything it started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub at_ms: i64,
    /// Percent of one core, so 400 means four cores busy.
    pub cpu: f32,
    /// Resident memory in bytes.
    pub rss: u64,
    /// Processes in the tree.
    pub procs: u32,
    /// The busiest process below Claude Code, e.g. `cargo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<String>,
}

impl Sample {
    /// The payload hooktop stores for a sample.
    pub fn to_payload(&self, session_id: &str) -> HookPayload {
        let mut extra = Map::new();
        extra.insert("cpu".into(), Value::from(self.cpu));
        extra.insert("rss".into(), Value::from(self.rss));
        extra.insert("procs".into(), Value::from(self.procs));
        if let Some(top) = &self.top {
            extra.insert("top".into(), Value::from(top.clone()));
        }
        HookPayload {
            session_id: session_id.into(),
            hook_event_name: SAMPLE_HOOK.into(),
            extra,
            ..Default::default()
        }
    }
}

/// Turn a raw hook payload into an [`Event`]. `seq` and `at_ms` come from
/// the ingest side so replays are deterministic.
pub fn normalize(p: &HookPayload, seq: u64, at_ms: i64) -> Event {
    let name = || p.tool_name.clone().unwrap_or_else(|| "unknown".into());
    let agent_type = || p.agent_type.clone().unwrap_or_else(|| "agent".into());

    let kind = match p.hook_event_name.as_str() {
        "SessionStart" => EventKind::SessionStarted {
            source: p.source.clone(),
            model: p.model.as_ref().map(model_name),
        },
        "UserPromptSubmit" => EventKind::PromptSubmitted {
            prompt: p.prompt.clone().unwrap_or_default(),
        },
        "PreToolUse" => EventKind::ToolStarted {
            tool_use_id: p.tool_use_id.clone(),
            name: name(),
            input: p.tool_input.clone().unwrap_or(Value::Null),
        },
        "PostToolUse" => EventKind::ToolFinished {
            tool_use_id: p.tool_use_id.clone(),
            name: name(),
            response: p.tool_response.clone().unwrap_or(Value::Null),
        },
        "PostToolUseFailure" => EventKind::ToolFailed {
            tool_use_id: p.tool_use_id.clone(),
            name: name(),
            error: p.error.as_ref().map(value_text).unwrap_or_else(|| "failed".into()),
        },
        "PostToolBatch" => EventKind::ToolBatchDone,
        "SubagentStart" => EventKind::SubagentStarted {
            agent_id: p.agent_id.clone().unwrap_or_default(),
            agent_type: agent_type(),
        },
        "SubagentStop" => EventKind::SubagentStopped {
            agent_id: p.agent_id.clone().unwrap_or_default(),
            agent_type: agent_type(),
            last_message: p.last_assistant_message.clone(),
        },
        "Stop" => EventKind::Stopped {
            last_message: p.last_assistant_message.clone(),
        },
        "SessionEnd" => EventKind::SessionEnded {
            reason: p.reason.clone(),
        },
        "Notification" => EventKind::Notification {
            message: p.message.clone().unwrap_or_default(),
        },
        SAMPLE_HOOK => EventKind::Sample(Sample {
            at_ms,
            cpu: p.extra.get("cpu").and_then(Value::as_f64).unwrap_or(0.0) as f32,
            rss: p.extra.get("rss").and_then(Value::as_u64).unwrap_or(0),
            procs: p.extra.get("procs").and_then(Value::as_u64).unwrap_or(0) as u32,
            top: p.extra.get("top").and_then(Value::as_str).map(str::to_string),
        }),
        other => EventKind::Other {
            hook_event_name: other.to_string(),
        },
    };

    // SubagentStart/Stop fire in the parent's context; the agent_id there
    // names the child, so it is carried in the kind, not as the source agent.
    let from_subagent = !matches!(
        kind,
        EventKind::SubagentStarted { .. } | EventKind::SubagentStopped { .. }
    );

    Event {
        seq,
        at_ms,
        session_id: p.session_id.clone(),
        agent_id: p.agent_id.clone().filter(|id| from_subagent && !id.is_empty()),
        agent_type: p.agent_type.clone().filter(|_| from_subagent),
        cwd: p.cwd.clone(),
        kind,
    }
}

fn model_name(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("id")
            .or_else(|| o.get("display_name"))
            .map(value_text)
            .unwrap_or_else(|| v.to_string()),
        other => other.to_string(),
    }
}

pub(crate) fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pre_tool_use_and_keeps_unknown_fields() {
        let raw = r#"{
            "session_id": "s1", "hook_event_name": "PreToolUse",
            "cwd": "/repo", "tool_name": "Bash", "tool_use_id": "toolu_1",
            "tool_input": {"command": "cargo test"}, "permission_mode": "default"
        }"#;
        let p: HookPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(p.extra["permission_mode"], "default");
        let ev = normalize(&p, 7, 1000);
        assert_eq!(ev.seq, 7);
        assert!(ev.agent_id.is_none());
        assert!(matches!(ev.kind, EventKind::ToolStarted { ref name, .. } if name == "Bash"));
    }

    #[test]
    fn subagent_start_is_attributed_to_the_parent() {
        let p = HookPayload {
            session_id: "s1".into(),
            hook_event_name: "SubagentStart".into(),
            agent_id: Some("a1".into()),
            agent_type: Some("Explore".into()),
            ..Default::default()
        };
        let ev = normalize(&p, 1, 0);
        assert!(ev.agent_id.is_none());
        assert_eq!(
            ev.kind,
            EventKind::SubagentStarted {
                agent_id: "a1".into(),
                agent_type: "Explore".into()
            }
        );
    }
}
