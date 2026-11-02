//! Token usage, read from Claude Code's session transcripts.
//!
//! Hooks don't carry token counts, but every hook names the session's
//! transcript (`transcript_path`), a JSONL file where each assistant
//! message records its model and `usage`. The server tails that file and
//! stores one [`Usage`] per model call as a `HooktopUsage` event; the graph
//! then attributes it to the turn, prompt or follower it belongs to.
//!
//! Only numbers and ids are kept: never the message text.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::event::HookPayload;
use crate::graph::TaskId;

/// The `hook_event_name` hooktop uses for usage it read from a transcript.
pub const USAGE_HOOK: &str = "HooktopUsage";

/// The tokens one model call used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// The API message id; a message streamed over several transcript
    /// lines is merged under it.
    pub message_id: String,
    pub model: String,
    /// When the message was written, from the transcript.
    pub at_ms: i64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    /// Tool calls this message made: how it's matched to a turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_use_ids: Vec<String>,
    /// `fast` when the call used fast mode (priced higher).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    /// `us` when the call ran with US-only data residency (1.1× prices).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geo: Option<String>,
    /// Set for messages from a subagent's transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The task it was attributed to (filled in by the graph).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
}

impl Usage {
    /// Everything sent to the model: what pricing tiers are measured on.
    pub fn prompt_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }

    pub fn total(&self) -> u64 {
        self.prompt_tokens() + self.output
    }

    /// Fold in another transcript line of the same message. Claude Code
    /// writes one line per content block, and the counts only grow.
    pub fn merge(&mut self, other: &Usage) {
        self.input = self.input.max(other.input);
        self.output = self.output.max(other.output);
        self.cache_read = self.cache_read.max(other.cache_read);
        self.cache_write_5m = self.cache_write_5m.max(other.cache_write_5m);
        self.cache_write_1h = self.cache_write_1h.max(other.cache_write_1h);
        for id in &other.tool_use_ids {
            if !self.tool_use_ids.contains(id) {
                self.tool_use_ids.push(id.clone());
            }
        }
        if self.agent_id.is_none() {
            self.agent_id.clone_from(&other.agent_id);
        }
        if self.model.is_empty() {
            self.model.clone_from(&other.model);
        }
        if self.speed.is_none() {
            self.speed.clone_from(&other.speed);
        }
        if self.geo.is_none() {
            self.geo.clone_from(&other.geo);
        }
    }

    /// The payload hooktop stores for this usage.
    pub fn to_payload(&self, session_id: &str) -> HookPayload {
        let mut extra = Map::new();
        extra.insert("usage".into(), serde_json::to_value(self).unwrap_or(Value::Null));
        HookPayload {
            session_id: session_id.into(),
            hook_event_name: USAGE_HOOK.into(),
            extra,
            ..Default::default()
        }
    }

    pub fn from_payload(p: &HookPayload) -> Option<Usage> {
        serde_json::from_value(p.extra.get("usage")?.clone()).ok()
    }
}

/// One transcript line → the usage it records, if it's an assistant
/// message with token counts.
pub fn parse_transcript_line(line: &str) -> Option<Usage> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let m = v.get("message")?;
    let model = m.get("model").and_then(Value::as_str).unwrap_or("");
    if model.is_empty() || model.starts_with('<') {
        // `<synthetic>` messages Claude Code writes itself cost nothing.
        return None;
    }
    let id = m
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| v.get("requestId").and_then(Value::as_str))?;
    let u = m.get("usage")?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let write_total = n("cache_creation_input_tokens");
    let (w5, w1) = match u.get("cache_creation") {
        Some(c) => {
            let g = |k: &str| c.get(k).and_then(Value::as_u64).unwrap_or(0);
            let (a, b) = (g("ephemeral_5m_input_tokens"), g("ephemeral_1h_input_tokens"));
            // Older transcripts only have the total: treat it as 5-minute.
            if a + b == 0 { (write_total, 0) } else { (a, b) }
        }
        None => (write_total, 0),
    };
    let usage = Usage {
        message_id: id.to_string(),
        model: model.to_string(),
        at_ms: v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms)
            .unwrap_or(0),
        input: n("input_tokens"),
        output: n("output_tokens"),
        cache_read: n("cache_read_input_tokens"),
        cache_write_5m: w5,
        cache_write_1h: w1,
        tool_use_ids: m
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter_map(|b| b.get("id").and_then(Value::as_str).map(str::to_string))
            .collect(),
        agent_id: v.get("agentId").and_then(Value::as_str).map(str::to_string),
        // Only kept when they change the price.
        speed: u
            .get("speed")
            .and_then(Value::as_str)
            .filter(|s| *s == "fast")
            .map(str::to_string),
        geo: u
            .get("inference_geo")
            .and_then(Value::as_str)
            .filter(|s| *s == "us")
            .map(str::to_string),
        task: None,
    };
    (usage.total() > 0).then_some(usage)
}

/// `2026-11-02T13:28:01.123Z` (or with a `+01:00` offset) → ms since the
/// Unix epoch.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    let mut ms = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        rest = &frac[digits.len()..];
        let padded = format!("{digits:0<3}");
        ms = padded[..3].parse::<i64>().ok()?;
    }
    let offset_min = match rest {
        "Z" | "z" | "" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o[1..3].parse::<i64>().ok()? * 60 + o[4..6].parse::<i64>().ok()?)
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3_600 + mi * 60 + sec) - offset_min * 60) * 1_000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("2026-11-02T13:28:01.123Z"), Some(1_793_626_081_123));
        assert_eq!(
            parse_rfc3339_ms("2026-11-02T14:28:01.123+01:00"),
            Some(1_793_626_081_123)
        );
        assert_eq!(parse_rfc3339_ms("2000-02-29T00:00:00.5Z"), Some(951_782_400_500));
        assert_eq!(parse_rfc3339_ms("yesterday"), None);
    }

    #[test]
    fn reads_assistant_usage_lines() {
        let line = r#"{"type":"assistant","timestamp":"2026-11-02T13:28:01.000Z","agentId":"a1",
            "message":{"id":"msg_1","model":"claude-opus-5-5","content":[
              {"type":"text","text":"secret file contents are never kept"},
              {"type":"tool_use","id":"toolu_9","name":"Read","input":{}}],
            "usage":{"input_tokens":12,"output_tokens":340,"cache_read_input_tokens":50000,
              "cache_creation_input_tokens":2000,
              "cache_creation":{"ephemeral_5m_input_tokens":1500,"ephemeral_1h_input_tokens":500}}}}"#
            .replace('\n', "");
        let u = parse_transcript_line(&line).unwrap();
        assert_eq!(u.message_id, "msg_1");
        assert_eq!(
            (u.input, u.output, u.cache_read, u.cache_write_5m, u.cache_write_1h),
            (12, 340, 50000, 1500, 500)
        );
        assert_eq!(u.tool_use_ids, vec!["toolu_9"]);
        assert_eq!(u.agent_id.as_deref(), Some("a1"));
        assert!(!serde_json::to_string(&u).unwrap().contains("secret"), "no text kept");

        // Total only: counts as 5-minute writes.
        let old = r#"{"type":"assistant","message":{"id":"m","model":"claude-sonnet-4-5","usage":{"input_tokens":1,"cache_creation_input_tokens":70}}}"#;
        assert_eq!(parse_transcript_line(old).unwrap().cache_write_5m, 70);

        for skip in [
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"id":"m","model":"<synthetic>","usage":{"input_tokens":0}}}"#,
            r#"{"type":"assistant","message":{"id":"m","model":"claude-opus-5-5","usage":{"input_tokens":0}}}"#,
            "not json",
        ] {
            assert!(parse_transcript_line(skip).is_none(), "{skip}");
        }
    }

    #[test]
    fn merges_streamed_lines_and_round_trips() {
        let mut a = Usage {
            message_id: "m".into(),
            output: 5,
            tool_use_ids: vec!["t1".into()],
            ..Default::default()
        };
        let b = Usage {
            message_id: "m".into(),
            output: 90,
            tool_use_ids: vec!["t2".into()],
            ..Default::default()
        };
        a.merge(&b);
        assert_eq!(a.output, 90);
        assert_eq!(a.tool_use_ids, vec!["t1", "t2"]);
        assert_eq!(Usage::from_payload(&a.to_payload("s")), Some(a));
    }
}
