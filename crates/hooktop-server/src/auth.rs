//! Who may talk to hooktop.
//!
//! hooktop listens on loopback, but "only this machine" still includes every
//! web page open in your browser and every other process. Three checks run
//! before any route:
//!
//! 1. **No browsers.** Browsers stamp cross-site requests and every
//!    WebSocket handshake with `Origin`; Claude Code and the TUI never send
//!    it. Anything with an `Origin` header is refused.
//! 2. **Only local host names.** A `Host` other than `localhost`,
//!    `127.0.0.1` or `[::1]` means DNS rebinding (an attacker's domain
//!    resolving to 127.0.0.1), so it's refused.
//! 3. **A shared secret.** Every request needs `Authorization: Bearer
//!    <token>`. The token is generated on first run and kept in a file only
//!    your user can read; `hooktop hooks install` writes it into Claude Code's
//!    hooks, and the TUI reads it from the same file.
//!
//! `/hook` also requires `Content-Type: application/json`, which a page
//! can't send cross-site without a CORS preflight that hooktop never grants.
//!
//! **Rotation.** `hooktop token rotate` writes a new token file; a running
//! server notices and switches without a restart. Claude Code reads hook
//! settings when a session starts, so sessions already running keep sending
//! the old token: by default it stays valid for `/hook` only (never for
//! reading data) during a grace period, kept in `token.previous`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::http::AppState;

/// Where the token lives: next to the event store.
pub fn token_path() -> PathBuf {
    crate::store::default_db_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("token")
}

/// The token to use: `$HOOKTOP_TOKEN` if set, otherwise the token file
/// (`file`, or the default one), created on first use.
///
/// There's deliberately no `--token <value>` flag: command lines are
/// visible to every user through `ps`. Environment variables and files
/// aren't.
pub fn resolve(file: Option<&Path>) -> Result<String> {
    if let Ok(t) = std::env::var("HOOKTOP_TOKEN")
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    match file {
        Some(f) => {
            let t = fs::read_to_string(f).with_context(|| format!("reading the token file {}", f.display()))?;
            let t = t.trim();
            anyhow::ensure!(
                t.len() >= 16,
                "the token in {} is shorter than 16 characters",
                f.display()
            );
            Ok(t.to_string())
        }
        None => load_or_create(&token_path()),
    }
}

/// Can anyone but this user read `path`?
pub fn readable_by_others(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o077 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Read the token at `path`, or create a new random one there that only
/// this user can read.
pub fn load_or_create(path: &Path) -> Result<String> {
    if let Ok(text) = fs::read_to_string(path) {
        let t = text.trim();
        if t.len() >= 16 {
            return Ok(t.to_string());
        }
    }
    let token = generate()?;
    write_private(path, &token)?;
    Ok(token)
}

/// 32 random bytes, hex encoded.
fn generate() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness for a token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Atomically replace `path` with `text`, readable only by this user.
fn write_private(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = private_file(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.write_all(b"\n")?;
    }
    fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))
}

/// Where the outgoing token waits out its grace period.
pub fn previous_path(token_file: &Path) -> PathBuf {
    token_file.with_extension("previous")
}

/// Replace the token at `path` with a new one. With a `grace_ms` above
/// zero the old token keeps working for hooks until `now_ms + grace_ms`.
pub fn rotate(path: &Path, grace_ms: i64, now_ms: i64) -> Result<String> {
    let old = fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| t.len() >= 16);
    let new = generate()?;
    let prev = previous_path(path);
    match old {
        Some(old) if grace_ms > 0 => write_private(&prev, &format!("{old} {}", now_ms + grace_ms))?,
        _ => match fs::remove_file(&prev) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", prev.display())),
        },
    }
    write_private(path, &new)?;
    Ok(new)
}

/// The token(s) a server accepts, re-read from disk when the files change.
pub struct Tokens {
    file: Option<PathBuf>,
    cache: Mutex<Cache>,
}

struct Cache {
    stamp: Stamp,
    current: String,
    /// The previous token and when it stops working for hooks (ms).
    previous: Option<(String, i64)>,
}

type Stamp = (Option<(SystemTime, u64)>, Option<(SystemTime, u64)>);

fn stamp_of(path: &Path) -> Option<(SystemTime, u64)> {
    let m = fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret.
        f.debug_struct("Tokens")
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

impl Tokens {
    /// One token that never changes (`HOOKTOP_TOKEN`, tests).
    pub fn fixed(token: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            file: None,
            cache: Mutex::new(Cache {
                stamp: (None, None),
                current: token.into(),
                previous: None,
            }),
        })
    }

    /// The token in `path` (created if missing), followed as it rotates.
    pub fn from_file(path: PathBuf) -> Result<Arc<Self>> {
        let current = load_or_create(&path)?;
        let t = Arc::new(Self {
            cache: Mutex::new(Cache {
                stamp: (None, None),
                current,
                previous: None,
            }),
            file: Some(path),
        });
        drop(t.refresh());
        Ok(t)
    }

    /// `$HOOKTOP_TOKEN` if set, else `file` (which must exist), else the
    /// default token file.
    pub fn resolve(file: Option<&Path>) -> Result<Arc<Self>> {
        if std::env::var("HOOKTOP_TOKEN").is_ok_and(|t| !t.trim().is_empty()) {
            return Ok(Self::fixed(resolve(None)?));
        }
        match file {
            Some(f) => {
                resolve(Some(f))?;
                Self::from_file(f.to_path_buf())
            }
            None => Self::from_file(token_path()),
        }
    }

    /// The file this token rotates through, if any.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    fn refresh(&self) -> std::sync::MutexGuard<'_, Cache> {
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let Some(path) = &self.file else { return c };
        let prev = previous_path(path);
        let stamp = (stamp_of(path), stamp_of(&prev));
        if stamp != c.stamp {
            if let Ok(t) = fs::read_to_string(path)
                && t.trim().len() >= 16
            {
                if t.trim() != c.current {
                    tracing::info!("hooktop's token changed on disk; using the new one");
                }
                c.current = t.trim().to_string();
            }
            c.previous = fs::read_to_string(&prev).ok().and_then(|s| {
                let (tok, until) = s.trim().split_once(' ')?;
                Some((tok.to_string(), until.parse().ok()?))
            });
            c.stamp = stamp;
        }
        c
    }

    pub fn current(&self) -> String {
        self.refresh().current.clone()
    }

    /// Does `got` let a request in? The previous token, within its grace
    /// period, is enough to send hooks but not to read anything.
    pub fn accepts(&self, got: &str, hook: bool) -> bool {
        let c = self.refresh();
        if same(got.as_bytes(), c.current.as_bytes()) {
            return true;
        }
        hook && c
            .previous
            .as_ref()
            .is_some_and(|(p, until)| crate::now_ms() < *until && same(got.as_bytes(), p.as_bytes()))
    }
}

/// `90s`, `30m`, `12h`, `7d` (or plain seconds) in milliseconds.
pub fn parse_duration(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num
        .parse()
        .map_err(|_| format!("expected a duration like 30m, 12h or 7d, got {s:?}"))?;
    let ms = match unit {
        "" | "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(format!("unknown unit {unit:?}; use s, m, h or d")),
    };
    Ok(n * ms)
}

/// Create `dir` (and parents) readable only by this user.
pub fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", dir.display()))?;
    }
    Ok(())
}

/// Open `path` for writing (truncating), readable only by this user.
pub fn private_file(path: &Path) -> Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(path).with_context(|| format!("opening {}", path.display()))?;
    restrict(path);
    Ok(f)
}

/// Make an existing file readable only by this user (best effort).
pub fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Is `host` (a `Host` header value, maybe with a port) this machine?
pub fn is_local_host(host: &str, extra: &[String]) -> bool {
    let name = match host.strip_prefix('[') {
        // [::1]:4317
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    let name = name.to_ascii_lowercase();
    matches!(name.as_str(), "localhost" | "127.0.0.1" | "::1") || extra.iter().any(|e| e.eq_ignore_ascii_case(&name))
}

/// Compare without leaking how many leading bytes matched.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    v.strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))
        .map(str::trim)
}

fn refuse(status: StatusCode, why: &'static str) -> Response {
    tracing::warn!(%status, "refused a request: {why}");
    (status, why).into_response()
}

/// The checks above, as axum middleware for every route.
pub async fn guard(State(st): State<AppState>, req: Request<Body>, next: Next) -> Response {
    let h = req.headers();
    if h.contains_key(header::ORIGIN) {
        return refuse(StatusCode::FORBIDDEN, "browsers can't talk to hooktop");
    }
    if let Some(host) = h.get(header::HOST).and_then(|v| v.to_str().ok())
        && !is_local_host(host, &st.allowed_hosts)
    {
        return refuse(StatusCode::MISDIRECTED_REQUEST, "unexpected Host header");
    }
    let is_hook = req.uri().path() == "/hook";
    if let Some(tokens) = &st.token
        && !bearer(h).is_some_and(|got| tokens.accepts(got, is_hook))
    {
        return refuse(StatusCode::UNAUTHORIZED, "missing or wrong hooktop token");
    }
    st.activity.touch();
    if req.uri().path() == "/hook" {
        let json = h
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
            });
        if !json {
            return refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, "hooks must be application/json");
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hosts() {
        for ok in ["127.0.0.1:4317", "localhost", "LOCALHOST:1", "[::1]:4317", "127.0.0.1"] {
            assert!(is_local_host(ok, &[]), "{ok}");
        }
        for bad in [
            "evil.example:4317",
            "127.0.0.1.evil.example",
            "192.168.1.5:4317",
            "[::2]:1",
        ] {
            assert!(!is_local_host(bad, &[]), "{bad}");
        }
        assert!(is_local_host("devbox:4317", &["devbox".into()]));
    }

    #[test]
    fn creates_a_private_token_once() {
        let dir = std::env::temp_dir().join(format!("hooktop-token-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("hooktop").join("token");
        let a = load_or_create(&path).unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(load_or_create(&path).unwrap(), a, "reused, not regenerated");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(
                fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let given = dir.join("given");
        fs::write(&given, " 0123456789abcdef0123 \n").unwrap();
        assert_eq!(resolve(Some(&given)).unwrap(), "0123456789abcdef0123");
        fs::write(&given, "short").unwrap();
        assert!(resolve(Some(&given)).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_is_picked_up_with_a_hook_only_grace_period() {
        let dir = std::env::temp_dir().join(format!("hooktop-rotate-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("token");
        let tokens = Tokens::from_file(path.clone()).unwrap();
        let old = tokens.current();

        let new = rotate(&path, 60_000, crate::now_ms()).unwrap();
        assert_ne!(new, old);
        assert_eq!(tokens.current(), new, "a running server follows the file");
        assert!(tokens.accepts(&new, false));
        assert!(tokens.accepts(&old, true), "old token still sends hooks");
        assert!(!tokens.accepts(&old, false), "but can't read anything");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let prev = previous_path(&path);
            assert_eq!(fs::metadata(prev).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // An expired grace period, then an immediate rotation.
        let newer = rotate(&path, 1, crate::now_ms() - 10).unwrap();
        assert!(!tokens.accepts(&new, true));
        let newest = rotate(&path, 0, crate::now_ms()).unwrap();
        assert!(!tokens.accepts(&newer, true), "--now leaves no grace");
        assert!(tokens.accepts(&newest, false));
        assert!(!previous_path(&path).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("12h"), Ok(12 * 3_600_000));
        assert_eq!(parse_duration("90"), Ok(90_000));
        assert_eq!(parse_duration("7d"), Ok(7 * 86_400_000));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("3w").is_err());
        assert!(
            format!("{:?}", Tokens::fixed("secret-value")).contains("Tokens")
                && !format!("{:?}", Tokens::fixed("secret-value")).contains("secret")
        );
    }

    #[test]
    fn constant_time_compare() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
    }
}
