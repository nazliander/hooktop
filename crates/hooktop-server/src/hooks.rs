//! Adding hooktop's `http` hooks to (and removing them from) a Claude Code
//! `settings.json`, without touching anything else in the file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// The hook events hooktop listens to.
pub const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PostToolBatch",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "SessionEnd",
    "Notification",
];

/// Seconds Claude Code waits for hooktop. The endpoint answers immediately,
/// so this only matters if the server is wedged.
const TIMEOUT_SECS: u64 = 5;

pub fn default_settings_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("settings.json")
}

pub fn hook_url(host: &str, port: u16) -> String {
    format!("http://{host}:{port}/hook")
}

/// How the hooks prove they're Claude Code to hooktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAuth {
    /// The token itself, written into settings.json.
    Token(String),
    /// `$HOOKTOP_TOKEN`, read from Claude Code's environment (for a
    /// settings.json you share or commit).
    FromEnv,
}

fn hooktop_hook(url: &str, auth: &HookAuth) -> Value {
    let mut h = json!({ "type": "http", "url": url, "timeout": TIMEOUT_SECS });
    match auth {
        HookAuth::Token(t) => {
            h["headers"] = json!({ "Authorization": format!("Bearer {t}") });
        }
        HookAuth::FromEnv => {
            h["headers"] = json!({ "Authorization": "Bearer $HOOKTOP_TOKEN" });
            h["allowedEnvVars"] = json!(["HOOKTOP_TOKEN"]);
        }
    }
    h
}

/// The `hooks` block hooktop needs, for printing or merging.
pub fn hooks_block(url: &str, auth: &HookAuth) -> Value {
    let mut hooks = Map::new();
    for ev in EVENTS {
        hooks.insert((*ev).into(), json!([{ "hooks": [hooktop_hook(url, auth)] }]));
    }
    json!({ "hooks": hooks })
}

fn is_hooktop_hook(h: &Value, url: &str) -> bool {
    h.get("type").and_then(Value::as_str) == Some("http") && h.get("url").and_then(Value::as_str) == Some(url)
}

/// What [`merge`] changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Merged {
    pub added: usize,
    /// Existing hooktop hooks whose headers were brought up to date.
    pub updated: usize,
    /// settings.json was readable by other users and now isn't.
    pub made_private: bool,
}

/// Merge hooktop's hooks into `settings`, adding missing ones and updating
/// existing ones to send the current token.
pub fn merge(settings: &mut Value, url: &str, auth: &HookAuth) -> Result<Merged> {
    let Some(root) = settings.as_object_mut() else {
        bail!("settings.json is not a JSON object")
    };
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("`hooks` in settings.json is not an object")
    };

    let want = hooktop_hook(url, auth);
    let mut out = Merged::default();
    for ev in EVENTS {
        let groups = hooks.entry(*ev).or_insert_with(|| json!([]));
        let Some(groups) = groups.as_array_mut() else {
            bail!("`hooks.{ev}` is not an array")
        };
        let mut present = false;
        for g in groups.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                for h in hs.iter_mut().filter(|h| is_hooktop_hook(h, url)) {
                    present = true;
                    if *h != want {
                        *h = want.clone();
                        out.updated += 1;
                    }
                }
            }
        }
        if !present {
            groups.push(json!({ "hooks": [want.clone()] }));
            out.added += 1;
        }
    }
    Ok(out)
}

/// Are Claude Code's hooks set up to reach a server that wants `token`?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookCheck {
    Ok,
    /// No hooktop hooks for this URL.
    NotInstalled,
    /// hooktop hooks without the right token: they'd be refused.
    WrongToken,
}

pub fn check(path: &Path, url: &str, token: Option<&str>) -> HookCheck {
    let Ok(settings) = read_settings(path) else {
        return HookCheck::NotInstalled;
    };
    let mine: Vec<&Value> = settings
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|g| g.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|h| is_hooktop_hook(h, url))
        .collect();
    if mine.is_empty() {
        return HookCheck::NotInstalled;
    }
    let Some(token) = token else { return HookCheck::Ok };
    let ok = mine.iter().all(|h| {
        let auth = h.pointer("/headers/Authorization").and_then(Value::as_str);
        auth == Some(&format!("Bearer {token}")) || auth == Some("Bearer $HOOKTOP_TOKEN")
    });
    if ok { HookCheck::Ok } else { HookCheck::WrongToken }
}

/// Remove hooktop's hooks (and any groups/events left empty). Returns how many
/// hook entries were removed.
pub fn unmerge(settings: &mut Value, url: &str) -> usize {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut removed = 0;
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else { continue };
        for g in groups.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = hs.len();
                hs.retain(|h| !is_hooktop_hook(h, url));
                removed += before - hs.len();
            }
        }
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|hs| !hs.is_empty()));
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|a| !a.is_empty()));
    removed
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write `settings` back, keeping a one-time backup of the original file.
/// `private` makes the file readable by this user only.
fn write_settings(path: &Path, settings: &Value, private: bool) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        let backup = path.with_extension("json.hooktop-backup");
        if !backup.exists() {
            std::fs::copy(path, &backup).with_context(|| format!("backing up to {}", backup.display()))?;
        }
    }
    let tmp = path.with_extension("json.hooktop-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(settings)? + "\n")?;
    // The file now holds hooktop's token: keep the original's permissions,
    // or make a new file private.
    match std::fs::metadata(path) {
        Ok(m) if !private => std::fs::set_permissions(&tmp, m.permissions())?,
        _ => crate::auth::restrict(&tmp),
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn install(path: &Path, url: &str, auth: &HookAuth) -> Result<Merged> {
    let mut settings = read_settings(path)?;
    let mut merged = merge(&mut settings, url, auth)?;
    // With the token written in, other users mustn't be able to read the
    // file (it's often created 644, and macOS home folders are 755).
    let private = matches!(auth, HookAuth::Token(_));
    merged.made_private = private && crate::auth::readable_by_others(path);
    if merged != Merged::default() {
        write_settings(path, &settings, private)?;
        let backup = path.with_extension("json.hooktop-backup");
        if private {
            crate::auth::restrict(&backup);
        }
    }
    Ok(merged)
}

/// After a rotation: put `token` into hooktop hooks that carry a token of
/// their own (hooks using `$HOOKTOP_TOKEN` are left alone). Returns how many
/// hooks changed; adds none.
pub fn retoken(path: &Path, url: &str, token: &str) -> Result<usize> {
    let mut settings = read_settings(path)?;
    let want = format!("Bearer {token}");
    let mut n = 0;
    if let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) {
        for h in hooks
            .values_mut()
            .filter_map(Value::as_array_mut)
            .flatten()
            .filter_map(|g| g.get_mut("hooks").and_then(Value::as_array_mut))
            .flatten()
            .filter(|h| is_hooktop_hook(h, url))
        {
            let Some(auth) = h.pointer_mut("/headers/Authorization") else {
                continue;
            };
            if auth.as_str().is_some_and(|a| a != want && a != "Bearer $HOOKTOP_TOKEN") {
                *auth = Value::from(want.clone());
                n += 1;
            }
        }
    }
    if n > 0 {
        write_settings(path, &settings, true)?;
    }
    Ok(n)
}

pub fn uninstall(path: &Path, url: &str) -> Result<usize> {
    let mut settings = read_settings(path)?;
    let removed = unmerge(&mut settings, url);
    if removed > 0 {
        write_settings(path, &settings, false)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "http://127.0.0.1:4317/hook";

    #[test]
    fn merge_is_idempotent_and_keeps_other_hooks() {
        let mut s = json!({
            "model": "opus",
            "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "lint" }] }] }
        });
        let auth = HookAuth::Token("t1".into());
        assert_eq!(merge(&mut s, URL, &auth).unwrap().added, EVENTS.len());
        assert_eq!(merge(&mut s, URL, &auth).unwrap(), Merged::default());
        assert_eq!(
            s["hooks"]["Stop"][0]["hooks"][0]["headers"]["Authorization"],
            "Bearer t1"
        );
        // A new token updates the existing entries in place.
        let again = merge(&mut s, URL, &HookAuth::Token("t2".into())).unwrap();
        assert_eq!((again.added, again.updated), (0, EVENTS.len()));
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(s["model"], "opus");
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);

        assert_eq!(unmerge(&mut s, URL), EVENTS.len());
        assert_eq!(s["hooks"].as_object().unwrap().len(), 1);
        assert_eq!(s["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "lint");
    }

    #[test]
    fn install_writes_a_backup() {
        let dir = std::env::temp_dir().join(format!("hooktop-hooks-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
        install(&path, URL, &HookAuth::FromEnv).unwrap();
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["theme"], "dark");
        assert_eq!(
            written["hooks"]["Stop"][0]["hooks"][0]["allowedEnvVars"][0],
            "HOOKTOP_TOKEN"
        );
        assert!(dir.join("settings.json.hooktop-backup").exists());
        assert_eq!(
            check(&path, URL, Some("anything")),
            HookCheck::Ok,
            "$HOOKTOP_TOKEN can't be checked"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let m = install(&path, URL, &HookAuth::Token("abcdefabcdefabcdef".into())).unwrap();
            assert!(m.made_private);
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        install(&path, URL, &HookAuth::Token("abc".into())).unwrap();
        assert_eq!(check(&path, URL, Some("abc")), HookCheck::Ok);
        assert_eq!(check(&path, URL, Some("other")), HookCheck::WrongToken);
        assert_eq!(retoken(&path, URL, "rotated-token").unwrap(), EVENTS.len());
        assert_eq!(check(&path, URL, Some("rotated-token")), HookCheck::Ok);
        assert_eq!(retoken(&path, URL, "rotated-token").unwrap(), 0);
        uninstall(&path, URL).unwrap();
        assert_eq!(check(&path, URL, Some("abc")), HookCheck::NotInstalled);
        assert_eq!(retoken(&path, URL, "x").unwrap(), 0, "adds nothing");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
