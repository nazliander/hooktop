//! Append-only event log in SQLite.
//!
//! Raw hook payloads are stored (not derived graphs), so improving the
//! grouping logic in `hooktop-core` re-applies to old sessions on restart.
//! Writes go through one background thread that batches inserts into a
//! single transaction, so the hook endpoint never waits on disk.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use hooktop_core::HookPayload;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub seq: u64,
    pub at_ms: i64,
    pub payload: HookPayload,
}

/// A stored event as the `/events` endpoint returns it.
#[derive(Debug, Serialize)]
pub struct EventRow {
    pub seq: u64,
    pub at_ms: i64,
    pub hook: String,
    pub payload: serde_json::Value,
}

enum Msg {
    Append(Box<StoredEvent>),
    /// Delete every stored event of these sessions (after earlier appends).
    Delete(Vec<String>),
    Shutdown(mpsc::Sender<()>),
}

#[derive(Clone)]
pub struct StoreWriter {
    tx: mpsc::Sender<Msg>,
}

impl StoreWriter {
    pub fn append(&self, e: StoredEvent) {
        if self.tx.send(Msg::Append(Box::new(e))).is_err() {
            tracing::error!("event store writer is gone; event not persisted");
        }
    }

    /// Delete sessions' stored events, after any appends already queued.
    pub fn delete(&self, sessions: Vec<String>) {
        if self.tx.send(Msg::Delete(sessions)).is_err() {
            tracing::error!("event store writer is gone; sessions not deleted");
        }
    }

    /// Flush queued events and stop the writer thread.
    pub fn shutdown(&self, timeout: Duration) {
        let (done_tx, done_rx) = mpsc::channel();
        if self.tx.send(Msg::Shutdown(done_tx)).is_ok() {
            let _ = done_rx.recv_timeout(timeout);
        }
    }
}

pub struct Store {
    pub path: PathBuf,
    pub writer: StoreWriter,
    _thread: JoinHandle<()>,
}

const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    CREATE TABLE IF NOT EXISTS events (
        seq        INTEGER PRIMARY KEY,
        at_ms      INTEGER NOT NULL,
        session_id TEXT    NOT NULL,
        hook       TEXT    NOT NULL,
        payload    TEXT    NOT NULL
    );
    CREATE INDEX IF NOT EXISTS events_by_session ON events (session_id, seq);
";

impl Store {
    /// Open (or create) the database, returning the store and every event
    /// already in it, in seq order.
    pub fn open(path: &Path) -> Result<(Self, Vec<StoredEvent>)> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(SCHEMA)?;
        let existing = load_all(&conn)?;

        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("hooktop-store".into())
            .spawn(move || writer_loop(conn, rx))?;
        let store = Self {
            path: path.to_path_buf(),
            writer: StoreWriter { tx },
            _thread: thread,
        };
        Ok((store, existing))
    }
}

fn load_all(conn: &Connection) -> Result<Vec<StoredEvent>> {
    let mut stmt = conn.prepare("SELECT seq, at_ms, payload FROM events ORDER BY seq")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, at_ms, json) = row?;
        match serde_json::from_str::<HookPayload>(&json) {
            Ok(payload) => out.push(StoredEvent {
                seq: seq as u64,
                at_ms,
                payload,
            }),
            Err(e) => tracing::warn!(seq, "skipping unreadable stored event: {e}"),
        }
    }
    Ok(out)
}

fn writer_loop(mut conn: Connection, rx: mpsc::Receiver<Msg>) {
    const MAX_BATCH: usize = 1024;
    let flush = |conn: &mut Connection, batch: &mut Vec<StoredEvent>| {
        if let Err(e) = write_batch(conn, batch) {
            tracing::error!("failed to persist {} events: {e:#}", batch.len());
        }
        batch.clear();
    };
    while let Ok(first) = rx.recv() {
        let mut batch = Vec::new();
        let mut next = Some(first);
        // Handle messages in order, batching appends between deletes.
        while let Some(m) = next.take() {
            match m {
                Msg::Append(e) => batch.push(*e),
                Msg::Delete(sessions) => {
                    flush(&mut conn, &mut batch);
                    if let Err(e) = delete_sessions(&conn, &sessions) {
                        tracing::error!("failed to delete sessions: {e:#}");
                    }
                }
                Msg::Shutdown(done) => {
                    flush(&mut conn, &mut batch);
                    let _ = done.send(());
                    return;
                }
            }
            if batch.len() < MAX_BATCH {
                next = rx.try_recv().ok();
            }
        }
        flush(&mut conn, &mut batch);
    }
}

fn delete_sessions(conn: &Connection, sessions: &[String]) -> Result<()> {
    let mut stmt = conn.prepare_cached("DELETE FROM events WHERE session_id = ?1")?;
    for id in sessions {
        stmt.execute([id])?;
    }
    Ok(())
}

fn write_batch(conn: &mut Connection, batch: &[StoredEvent]) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO events (seq, at_ms, session_id, hook, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for e in batch {
            let json = serde_json::to_string(&e.payload)?;
            stmt.execute(params![
                e.seq as i64,
                e.at_ms,
                e.payload.session_id,
                e.payload.hook_event_name,
                json
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Read one session's stored events after `after`, oldest first.
/// Opens its own read-only connection; call from a blocking context.
pub fn read_events(path: &Path, session_id: &str, after: u64, limit: u32) -> Result<Vec<EventRow>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT seq, at_ms, hook, payload FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![session_id, after as i64, limit], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, at_ms, hook, json) = row?;
        let payload = serde_json::from_str(&json).unwrap_or(serde_json::Value::String(json));
        out.push(EventRow {
            seq: seq as u64,
            at_ms,
            hook,
            payload,
        });
    }
    Ok(out)
}

pub fn default_db_path() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hooktop")
        .join("events.db")
}

/// How much a reset would remove: `(sessions, events)`.
pub fn count(path: &Path, session_id: Option<&str>) -> Result<(u64, u64)> {
    if !path.exists() {
        return Ok((0, 0));
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (sql, arg) = match session_id {
        Some(id) => (
            "SELECT COUNT(DISTINCT session_id), COUNT(*) FROM events WHERE session_id = ?1",
            Some(id),
        ),
        None => ("SELECT COUNT(DISTINCT session_id), COUNT(*) FROM events", None),
    };
    let mut stmt = conn.prepare(sql)?;
    let row = |r: &rusqlite::Row| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64));
    Ok(match arg {
        Some(a) => stmt.query_row([a], row)?,
        None => stmt.query_row([], row)?,
    })
}

/// Delete one session's events, or the whole database (and its WAL files).
pub fn reset(path: &Path, session_id: Option<&str>) -> Result<()> {
    match session_id {
        Some(id) => {
            let conn = Connection::open(path)?;
            conn.execute("DELETE FROM events WHERE session_id = ?1", [id])?;
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        }
        None => {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = path.as_os_str().to_owned();
                p.push(suffix);
                match std::fs::remove_file(&p) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("removing {}", PathBuf::from(p).display())),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(session: &str) -> HookPayload {
        HookPayload {
            session_id: session.into(),
            hook_event_name: "Stop".into(),
            ..Default::default()
        }
    }

    #[test]
    fn reset_one_session_or_everything() {
        let dir = std::env::temp_dir().join(format!("hooktop-reset-test-{}", std::process::id()));
        let db = dir.join("events.db");
        {
            let (store, _) = Store::open(&db).unwrap();
            for (i, s) in ["a", "a", "b"].iter().enumerate() {
                store.writer.append(StoredEvent {
                    seq: i as u64 + 1,
                    at_ms: 0,
                    payload: payload(s),
                });
            }
            store.writer.shutdown(Duration::from_secs(2));
        }
        assert_eq!(count(&db, None).unwrap(), (2, 3));
        reset(&db, Some("a")).unwrap();
        assert_eq!(count(&db, None).unwrap(), (1, 1));
        reset(&db, None).unwrap();
        assert!(!db.exists());
        assert_eq!(count(&db, None).unwrap(), (0, 0));
        std::fs::remove_dir_all(dir).ok();
    }
}
