//! CPU and memory per session.
//!
//! Hooks don't say which process sent them, so on a session's first hook
//! hooktop looks up the process on the other end of the hook's TCP
//! connection: that's Claude Code. A sampler thread then reads the CPU and
//! resident memory of that process and everything it started (the shells,
//! test runners and builds tools launch) once a second while the session
//! is working, and records each reading as a [`Sample`] event.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, Weak};
use std::time::{Duration, Instant};

use hooktop_core::Sample;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::hub::Hub;
use crate::now_ms;

/// How many hooks to try a lookup on before giving up on a session (a
/// replay, `curl`, or a sender hooktop can't see).
const MAX_TRIES: u8 = 3;
const EVERY: Duration = Duration::from_secs(1);
/// Check that idle sessions' processes are still alive every this many ticks.
const LIVENESS_EVERY: u64 = 5;

#[derive(Default)]
pub struct Procs {
    inner: Mutex<HashMap<String, Tracked>>,
}

#[derive(Default)]
struct Tracked {
    pid: Option<u32>,
    tries: u8,
}

impl Procs {
    /// Should the next hook from this session try to find its process?
    pub fn needs_lookup(&self, session: &str) -> bool {
        let map = self.inner.lock().unwrap();
        map.get(session).is_none_or(|t| t.pid.is_none() && t.tries < MAX_TRIES)
    }

    pub fn record(&self, session: &str, pid: Option<u32>) {
        let mut map = self.inner.lock().unwrap();
        let t = map.entry(session.to_string()).or_default();
        t.tries = t.tries.saturating_add(1);
        if pid.is_some() {
            t.pid = pid;
        }
    }

    pub fn pid(&self, session: &str) -> Option<u32> {
        self.inner.lock().unwrap().get(session).and_then(|t| t.pid)
    }

    /// The process is gone: stop sampling, and allow a fresh lookup if the
    /// session comes back (`claude --resume`).
    pub fn forget(&self, session: &str) {
        self.inner.lock().unwrap().remove(session);
    }

    fn tracked(&self) -> Vec<(String, u32)> {
        let map = self.inner.lock().unwrap();
        map.iter().filter_map(|(s, t)| t.pid.map(|p| (s.clone(), p))).collect()
    }
}

/// The process that owns the client end of a connection from `peer`.
pub fn pid_for_peer(peer: SocketAddr) -> Option<u32> {
    if !peer.ip().is_loopback() {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        linux::pid_for_local_port(peer.port())
    }
    #[cfg(target_os = "macos")]
    {
        lsof_pid_for_local_port(peer.port())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;

    /// Find the socket inode in /proc/net/tcp{,6} whose local port is
    /// `port`, then the process holding a file descriptor to it.
    pub fn pid_for_local_port(port: u16) -> Option<u32> {
        let mut targets = Vec::new();
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let Ok(text) = fs::read_to_string(table) else { continue };
            for line in text.lines().skip(1) {
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.len() < 10 || cols[3] != "01" {
                    continue; // 01 = ESTABLISHED
                }
                let local = cols[1].rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok());
                if local == Some(port) {
                    targets.push(format!("socket:[{}]", cols[9]));
                }
            }
        }
        if targets.is_empty() {
            return None;
        }
        for entry in fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                if let Ok(link) = fs::read_link(fd.path())
                    && targets.iter().any(|t| link.as_os_str() == t.as_str())
                {
                    return Some(pid);
                }
            }
        }
        None
    }
}

#[cfg(target_os = "macos")]
fn lsof_pid_for_local_port(port: u16) -> Option<u32> {
    // Both ends of the connection match the port; ours is the server end.
    let me = std::process::id();
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:ESTABLISHED"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .find(|pid| *pid != me)
}

/// Sample every tracked, working session once a second until the hub goes
/// away.
pub fn spawn_sampler(hub: Weak<Hub>) {
    let spawned = std::thread::Builder::new()
        .name("hooktop-sampler".into())
        .spawn(move || {
            if !sysinfo::IS_SUPPORTED_SYSTEM {
                tracing::warn!("can't read process information here; CPU and RAM won't be recorded");
                return;
            }
            let mut sys = System::new();
            let kind = ProcessRefreshKind::nothing().with_cpu().with_memory().without_tasks();
            let mut cpu = CpuMeter::default();
            let mut tick: u64 = 0;
            loop {
                std::thread::sleep(EVERY);
                tick += 1;
                let Some(hub) = hub.upgrade() else { return };
                // Every few seconds, close sessions whose Claude Code has exited
                // without a SessionEnd (a killed terminal, a crash).
                if tick.is_multiple_of(LIVENESS_EVERY) {
                    let tracked = hub.procs().tracked();
                    if !tracked.is_empty() {
                        let pids: Vec<Pid> = tracked.iter().map(|(_, p)| Pid::from_u32(*p)).collect();
                        sys.refresh_processes_specifics(
                            ProcessesToUpdate::Some(&pids),
                            true,
                            ProcessRefreshKind::nothing(),
                        );
                        for (session, pid) in tracked {
                            if sys.process(Pid::from_u32(pid)).is_none() {
                                hub.process_exited(&session);
                            }
                        }
                    }
                }
                let targets: Vec<(String, u32)> = hub
                    .procs()
                    .tracked()
                    .into_iter()
                    .filter(|(s, _)| hub.is_working(s))
                    .collect();
                if targets.is_empty() {
                    continue;
                }
                sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
                let usage = cpu.measure(&sys, Instant::now());
                let children = children_of(&sys);
                for (session, pid) in targets {
                    match sample_tree(&sys, &children, &usage, Pid::from_u32(pid)) {
                        Some(s) => {
                            hub.ingest(s.to_payload(&session), s.at_ms, true);
                        }
                        None => hub.process_exited(&session),
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("couldn't start the CPU/RAM sampler: {e}");
    }
}

fn children_of(sys: &System) -> HashMap<Pid, Vec<Pid>> {
    let mut map: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            map.entry(parent).or_default().push(*pid);
        }
    }
    map
}

/// Per-process CPU, measured by hooktop itself from each process's CPU
/// time between two readings.
///
/// sysinfo's own percentage can spike far past what the machine has (a
/// process seen for the first time, or after a pause in sampling, reported
/// 4000% on a 10-core Mac). Here a process counts only from its second
/// reading, and never above the number of cores.
#[derive(Default)]
struct CpuMeter {
    /// CPU time (ms), when it was read, and the process's start time (a
    /// reused pid is a new process).
    last: HashMap<Pid, (u64, Instant, u64)>,
}

impl CpuMeter {
    /// CPU % per process since the last call (100 = one core busy).
    fn measure(&mut self, sys: &System, now: Instant) -> HashMap<Pid, f32> {
        let max = 100.0 * cores() as f32;
        let mut usage = HashMap::new();
        let mut next = HashMap::with_capacity(sys.processes().len());
        for (pid, p) in sys.processes() {
            let (cpu_ms, started) = (p.accumulated_cpu_time(), p.start_time());
            if let Some(&(was, at, s)) = self.last.get(pid)
                && s == started
            {
                let wall = now.duration_since(at).as_secs_f32() * 1000.0;
                if wall >= 1.0 {
                    usage.insert(*pid, (cpu_ms.saturating_sub(was) as f32 / wall * 100.0).min(max));
                }
            }
            next.insert(*pid, (cpu_ms, now, started));
        }
        self.last = next;
        usage
    }
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// CPU and memory of `root` and all its descendants; `None` once it exited.
fn sample_tree(
    sys: &System,
    children: &HashMap<Pid, Vec<Pid>>,
    usage: &HashMap<Pid, f32>,
    root: Pid,
) -> Option<Sample> {
    sys.processes().get(&root)?;
    let (mut cpu, mut rss, mut procs) = (0f32, 0u64, 0u32);
    let mut top: Option<(f32, String)> = None;
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        let Some(p) = sys.processes().get(&pid) else { continue };
        let p_cpu = usage.get(&pid).copied().unwrap_or(0.0);
        cpu += p_cpu;
        rss += p.memory();
        procs += 1;
        if pid != root && p_cpu > top.as_ref().map_or(1.0, |t| t.0) {
            top = Some((p_cpu, p.name().to_string_lossy().into_owned()));
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    Some(Sample {
        at_ms: now_ms(),
        // Many processes at once still can't use more than the whole machine.
        cpu: cpu.min(100.0 * cores() as f32),
        rss,
        procs,
        top: top.map(|t| t.1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_lookups_and_gives_up() {
        let p = Procs::default();
        assert!(p.needs_lookup("s"));
        for _ in 0..MAX_TRIES {
            p.record("s", None);
        }
        assert!(!p.needs_lookup("s"));
        p.record("t", Some(42));
        assert_eq!(p.pid("t"), Some(42));
        assert!(!p.needs_lookup("t"));
        p.forget("t");
        assert!(p.needs_lookup("t"));
    }

    #[test]
    fn samples_our_own_process_tree() {
        let mut sys = System::new();
        let kind = ProcessRefreshKind::nothing().with_cpu().with_memory().without_tasks();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let me = Pid::from_u32(std::process::id());
        let usage = HashMap::new();
        let s = sample_tree(&sys, &children_of(&sys), &usage, me).expect("we exist");
        assert!(s.rss > 0 && s.procs >= 1, "{s:?}");
        assert!(sample_tree(&sys, &children_of(&sys), &usage, Pid::from_u32(u32::MAX - 7)).is_none());
    }

    #[test]
    fn cpu_counts_from_the_second_reading_and_stays_within_the_machine() {
        let mut sys = System::new();
        let kind = ProcessRefreshKind::nothing().with_cpu().with_memory().without_tasks();
        let me = Pid::from_u32(std::process::id());
        let mut meter = CpuMeter::default();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let t0 = Instant::now();
        assert!(meter.measure(&sys, t0).is_empty(), "nothing on the first reading");
        // Burn some CPU, then read again.
        let spin = Instant::now();
        let mut x = 0u64;
        while spin.elapsed() < Duration::from_millis(300) {
            x = x.wrapping_mul(31).wrapping_add(7);
        }
        std::hint::black_box(x);
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let usage = meter.measure(&sys, Instant::now());
        let mine = usage.get(&me).copied().unwrap_or(0.0);
        assert!(mine > 10.0, "we were busy: {mine}");
        assert!(usage.values().all(|v| *v <= 100.0 * cores() as f32));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn finds_the_process_behind_a_connection() {
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_server_side, peer) = listener.accept().unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
        // On macOS the lookup skips our own pid (it can't tell the two ends
        // apart), so only Linux can check the answer here.
        #[cfg(target_os = "linux")]
        assert_eq!(pid_for_peer(peer), Some(std::process::id()));
        #[cfg(target_os = "macos")]
        let _ = pid_for_peer(peer);
    }
}
