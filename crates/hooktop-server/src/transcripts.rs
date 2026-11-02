//! Reading token usage from Claude Code's transcripts.
//!
//! Every hook names the session's transcript (`transcript_path`), and
//! `SubagentStop` names the subagent's (`agent_transcript_path`). After a
//! hook, hooktop reads whatever was appended since last time, keeps the
//! usage numbers of each assistant message (never the text), and records
//! them as `HooktopUsage` events.
//!
//! Only `.jsonl` files inside Claude Code's config folder (`~/.claude`, or
//! `$CLAUDE_CONFIG_DIR`) are read, so a hook can't point hooktop at any
//! other file.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hooktop_core::HookPayload;
use hooktop_core::usage::{Usage, parse_transcript_line};

/// Don't re-read a transcript more often than this, except on the hooks
/// that end a turn (where the final numbers land).
const MIN_GAP: Duration = Duration::from_millis(1500);
/// Hooks after which the transcript is always read.
const ALWAYS: &[&str] = &["Stop", "SubagentStop", "PostToolBatch", "SessionEnd"];

#[derive(Default)]
pub struct Transcripts {
    files: Mutex<HashMap<PathBuf, Tail>>,
}

#[derive(Default)]
struct Tail {
    offset: u64,
    last_read: Option<Instant>,
}

/// Claude Code's config folders: where transcripts may live.
fn roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        v.push(PathBuf::from(d));
    }
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".claude"));
    }
    v.into_iter().filter_map(|p| p.canonicalize().ok()).collect()
}

/// `path`, resolved, if it's a transcript hooktop may read.
pub fn allowed(path: &str) -> Option<PathBuf> {
    let p = Path::new(path).canonicalize().ok()?;
    if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return None;
    }
    roots().iter().any(|r| p.starts_with(r)).then_some(p)
}

/// The transcripts a hook points at.
pub fn paths_of(p: &HookPayload) -> Vec<String> {
    let mut v: Vec<String> = p.transcript_path.iter().cloned().collect();
    if let Some(a) = p.extra.get("agent_transcript_path").and_then(|v| v.as_str()) {
        v.push(a.to_string());
    }
    v
}

impl Transcripts {
    /// Should a hook named `hook` trigger a read of `path` now?
    pub fn due(&self, path: &Path, hook: &str) -> bool {
        let files = self.files.lock().unwrap();
        ALWAYS.contains(&hook)
            || files
                .get(path)
                .and_then(|t| t.last_read)
                .is_none_or(|at| at.elapsed() >= MIN_GAP)
    }

    /// Usage in the lines appended to `path` since the last read. Lines of
    /// the same message are merged; a half-written last line waits.
    pub fn read_new(&self, path: &Path) -> Vec<Usage> {
        let start = {
            let mut files = self.files.lock().unwrap();
            let t = files.entry(path.to_path_buf()).or_default();
            t.last_read = Some(Instant::now());
            t.offset
        };
        let Ok(mut f) = File::open(path) else { return Vec::new() };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        // Rewritten or truncated: start over (usage is merged by id anyway).
        let start = if len < start { 0 } else { start };
        if f.seek(SeekFrom::Start(start)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        let complete = &buf[..=end];
        self.files.lock().unwrap().entry(path.to_path_buf()).or_default().offset = start + complete.len() as u64;

        let mut out: Vec<Usage> = Vec::new();
        for line in complete.split(|b| *b == b'\n') {
            // Cheap filter before parsing JSON: most lines aren't usage.
            if !line.windows(7).any(|w| w == b"\"usage\"") {
                continue;
            }
            let Some(u) = std::str::from_utf8(line).ok().and_then(parse_transcript_line) else {
                continue;
            };
            match out.iter_mut().find(|x| x.message_id == u.message_id) {
                Some(x) => x.merge(&u),
                None => out.push(u),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(id: &str, out: u64, tool: Option<&str>) -> String {
        let content = match tool {
            Some(t) => format!(r#"[{{"type":"tool_use","id":"{t}","name":"Read","input":{{}}}}]"#),
            None => r#"[{"type":"text","text":"hi"}]"#.to_string(),
        };
        format!(
            r#"{{"type":"assistant","timestamp":"2026-11-02T13:00:00Z","message":{{"id":"{id}","model":"claude-opus-5-5","content":{content},"usage":{{"input_tokens":10,"output_tokens":{out}}}}}}}"#
        )
    }

    #[test]
    fn tails_only_new_complete_lines() {
        let dir = std::env::temp_dir().join(format!("hooktop-transcript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut f = File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"content":"secret"}}}}"#).unwrap();
        writeln!(f, "{}", line("m1", 5, Some("t1"))).unwrap();
        writeln!(f, "{}", line("m1", 50, None)).unwrap();
        write!(f, "{}", line("m2", 7, None)).unwrap(); // no newline yet
        f.flush().unwrap();

        let t = Transcripts::default();
        let first = t.read_new(&path);
        assert_eq!(first.len(), 1, "half-written line waits");
        assert_eq!(first[0].output, 50, "lines of one message merge");
        assert_eq!(first[0].tool_use_ids, vec!["t1"]);

        writeln!(f).unwrap();
        writeln!(f, "{}", line("m3", 9, None)).unwrap();
        f.flush().unwrap();
        let ids: Vec<String> = t.read_new(&path).into_iter().map(|u| u.message_id).collect();
        assert_eq!(ids, vec!["m2", "m3"]);
        assert!(t.read_new(&path).is_empty());
        assert!(!t.due(&path, "PostToolUse"), "throttled");
        assert!(t.due(&path, "Stop"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_transcripts_under_the_claude_folder() {
        let dir = std::env::temp_dir().join(format!("hooktop-claude-dir-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        let inside = dir.join("projects").join("s.jsonl");
        std::fs::write(&inside, "").unwrap();
        let outside = std::env::temp_dir().join(format!("hooktop-outside-{}.jsonl", std::process::id()));
        std::fs::write(&outside, "").unwrap();
        // SAFETY: tests in this module don't run anything else that reads
        // CLAUDE_CONFIG_DIR concurrently.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };
        assert!(allowed(inside.to_str().unwrap()).is_some());
        assert!(
            allowed(outside.to_str().unwrap()).is_none(),
            "outside the config folder"
        );
        assert!(allowed("/etc/passwd").is_none());
        let sneaky = format!(
            "{}/projects/../../{}",
            dir.display(),
            outside.file_name().unwrap().to_str().unwrap()
        );
        assert!(allowed(&sneaky).is_none(), "no climbing out with ..");
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_file(outside).unwrap();
    }
}
