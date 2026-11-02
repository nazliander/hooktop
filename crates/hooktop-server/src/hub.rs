//! In-memory session graphs plus the broadcast channel that feeds
//! WebSocket clients.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use hooktop_core::{Delta, HookPayload, SessionGraph, SessionStatus, SessionSummary, normalize};
use tokio::sync::broadcast;

use crate::procs::Procs;
use crate::store::{StoreWriter, StoredEvent};
use crate::transcripts::Transcripts;

/// One ingested event's effect on one session.
#[derive(Debug)]
pub struct Update {
    /// Set when the session was deleted; `deltas` is then empty.
    pub removed: bool,
    pub session_id: String,
    pub seq: u64,
    pub deltas: Vec<Delta>,
    pub summary: SessionSummary,
}

pub struct Hub {
    sessions: RwLock<HashMap<String, SessionGraph>>,
    next_seq: AtomicU64,
    tx: broadcast::Sender<Arc<Update>>,
    store: Option<StoreWriter>,
    procs: Procs,
    transcripts: Transcripts,
    /// Removed sessions: their stray events are dropped until a new
    /// `SessionStart` (a resumed session) brings them back.
    removed: Mutex<HashSet<String>>,
}

impl Hub {
    pub fn new(store: Option<StoreWriter>) -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self {
            sessions: RwLock::default(),
            next_seq: AtomicU64::new(1),
            tx,
            store,
            procs: Procs::default(),
            transcripts: Transcripts::default(),
            removed: Mutex::default(),
        }
    }

    /// Rebuild state from stored events (no broadcast, no re-persist).
    pub fn restore(&self, events: impl IntoIterator<Item = StoredEvent>) -> usize {
        let mut map = self.sessions.write().unwrap();
        let mut n = 0;
        let mut max_seq = 0;
        for e in events {
            let ev = normalize(&e.payload, e.seq, e.at_ms);
            map.entry(ev.session_id.clone())
                .or_insert_with(|| SessionGraph::new(&ev.session_id, e.at_ms))
                .apply(&ev);
            max_seq = max_seq.max(e.seq);
            n += 1;
        }
        self.next_seq.fetch_max(max_seq + 1, Ordering::SeqCst);
        n
    }

    /// Apply one hook payload: assign a sequence number, update the graph,
    /// persist it (when `persist`), and broadcast the deltas.
    pub fn ingest(&self, payload: HookPayload, at_ms: i64, persist: bool) -> u64 {
        {
            let mut removed = self.removed.lock().unwrap();
            if removed.contains(&payload.session_id) {
                if payload.hook_event_name != "SessionStart" {
                    return self.next_seq.load(Ordering::SeqCst) - 1;
                }
                removed.remove(&payload.session_id);
            }
        }
        // Sequence assignment, apply and broadcast all happen under the write
        // lock so every subscriber sees deltas in seq order.
        let mut map = self.sessions.write().unwrap();
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let ev = normalize(&payload, seq, at_ms);
        let graph = map
            .entry(ev.session_id.clone())
            .or_insert_with(|| SessionGraph::new(&ev.session_id, at_ms));
        let deltas = graph.apply(&ev);
        let summary = graph.summary();

        if persist && let Some(store) = &self.store {
            store.append(StoredEvent { seq, at_ms, payload });
        }
        let _ = self.tx.send(Arc::new(Update {
            removed: false,
            session_id: ev.session_id,
            seq,
            deltas,
            summary,
        }));
        seq
    }

    /// A consistent copy of one session and the last seq it reflects.
    pub fn snapshot(&self, session_id: &str) -> Option<(u64, SessionGraph)> {
        let map = self.sessions.read().unwrap();
        let seq = self.next_seq.load(Ordering::SeqCst) - 1;
        map.get(session_id).map(|g| (seq, g.clone()))
    }

    /// All sessions, most recently active first.
    pub fn sessions(&self) -> Vec<SessionSummary> {
        let map = self.sessions.read().unwrap();
        let mut list: Vec<_> = map.values().map(SessionGraph::summary).collect();
        list.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        list
    }

    /// Forget sessions entirely: memory, process tracking and the store.
    /// Returns the ids that existed. Hooks that arrive later for one of them
    /// start it afresh.
    pub fn remove(&self, ids: &[String]) -> Vec<String> {
        let mut map = self.sessions.write().unwrap();
        let mut gone = Vec::new();
        for id in ids {
            if let Some(g) = map.remove(id) {
                self.procs.forget(id);
                self.removed.lock().unwrap().insert(id.clone());
                gone.push(id.clone());
                let _ = self.tx.send(Arc::new(Update {
                    removed: true,
                    session_id: id.clone(),
                    seq: self.next_seq.load(Ordering::SeqCst) - 1,
                    deltas: Vec::new(),
                    summary: g.summary(),
                }));
            }
        }
        if !gone.is_empty()
            && let Some(store) = &self.store
        {
            store.delete(gone.clone());
        }
        gone
    }

    /// Close connections with nothing heard for `quiet_ms`, the way a
    /// `SessionEnd` would (reason `idle`), dated at their last activity.
    /// Sessions Claude is working in with a live process are left open: a
    /// long build sends no hooks. Returns the ids closed.
    pub fn close_quiet(&self, now_ms: i64, quiet_ms: i64) -> Vec<String> {
        let quiet: Vec<(String, i64)> = {
            let map = self.sessions.read().unwrap();
            map.values()
                .filter(|g| g.status != SessionStatus::Ended && now_ms - g.updated_ms >= quiet_ms)
                .filter(|g| !(g.status == SessionStatus::Running && self.procs.pid(&g.session_id).is_some()))
                .map(|g| (g.session_id.clone(), g.updated_ms))
                .collect()
        };
        for (id, last) in &quiet {
            let payload = HookPayload {
                session_id: id.clone(),
                hook_event_name: "SessionEnd".into(),
                reason: Some("idle".into()),
                ..Default::default()
            };
            self.ingest(payload, *last, true);
        }
        quiet.into_iter().map(|(id, _)| id).collect()
    }

    /// The session's Claude Code process is gone: close the session the
    /// way a `SessionEnd` hook would.
    pub fn process_exited(&self, session_id: &str) {
        self.procs.forget(session_id);
        let open = {
            let map = self.sessions.read().unwrap();
            map.get(session_id).is_some_and(|g| g.status != SessionStatus::Ended)
        };
        if open {
            tracing::info!(session = session_id, "Claude Code exited; closing the connection");
            let payload = HookPayload {
                session_id: session_id.to_string(),
                hook_event_name: "SessionEnd".into(),
                reason: Some("process exited".into()),
                ..Default::default()
            };
            self.ingest(payload, crate::now_ms(), true);
        }
    }

    /// Sessions that are closed, or open with nothing heard for `quiet_ms`
    /// and no live Claude Code process hooktop knows of.
    pub fn stale(&self, now_ms: i64, quiet_ms: i64) -> Vec<String> {
        let map = self.sessions.read().unwrap();
        map.values()
            .filter(|g| {
                g.status == SessionStatus::Ended
                    || (now_ms - g.updated_ms > quiet_ms && self.procs.pid(&g.session_id).is_none())
            })
            .map(|g| g.session_id.clone())
            .collect()
    }

    pub fn transcripts(&self) -> &Transcripts {
        &self.transcripts
    }

    /// Record usage read from a transcript, unless it's already recorded
    /// with the same numbers (transcripts are re-read after a restart).
    pub fn ingest_usage(&self, session_id: &str, u: hooktop_core::usage::Usage) -> bool {
        {
            let map = self.sessions.read().unwrap();
            let Some(g) = map.get(session_id) else { return false };
            let same = g.usage.iter().any(|x| {
                x.message_id == u.message_id
                    && (x.input, x.output, x.cache_read, x.cache_write_5m, x.cache_write_1h)
                        == (u.input, u.output, u.cache_read, u.cache_write_5m, u.cache_write_1h)
                    && u.tool_use_ids.iter().all(|t| x.tool_use_ids.contains(t))
            });
            if same {
                return false;
            }
        }
        let at = if u.at_ms > 0 { u.at_ms } else { crate::now_ms() };
        self.ingest(u.to_payload(session_id), at, true);
        true
    }

    /// Which Claude Code process belongs to which session.
    pub fn procs(&self) -> &Procs {
        &self.procs
    }

    /// Is Claude working in some session whose process hooktop is watching?
    /// (A long build sends no hooks, but hooktop is still in use.) Sessions
    /// without a known process don't count, so a stale one can't keep
    /// hooktop running forever.
    pub fn any_working(&self) -> bool {
        let map = self.sessions.read().unwrap();
        map.values()
            .any(|g| g.status == SessionStatus::Running && self.procs.pid(&g.session_id).is_some())
    }

    /// Is Claude working on a prompt in this session right now?
    pub fn is_working(&self, session_id: &str) -> bool {
        let map = self.sessions.read().unwrap();
        map.get(session_id).is_some_and(|g| g.status == SessionStatus::Running)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.read().unwrap().len()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Update>> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(session: &str, name: &str) -> HookPayload {
        HookPayload {
            session_id: session.into(),
            hook_event_name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn removes_sessions_and_finds_stale_ones() {
        let hub = Hub::new(None);
        let mut rx = hub.subscribe();
        hub.ingest(hook("live", "SessionStart"), 1_000, false);
        hub.ingest(hook("done", "SessionStart"), 1_000, false);
        hub.ingest(hook("done", "SessionEnd"), 2_000, false);
        hub.ingest(hook("quiet", "SessionStart"), 0, false);

        let mut stale = hub.stale(3_600_500, 3_600_000);
        stale.sort();
        assert_eq!(stale, vec!["done".to_string(), "quiet".to_string()]);

        while rx.try_recv().is_ok() {}
        assert_eq!(hub.remove(&stale), stale);
        assert_eq!(hub.session_count(), 1);
        let u = rx.try_recv().unwrap();
        assert!(u.removed);
        assert!(hub.remove(&["nope".into()]).is_empty());

        // Stray events for a removed session are dropped; a new start brings it back.
        hub.ingest(hook("done", "PreToolUse"), 4_000_000, false);
        assert!(hub.snapshot("done").is_none());
        hub.ingest(hook("done", "SessionStart"), 4_000_100, false);
        assert!(hub.snapshot("done").is_some());
    }

    #[test]
    fn quiet_connections_close_and_reopen() {
        let hub = Hub::new(None);
        hub.ingest(hook("waiting", "SessionStart"), 1_000, false);
        hub.ingest(hook("waiting", "Stop"), 2_000, false);
        hub.ingest(hook("fresh", "SessionStart"), 3_590_000, false);
        hub.ingest(hook("building", "UserPromptSubmit"), 1_000, false);
        hub.procs().record("building", Some(42));

        let closed = hub.close_quiet(3_600_000 + 2_000, 3_600_000);
        assert_eq!(closed, vec!["waiting".to_string()]);
        let g = hub.snapshot("waiting").unwrap().1;
        assert_eq!(g.status, SessionStatus::Ended);
        assert_eq!(g.updated_ms, 2_000, "dated at its last activity, not now");
        assert!(
            hub.close_quiet(3_700_000, 3_600_000).iter().all(|s| s != "waiting"),
            "closed once"
        );

        // Claude picks it up again: open.
        let mut tool = hook("waiting", "PreToolUse");
        tool.tool_name = Some("Read".into());
        tool.tool_use_id = Some("t1".into());
        hub.ingest(tool, 3_800_000, false);
        assert_eq!(hub.snapshot("waiting").unwrap().1.status, SessionStatus::Running);
    }

    #[test]
    fn only_watched_working_sessions_count_as_use() {
        let hub = Hub::new(None);
        hub.ingest(hook("s", "UserPromptSubmit"), 1_000, false);
        assert!(!hub.any_working(), "no known process");
        hub.procs().record("s", Some(42));
        assert!(hub.any_working());
        hub.ingest(hook("s", "Stop"), 2_000, false);
        assert!(!hub.any_working(), "waiting for you isn't use");
    }

    #[test]
    fn a_dead_process_closes_its_session() {
        let hub = Hub::new(None);
        hub.ingest(hook("s", "SessionStart"), 1_000, false);
        hub.procs().record("s", Some(42));
        hub.process_exited("s");
        assert_eq!(hub.snapshot("s").unwrap().1.status, SessionStatus::Ended);
        assert_eq!(hub.procs().pid("s"), None);
    }
}
