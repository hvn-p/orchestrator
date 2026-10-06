//! The coordinator: a fresh Claude Code session started for each batch of
//! events that need judgment, never resumed, never two at a time. See
//! docs/design.md, "The coordinator".
//!
//! Its runtime state lives in `<runtime>/coordinator/`. `holder.json` names
//! the coordinator running now by its pid and start time: `watch`'s runs,
//! `orchestrator setup` and `orchestrator coordinator` all take it before
//! starting one, and it frees itself when that process ends. `pending.json`
//! holds the events waiting for a coordinator, duplicates merged. Both change
//! only under the file lock `lock`. What the coordinator remembers between
//! runs lives in its own directory under the state directory, its working
//! directory: its journal.

pub mod run;
pub mod service;

use crate::admission::{self, Waiting};
use crate::events::{self, Event};
use crate::{procfs, runtime, state};
use anyhow::{Context, Result};
use inotify::{Inotify, WatchMask};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, PidfdFlags, pidfd_open};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The longest the lock is waited for: it is held for a few reads and writes,
/// and for starting a process.
const LOCK_PATIENCE: Duration = Duration::from_secs(5);
/// Between two looks at the queue or the holder when nothing reports a
/// change.
const RECHECK: Duration = Duration::from_secs(30);
/// Room for a batch of inotify events; one takes 16 bytes plus its name.
const EVENT_BUFFER: usize = 4096;

/// Where the coordinator's files are.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `<runtime>/coordinator`: lock, holder, queue, role, runs.
    pub runtime: PathBuf,
    /// `<state>/coordinator`: its working directory and journal.
    pub home: PathBuf,
    /// The user's priorities, next to the configuration.
    pub priorities: PathBuf,
}

impl Paths {
    pub fn new(runtime: &Path, state: &Path, config: &Path) -> Paths {
        Paths {
            runtime: runtime.join("coordinator"),
            home: state.join("coordinator"),
            priorities: crate::config::priorities_path(config),
        }
    }

    /// From the default runtime, state and configuration places.
    pub fn from_env() -> Result<Paths> {
        Ok(Paths::new(
            &runtime::default_dir()?,
            &state::default_dir()?,
            &crate::config::default_path()?,
        ))
    }

    pub fn journal(&self) -> PathBuf {
        self.home.join("journal.md")
    }

    pub fn role(&self) -> PathBuf {
        self.runtime.join("role.md")
    }

    /// One line per run `watch` started.
    pub fn runs(&self) -> PathBuf {
        self.runtime.join("runs.jsonl")
    }

    /// What the latest run printed.
    pub fn last_run(&self) -> PathBuf {
        self.runtime.join("last-run.json")
    }

    /// What the latest run wrote to its standard error.
    pub fn last_errors(&self) -> PathBuf {
        self.runtime.join("last-run.err")
    }

    fn holder(&self) -> PathBuf {
        self.runtime.join("holder.json")
    }

    fn pending(&self) -> PathBuf {
        self.runtime.join("pending.json")
    }

    /// Takes the lock under which the holder and the queue change.
    pub fn lock(&self) -> Result<Locked<'_>> {
        fs::create_dir_all(&self.runtime)
            .with_context(|| format!("creating {}", self.runtime.display()))?;
        let file = admission::lock(&self.runtime.join("lock"), LOCK_PATIENCE)?;
        Ok(Locked {
            paths: self,
            _file: file,
        })
    }
}

/// The process running a coordinator: a pid and its start time, so that a
/// reused pid is not taken for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    pub start_time: u64,
}

impl Holder {
    /// The process `pid`, None once it is gone.
    pub fn of(proc_root: &Path, pid: u32) -> Option<Holder> {
        procfs::start_time(proc_root, pid).map(|start_time| Holder { pid, start_time })
    }

    /// It still runs.
    pub fn running(&self, proc_root: &Path) -> bool {
        procfs::running(proc_root, self.pid, self.start_time)
    }
}

/// The coordinator's lock, held. The holder and the queue are read and
/// written through it.
pub struct Locked<'a> {
    paths: &'a Paths,
    _file: File,
}

impl Locked<'_> {
    /// The process that last took the coordinator, running or not.
    pub fn holder(&self) -> Option<Holder> {
        serde_json::from_slice(&fs::read(self.paths.holder()).ok()?).ok()
    }

    pub fn set_holder(&self, holder: Holder) -> Result<()> {
        write_json(&self.paths.holder(), &holder)
    }

    /// Frees the coordinator if `holder` still holds it.
    pub fn clear_holder(&self, holder: Holder) -> Result<()> {
        if self.holder() == Some(holder) {
            match fs::remove_file(self.paths.holder()) {
                Err(e) if e.kind() != ErrorKind::NotFound => {
                    return Err(e).context("removing the coordinator's holder");
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The events waiting for a coordinator, oldest first.
    pub fn pending(&self) -> Vec<Pending> {
        fs::read(self.paths.pending())
            .ok()
            .and_then(|json| serde_json::from_slice(&json).ok())
            .unwrap_or_default()
    }

    pub fn set_pending(&self, queue: &[Pending]) -> Result<()> {
        write_json(&self.paths.pending(), &queue)
    }
}

/// Replaces `path` at once, so that a reader never sees half a file.
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let json = serde_json::to_vec(value).context("serializing")?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        let _ = fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })
}

/// An event waiting for a coordinator. Events of the same key merge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub key: String,
    /// How many events merged into this one.
    pub count: u32,
    /// When the first of them came, in seconds since the Unix epoch.
    pub first_at: u64,
    /// The latest of them, as the events file holds it.
    pub event: Value,
}

impl Pending {
    /// The event `watch` wrote at `at`, if it needs judgment. Memory pressure
    /// events all merge, the latest one standing for the others; an
    /// admission wait is reported once per call. Orphans are not the
    /// coordinator's yet.
    pub fn new(at: u64, event: &Event) -> Result<Option<Pending>> {
        let key = match event {
            Event::MemoryPressure { .. } => "memory_pressure".to_string(),
            Event::AdmissionWait { job, .. } => format!("admission_wait {job}"),
            Event::Orphans { .. } => return Ok(None),
        };
        Ok(Some(Pending {
            key,
            count: 1,
            first_at: at,
            event: events::to_value(at, event)?,
        }))
    }

    /// The call an admission wait is about.
    fn waiting_job(&self) -> Option<&str> {
        (self.event["kind"] == "admission_wait")
            .then(|| self.event["job"].as_str())
            .flatten()
    }

    /// What the coordinator reads: the event, with how many merged into it
    /// and when the first came.
    pub fn line(&self) -> String {
        let mut event = self.event.clone();
        if let Value::Object(fields) = &mut event {
            fields.insert("count".into(), self.count.into());
            fields.insert("first_at".into(), self.first_at.into());
        }
        event.to_string()
    }
}

/// Adds `new` to `queue`: an event of the same key replaces the one there,
/// which keeps its place.
pub fn merge(queue: &mut Vec<Pending>, new: Pending) {
    match queue.iter_mut().find(|p| p.key == new.key) {
        Some(p) => {
            p.count = p.count.saturating_add(new.count);
            p.first_at = p.first_at.min(new.first_at);
            p.event = new.event;
        }
        None => queue.push(new),
    }
}

/// The events that still need judgment, given the jobs of the calls waiting
/// now: an admission wait whose call runs already does not.
pub fn due(queue: Vec<Pending>, waiting: &[String]) -> Vec<Pending> {
    queue
        .into_iter()
        .filter(|p| {
            p.waiting_job()
                .is_none_or(|job| waiting.iter().any(|w| w == job))
        })
        .collect()
}

/// Adds `pending` to the queue.
pub fn enqueue(paths: &Paths, pending: Pending) -> Result<()> {
    let locked = paths.lock()?;
    let mut queue = locked.pending();
    merge(&mut queue, pending);
    locked.set_pending(&queue)
}

/// Takes the queue's events that still need judgment, emptying it.
pub fn take_due(locked: &Locked<'_>, admission: &admission::Paths) -> Result<Vec<Pending>> {
    let queue = locked.pending();
    if queue.is_empty() {
        return Ok(queue);
    }
    let waiting: Vec<String> = admission::waiting(admission, false)
        .into_iter()
        .map(|w| w.job)
        .collect();
    locked.set_pending(&[])?;
    Ok(due(queue, &waiting))
}

/// Which calls have waited long enough to wake the coordinator. Each is
/// reported once.
#[derive(Debug, Default)]
pub struct Waits {
    reported: HashSet<String>,
}

impl Waits {
    /// Of the calls `waiting` now, those that have waited `after_ms` by
    /// `now_ms` and were not reported yet, and when the next one will have,
    /// in ms since the Unix epoch.
    pub fn due(
        &mut self,
        waiting: &[Waiting],
        now_ms: u64,
        after_ms: u64,
    ) -> (Vec<Waiting>, Option<u64>) {
        self.reported
            .retain(|job| waiting.iter().any(|w| &w.job == job));
        let mut due = Vec::new();
        let mut next: Option<u64> = None;
        for w in waiting {
            if self.reported.contains(&w.job) {
                continue;
            }
            let at = w.since_ms.saturating_add(after_ms);
            if at <= now_ms {
                self.reported.insert(w.job.clone());
                due.push(w.clone());
            } else {
                next = Some(next.map_or(at, |n| n.min(at)));
            }
        }
        (due, next)
    }
}

/// Takes the coordinator for the process `me`, waiting for a running one to
/// end. `proc_root` tells which processes run.
pub fn acquire(paths: &Paths, proc_root: &Path, me: Holder) -> Result<()> {
    let mut told = false;
    loop {
        let locked = paths.lock()?;
        let other = locked.holder().filter(|h| *h != me && h.running(proc_root));
        let Some(other) = other else {
            return locked.set_holder(me);
        };
        drop(locked);
        if !told {
            eprintln!(
                "orchestrator: a coordinator is running (pid {}); waiting for it to end",
                other.pid
            );
            told = true;
        }
        wait_exit(other.pid, RECHECK);
    }
}

/// Sleeps until process `pid` exits, or `timeout` runs out.
fn wait_exit(pid: u32, timeout: Duration) {
    let fd = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .and_then(|p| pidfd_open(p, PidfdFlags::empty()).ok());
    // Gone already: nothing to wait for.
    let Some(fd) = fd else { return };
    poll_one(&fd, timeout);
}

/// Sleeps until `fd` is readable, or `timeout` runs out.
fn poll_one(fd: &impl AsFd, timeout: Duration) {
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    let timeout = Timespec::try_from(timeout).unwrap_or(Timespec {
        tv_sec: i64::MAX,
        tv_nsec: 0,
    });
    let _ = poll(&mut fds, Some(&timeout));
}

/// The events pending for an interactive coordinator, once some are due:
/// waits until there are. They leave the queue.
pub fn next(paths: &Paths, admission: &admission::Paths) -> Result<Vec<Pending>> {
    fs::create_dir_all(&paths.runtime)
        .with_context(|| format!("creating {}", paths.runtime.display()))?;
    let inotify = Inotify::init().context("watching the coordinator's queue")?;
    // The queue is replaced through a rename.
    inotify
        .watches()
        .add(&paths.runtime, WatchMask::MOVED_TO | WatchMask::ONLYDIR)
        .with_context(|| format!("watching {}", paths.runtime.display()))?;
    let mut inotify = inotify;
    let mut buffer = [0; EVENT_BUFFER];
    loop {
        let due = take_due(&paths.lock()?, admission)?;
        if !due.is_empty() {
            return Ok(due);
        }
        poll_one(&inotify, RECHECK);
        while inotify
            .read_events(&mut buffer)
            .is_ok_and(|mut e| e.next().is_some())
        {}
    }
}

/// An open pidfd on `pid`, which becomes readable when it exits.
fn pidfd(pid: u32) -> std::io::Result<OwnedFd> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| std::io::Error::from(ErrorKind::InvalidInput))?;
    pidfd_open(pid, PidfdFlags::empty()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::SessionBrief;

    fn pressure(available_mb: u64) -> Event {
        Event::MemoryPressure {
            available_mb,
            stall_ms: 200,
            largest: None,
            next: vec![SessionBrief {
                session: "alpha".into(),
                rss_mb: 4000,
            }],
        }
    }

    fn wait(job: &str) -> Event {
        Event::AdmissionWait {
            session: Some("alpha".into()),
            session_id: Some("a".into()),
            job: job.into(),
            command: "make".into(),
            waited_secs: 20,
            peak_mb: 3000,
            need_mb: 5000,
            free_mb: 1000,
        }
    }

    fn pending(at: u64, event: &Event) -> Pending {
        Pending::new(at, event).unwrap().unwrap()
    }

    #[test]
    fn duplicates_merge_into_the_latest() {
        let mut queue = Vec::new();
        merge(&mut queue, pending(10, &pressure(900)));
        merge(&mut queue, pending(11, &wait("job-bash-1-1")));
        merge(&mut queue, pending(70, &pressure(500)));
        merge(&mut queue, pending(71, &wait("job-bash-2-1")));
        assert_eq!(queue.len(), 3);
        assert_eq!(
            (
                queue[0].count,
                queue[0].first_at,
                &queue[0].event["available_mb"]
            ),
            (2, 10, &Value::from(500))
        );
        assert_eq!(queue[0].event["at"], 70);
        assert_eq!(queue[1].key, "admission_wait job-bash-1-1");
        assert_eq!(queue[2].key, "admission_wait job-bash-2-1");
        let line: Value = serde_json::from_str(&queue[0].line()).unwrap();
        assert_eq!(line["kind"], "memory_pressure");
        assert_eq!((&line["count"], &line["first_at"]), (&2.into(), &10.into()));
    }

    #[test]
    fn orphans_are_not_queued() {
        let orphans = Event::Orphans { orphans: vec![] };
        assert_eq!(Pending::new(1, &orphans).unwrap(), None);
    }

    #[test]
    fn a_wait_that_ended_needs_no_judgment() {
        let queue = vec![
            pending(1, &pressure(900)),
            pending(2, &wait("job-bash-1-1")),
            pending(3, &wait("job-bash-2-1")),
        ];
        let kept = due(queue, &["job-bash-2-1".to_string()]);
        let keys: Vec<&str> = kept.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["memory_pressure", "admission_wait job-bash-2-1"]);
    }

    fn waiting(job: &str, since_ms: u64) -> Waiting {
        Waiting {
            job: job.into(),
            group: format!("/s.scope/{job}"),
            label: "make".into(),
            peak_mb: 3000,
            need_mb: 5000,
            since_ms,
        }
    }

    #[test]
    fn a_long_wait_is_reported_once() {
        let mut waits = Waits::default();
        let now = [waiting("a", 1_000), waiting("b", 5_000)];
        let (due, next) = waits.due(&now, 10_000, 20_000);
        assert_eq!(due, []);
        assert_eq!(next, Some(21_000));
        let (due, next) = waits.due(&now, 21_000, 20_000);
        assert_eq!(due, [waiting("a", 1_000)]);
        assert_eq!(next, Some(25_000));
        let (due, _) = waits.due(&now, 22_000, 20_000);
        assert_eq!(due, [], "reported once");
        let (due, next) = waits.due(&now, 30_000, 20_000);
        assert_eq!(due, [waiting("b", 5_000)]);
        assert_eq!(next, None);
        // A call that stopped waiting is forgotten.
        let (_, _) = waits.due(&[], 31_000, 20_000);
        assert_eq!(waits.reported.len(), 0);
    }

    fn temp_paths() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(
            &tmp.path().join("run"),
            &tmp.path().join("state"),
            &tmp.path().join("config/config.json"),
        );
        (tmp, paths)
    }

    #[test]
    fn the_queue_survives_on_disk_and_empties_when_taken() {
        let (tmp, paths) = temp_paths();
        let admission = admission::Paths {
            cgroup_root: tmp.path().join("cgroup"),
            meminfo: tmp.path().join("meminfo"),
            runtime: tmp.path().join("run"),
        };
        enqueue(&paths, pending(1, &pressure(900))).unwrap();
        enqueue(&paths, pending(2, &wait("job-bash-1-1"))).unwrap();
        enqueue(&paths, pending(3, &pressure(800))).unwrap();
        let locked = paths.lock().unwrap();
        assert_eq!(locked.pending().len(), 2);
        // No call waits: only the pressure is still due.
        let taken = take_due(&locked, &admission).unwrap();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].count, 2);
        assert_eq!(locked.pending(), []);
    }

    #[test]
    fn one_holder_at_a_time() {
        let (_tmp, paths) = temp_paths();
        let proc_root = Path::new("/proc");
        let me = Holder::of(proc_root, std::process::id()).unwrap();
        assert!(me.running(proc_root));
        let locked = paths.lock().unwrap();
        assert_eq!(locked.holder(), None);
        locked.set_holder(me).unwrap();
        assert_eq!(locked.holder(), Some(me));
        let other = Holder {
            pid: me.pid,
            start_time: me.start_time + 1,
        };
        assert!(!other.running(proc_root), "a reused pid is not the holder");
        locked.clear_holder(other).unwrap();
        assert_eq!(locked.holder(), Some(me), "only the holder frees it");
        locked.clear_holder(me).unwrap();
        assert_eq!(locked.holder(), None);
        drop(locked);
        // A holder that ended frees the coordinator: taking it does not wait.
        paths.lock().unwrap().set_holder(other).unwrap();
        acquire(&paths, proc_root, me).unwrap();
        assert_eq!(paths.lock().unwrap().holder(), Some(me));
    }

    #[test]
    fn next_waits_for_an_event() {
        let (tmp, paths) = temp_paths();
        let admission = admission::Paths {
            cgroup_root: tmp.path().join("cgroup"),
            meminfo: tmp.path().join("meminfo"),
            runtime: tmp.path().join("run"),
        };
        fs::create_dir_all(&paths.runtime).unwrap();
        let writer = {
            let paths = paths.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                enqueue(&paths, pending(5, &pressure(700))).unwrap();
            })
        };
        let start = std::time::Instant::now();
        let got = next(&paths, &admission).unwrap();
        writer.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].event["available_mb"], 700);
        assert_eq!(paths.lock().unwrap().pending(), []);
    }
}
