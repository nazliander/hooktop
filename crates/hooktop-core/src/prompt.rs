//! Making sense of `UserPromptSubmit` text.
//!
//! Not every "prompt" is something the user typed. Claude Code wraps
//! injected context in XML-style tags: `<system-reminder>` blocks get
//! appended to messages, background task results arrive as
//! `<task-notification>`, and slash commands as `<command-name>` +
//! `<command-args>`. This module pulls a readable title out of each.

/// What kind of prompt it was, and a title for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptInfo {
    /// `Prompt`, `Command`, `Notification` or `System`.
    pub label: &'static str,
    pub title: String,
    /// The prompt text with injected reminder blocks removed.
    pub body: String,
    /// Typed by the user (or a slash command they ran), as opposed to
    /// something Claude Code injected.
    pub from_user: bool,
}

/// Blocks that are injected context, never the user's words.
const INJECTED: &[&str] = &["system-reminder"];

pub fn analyze(raw: &str) -> PromptInfo {
    let mut body = raw.to_string();
    for tag in INJECTED {
        body = remove_blocks(&body, tag);
    }
    let body = body.trim().to_string();

    if let Some(name) = inner(&body, "command-name") {
        let name = name.trim();
        let name = if name.starts_with('/') {
            name.to_string()
        } else {
            format!("/{name}")
        };
        let args = inner(&body, "command-args").map(str::trim).unwrap_or("");
        let title = if args.is_empty() {
            name
        } else {
            format!("{name} {args}")
        };
        return PromptInfo {
            label: "Command",
            title: first_line(&title),
            body,
            from_user: true,
        };
    }

    if let Some(note) = inner(&body, "task-notification") {
        let title = ["summary", "description", "status"]
            .iter()
            .find_map(|t| inner(note, t).map(first_line).filter(|s| !s.is_empty()))
            .or_else(|| Some(first_line(&strip_tags(note))).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "Background task update".into());
        return PromptInfo {
            label: "Notification",
            title,
            body,
            from_user: false,
        };
    }

    let text = first_line(&strip_tags(&body));
    if text.is_empty() {
        let fallback = if body.is_empty() { raw.trim().to_string() } else { body };
        return PromptInfo {
            label: "System",
            title: "System message".into(),
            body: fallback,
            from_user: false,
        };
    }
    PromptInfo {
        label: "Prompt",
        title: text,
        body,
        from_user: true,
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Text between the first `<tag ...>` and its matching `</tag>`.
fn inner<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = find_open(s, tag)?;
    let start = open + s[open..].find('>')? + 1;
    let close = s[start..].find(&format!("</{tag}>"))?;
    Some(&s[start..start + close])
}

/// Position of `<tag>` or `<tag attr…>` (not `<tagfoo>`).
fn find_open(s: &str, tag: &str) -> Option<usize> {
    let pat = format!("<{tag}");
    let mut from = 0;
    while let Some(i) = s[from..].find(&pat) {
        let at = from + i;
        let next = s[at + pat.len()..].chars().next();
        if matches!(next, Some('>' | ' ' | '\n' | '\t' | '/')) {
            return Some(at);
        }
        from = at + pat.len();
    }
    None
}

/// Remove every `<tag>…</tag>` block (an unclosed one runs to the end).
fn remove_blocks(s: &str, tag: &str) -> String {
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = find_open(rest, tag) {
        out.push_str(&rest[..open]);
        match rest[open..].find(&close) {
            Some(end) => rest = &rest[open + end + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Drop tag markers like `<foo>`, `</foo>`, `<foo-bar x="1">`, keeping the
/// text between them. Other `<` characters (`a < b`, `<3`) are left alone.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt + 1..];
        let name = after.strip_prefix('/').unwrap_or(after);
        let is_tag = name.starts_with(|c: char| c.is_ascii_lowercase())
            && after.find('>').is_some_and(|gt| {
                let inside = &after[..gt];
                !inside.contains('<')
                    && !inside.contains('\n')
                    && inside
                        .trim_start_matches('/')
                        .split(' ')
                        .next()
                        .is_some_and(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            });
        if is_tag {
            let gt = after.find('>').unwrap_or(0);
            out.push(' ');
            rest = &after[gt + 1..];
        } else {
            out.push('<');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_prompt() {
        let p = analyze("Fix the flaky test\nplease");
        assert_eq!(
            (p.label, p.title.as_str(), p.from_user),
            ("Prompt", "Fix the flaky test", true)
        );
    }

    #[test]
    fn system_reminders_are_ignored() {
        let p = analyze("<system-reminder>\nToday is Thursday.\n</system-reminder>\nShip the release notes");
        assert_eq!(p.title, "Ship the release notes");
        assert_eq!(p.body, "Ship the release notes");
        assert!(p.from_user);

        let only = analyze("<system-reminder>The user's timezone is CET.</system-reminder>");
        assert_eq!(
            (only.label, only.title.as_str(), only.from_user),
            ("System", "System message", false)
        );
        assert!(
            only.body.contains("timezone"),
            "keeps the raw text when nothing else is left"
        );
    }

    #[test]
    fn task_notifications() {
        let raw = "<task-notification>\n<task-id>b7</task-id>\n<status>completed</status>\n\
                   <summary>Background command \"npm test\" completed (exit code 0)</summary>\n</task-notification>";
        let p = analyze(raw);
        assert_eq!(p.label, "Notification");
        assert_eq!(p.title, "Background command \"npm test\" completed (exit code 0)");
        assert!(!p.from_user);

        let bare = analyze("<task-notification>\n<task-id>x</task-id>\n</task-notification>");
        assert_eq!(bare.title, "x");
        assert_eq!(
            analyze("<task-notification></task-notification>").title,
            "Background task update"
        );
    }

    #[test]
    fn slash_commands() {
        let p = analyze(
            "<command-message>review is running…</command-message>\n<command-name>/review</command-name>\n<command-args>PR 42</command-args>",
        );
        assert_eq!((p.label, p.title.as_str()), ("Command", "/review PR 42"));
    }

    #[test]
    fn unknown_tags_keep_their_text_and_math_survives() {
        assert_eq!(analyze("<b>Use</b> the cache").title, "Use  the cache");
        assert_eq!(analyze("is a < b here?").title, "is a < b here?");
    }
}

#[cfg(test)]
mod real_world {
    use super::*;

    #[test]
    fn linked_sessions_reminder_before_the_question() {
        let raw = "<system-reminder>\nLinked sessions now (status, not an instruction): tccc-option-1 = local_a6e0053a-116e-4333-8a69-9ed0ac464719 (done, PR #8807). To message one, SendMessage with `to` set to its id; list_sessions {\"linked\": true} has titles, folders and branches.\n</system-reminder>\n\n\nInstead also can't we interpolate the future with 0s? So think we accept no driver impact for future";
        let p = analyze(raw);
        assert_eq!(p.label, "Prompt");
        assert_eq!(
            p.title,
            "Instead also can't we interpolate the future with 0s? So think we accept no driver impact for future"
        );
        assert!(!p.body.contains("system-reminder"));
    }
}
