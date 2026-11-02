//! Short human labels for tool calls and turns.

use serde_json::Value;

use crate::event::value_text;

/// Max characters kept for a tool's output preview.
pub const OUTPUT_PREVIEW: usize = 4000;

/// The one-line "what did it touch" label shown next to a tool name,
/// e.g. `src/auth.rs` for Read or `cargo test` for Bash.
pub fn tool_target(name: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).map(value_text);
    let target = match name {
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => field("file_path")
            .or_else(|| field("notebook_path"))
            .map(|p| short_path(&p)),
        "Bash" | "BashOutput" => field("command").map(|c| strip_env_prefix(&c).to_string()),
        "Grep" => field("pattern").map(|p| format!("\"{p}\"")),
        "Glob" => field("pattern"),
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        "Agent" | "Task" => field("description").or_else(|| field("subagent_type")),
        "TodoWrite" => input
            .get("todos")
            .and_then(Value::as_array)
            .map(|t| format!("{} todos", t.len())),
        _ => None,
    };
    let target = target.unwrap_or_else(|| first_string_field(input).unwrap_or_default());
    clip(target.lines().next().unwrap_or(""), 120)
}

/// Title for a main-thread turn built from its tools: `Read ×3 · Grep`.
pub fn turn_title<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for n in names.map(short_tool_name) {
        match counts.iter_mut().find(|(k, _)| *k == n) {
            Some((_, c)) => *c += 1,
            None => counts.push((n, 1)),
        }
    }
    if counts.is_empty() {
        return "Thinking".into();
    }
    let mut parts: Vec<String> = counts
        .iter()
        .take(3)
        .map(|(n, c)| if *c > 1 { format!("{n} ×{c}") } else { (*n).to_string() })
        .collect();
    if counts.len() > 3 {
        parts.push(format!("+{}", counts.len() - 3));
    }
    parts.join(" · ")
}

/// Text preview of a tool response, clipped to [`OUTPUT_PREVIEW`].
pub fn output_preview(v: &Value) -> Option<String> {
    let text = match v {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        Value::Object(o) => ["stdout", "output", "content", "result", "text"]
            .iter()
            .find_map(|k| o.get(*k).map(value_text))
            .unwrap_or_else(|| v.to_string()),
        other => other.to_string(),
    };
    Some(clip(&text, OUTPUT_PREVIEW))
}

/// Copy of a tool input with long strings clipped, so a `Write` of a large
/// file doesn't bloat every snapshot.
pub fn clip_value(v: &Value, max: usize) -> Value {
    match v {
        Value::String(s) if s.chars().count() > max => Value::String(clip(s, max)),
        Value::Array(a) => Value::Array(a.iter().map(|x| clip_value(x, max)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), clip_value(x, max))).collect()),
        other => other.clone(),
    }
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `mcp__server__do_thing` → `do_thing`; other names unchanged.
pub fn short_tool_name(name: &str) -> &str {
    match name.strip_prefix("mcp__") {
        Some(rest) => rest.rsplit("__").next().unwrap_or(rest),
        None => name,
    }
}

/// Drop leading `VAR=value` assignments so the label shows the command:
/// `FOO=1 BAR="a b" cargo test` → `cargo test`.
fn strip_env_prefix(cmd: &str) -> &str {
    let mut rest = cmd.trim_start();
    loop {
        let word_end = word_end(rest);
        let word = &rest[..word_end];
        let is_assign = word.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty()
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !k.starts_with(|c: char| c.is_ascii_digit())
        });
        if !is_assign || word_end == rest.len() {
            return rest;
        }
        rest = rest[word_end..].trim_start();
    }
}

/// End of the first shell word, treating quoted spans as part of it.
fn word_end(s: &str) -> usize {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, c) if c.is_whitespace() => return i,
            _ => {}
        }
    }
    s.len()
}

fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.trim_end_matches('/').rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn first_string_field(v: &Value) -> Option<String> {
    v.as_object()?.values().find_map(|x| x.as_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn targets() {
        assert_eq!(
            tool_target("Read", &json!({"file_path": "/a/b/src/auth.rs"})),
            "src/auth.rs"
        );
        assert_eq!(tool_target("Grep", &json!({"pattern": "expires_at"})), "\"expires_at\"");
        assert_eq!(
            tool_target("Bash", &json!({"command": "cargo test\necho hi"})),
            "cargo test"
        );
    }

    #[test]
    fn titles() {
        let t = turn_title(["Read", "Read", "Grep", "Read"].into_iter());
        assert_eq!(t, "Read ×3 · Grep");
        assert_eq!(turn_title(std::iter::empty()), "Thinking");
        assert_eq!(
            turn_title(["mcp__ccd_turn__update_plan", "Bash"].into_iter()),
            "update_plan · Bash"
        );
    }
}
