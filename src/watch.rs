//! The watch loop: cheap memory check on every tick, full process scan only
//! when memory is low or an orphan scan is due.

use crate::attribution::{self, Attribution};
use crate::events::{self, Event};
use crate::{memory, procfs, sessions};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub mem_min_mb: u64,
    pub cooldown_secs: u64,
}

pub struct Config {
    pub proc_root: PathBuf,
    pub sessions_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub interval: Duration,
    pub orphan_interval: Duration,
    pub thresholds: Thresholds,
}

/// Decides which events to emit. Holds no IO, so its rules are tested alone.
pub struct Watcher {
    thresholds: Thresholds,
    last_pressure: Option<u64>,
    /// Orphan groups already reported, by root (pid, start time).
    reported: HashSet<(u32, u64)>,
}

impl Watcher {
    pub fn new(thresholds: Thresholds) -> Self {
        Watcher {
            thresholds,
            last_pressure: None,
            reported: HashSet::new(),
        }
    }

    /// `scan` runs only when an event is due.
    pub fn check_memory(
        &mut self,
        available_mb: u64,
        now: u64,
        scan: impl FnOnce() -> Result<Attribution>,
    ) -> Result<Option<Event>> {
        let cooled = self
            .last_pressure
            .is_none_or(|t| now.saturating_sub(t) >= self.thresholds.cooldown_secs);
        if available_mb >= self.thresholds.mem_min_mb || !cooled {
            return Ok(None);
        }
        let att = scan()?;
        self.last_pressure = Some(now);
        Ok(Some(Event::memory_pressure(
            available_mb,
            self.thresholds.mem_min_mb,
            &att,
        )))
    }

    /// Reports each orphan group once, for as long as it lives.
    pub fn check_orphans(&mut self, att: &Attribution) -> Option<Event> {
        let current: HashSet<(u32, u64)> = att
            .orphans
            .iter()
            .map(|o| (o.root.pid, o.start_time))
            .collect();
        let new: Vec<_> = att
            .orphans
            .iter()
            .filter(|o| !self.reported.contains(&(o.root.pid, o.start_time)))
            .collect();
        self.reported = current;
        (!new.is_empty()).then(|| Event::orphans(&new))
    }
}

/// Reads processes and sessions, then attributes. A missing sessions
/// directory means no Claude session has run yet.
pub fn scan(proc_root: &Path, sessions_dir: &Path) -> Result<Attribution> {
    let procs = procfs::read_processes(proc_root)
        .with_context(|| format!("reading {}", proc_root.display()))?;
    let sessions = match sessions::read_sessions(sessions_dir) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", sessions_dir.display())),
    };
    Ok(attribution::attribute(&procs, &sessions))
}

pub fn run(cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.runtime_dir)
        .with_context(|| format!("creating {}", cfg.runtime_dir.display()))?;
    let meminfo = cfg.proc_root.join("meminfo");
    // Fail fast on a wrong proc root rather than logging the same error forever.
    memory::available_mb(&meminfo)?;
    let events_path = cfg.runtime_dir.join("events.jsonl");
    let mut watcher = Watcher::new(cfg.thresholds);
    let mut next_orphan_scan = Instant::now();
    loop {
        let orphan_scan_due = Instant::now() >= next_orphan_scan;
        if orphan_scan_due {
            next_orphan_scan = Instant::now() + cfg.orphan_interval;
        }
        if let Err(e) = tick(cfg, &meminfo, &events_path, &mut watcher, orphan_scan_due) {
            // A daemon outlives transient errors: a process vanishing mid-read,
            // a sessions file being rewritten.
            eprintln!("orchestrator: {e:#}");
        }
        std::thread::sleep(cfg.interval);
    }
}

fn tick(
    cfg: &Config,
    meminfo: &Path,
    events_path: &Path,
    watcher: &mut Watcher,
    orphan_scan_due: bool,
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let available = memory::available_mb(meminfo)?;
    if let Some(event) =
        watcher.check_memory(available, now, || scan(&cfg.proc_root, &cfg.sessions_dir))?
    {
        events::append(events_path, now, &event)?;
    }
    if orphan_scan_due {
        let att = scan(&cfg.proc_root, &cfg.sessions_dir)?;
        if let Some(event) = watcher.check_orphans(&att) {
            events::append(events_path, now, &event)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::{Orphan, ProcRef};

    const T: Thresholds = Thresholds {
        mem_min_mb: 3000,
        cooldown_secs: 60,
    };

    fn orphan(pid: u32, start_time: u64) -> Orphan {
        Orphan {
            root: ProcRef {
                pid,
                rss_kb: 1024,
                comm: "node".into(),
                cmdline: "node dev".into(),
            },
            start_time,
            session_id: "dead".into(),
            rss_kb: 1024,
            processes: 1,
        }
    }

    #[test]
    fn memory_pressure_respects_threshold_and_cooldown() {
        let mut w = Watcher::new(T);
        let empty = || Ok(Attribution::default());
        assert!(w.check_memory(5000, 0, empty).unwrap().is_none());
        assert!(w.check_memory(2000, 10, empty).unwrap().is_some());
        assert!(w.check_memory(2000, 69, empty).unwrap().is_none());
        assert!(w.check_memory(2000, 70, empty).unwrap().is_some());
    }

    #[test]
    fn no_scan_when_memory_is_fine() {
        let mut w = Watcher::new(T);
        let event = w
            .check_memory(5000, 0, || anyhow::bail!("scan must not run"))
            .unwrap();
        assert!(event.is_none());
    }

    #[test]
    fn each_orphan_group_is_reported_once() {
        let mut w = Watcher::new(T);
        let one = Attribution {
            sessions: vec![],
            orphans: vec![orphan(10, 1)],
        };
        let two = Attribution {
            sessions: vec![],
            orphans: vec![orphan(10, 1), orphan(20, 2)],
        };
        assert!(w.check_orphans(&one).is_some());
        assert!(w.check_orphans(&one).is_none());
        match w.check_orphans(&two) {
            Some(Event::Orphans { orphans }) => assert_eq!(orphans.len(), 1),
            other => panic!("expected one new orphan, got {other:?}"),
        }
        // The pid comes back with another start time: a new process.
        let reused = Attribution {
            sessions: vec![],
            orphans: vec![orphan(10, 99)],
        };
        assert!(w.check_orphans(&reused).is_some());
    }
}
