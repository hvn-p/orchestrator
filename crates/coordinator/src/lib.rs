//! The coordinator: a fresh Claude Code session started for each batch of
//! events that need judgment, never resumed, never two at a time.
//!
//! Its runtime state lives in `<runtime>/coordinator/`. `holder.json` names
//! the coordinator running now by its pid and start time: `watch`'s runs,
//! the setup conversation and `orchestrator coordinator` all take it before
//! starting one, and it frees itself when that process ends. `queue.json`
//! holds the events for coordinators and where each stands (see `queue`).
//! Both change only under the file lock `lock`. What the coordinator
//! remembers between runs lives in its own directory under the state
//! directory, its working directory: its journal.
//!
//! What a coordinator must remember lives in files, rather than in a
//! conversation resumed at each event, whose context would grow with every
//! event and which a user reopening it would race with `watch`. A permanent
//! background session (`claude --bg`) works, but holds 0.5 to 0.7 GB of
//! memory all the time, on a tool meant to spare it. A coordinator
//! authorising every command would be a bottleneck, cost tokens per
//! request, and hold everything back while busy. It talks to sessions
//! through Claude Code's messages between sessions, which are enough: no
//! transport of its own, and no agent team, whose members the lead starts
//! rather than the user opening them in their own worktrees.
//!
//! The setup conversation writes the configuration, which by hand is
//! tedious and needs exact values: a coordinator explains, takes answers in
//! plain words and proposes values the machine supports. `orchestrator
//! coordinator` opens an interactive coordinator, which holds the
//! coordinator and receives the events itself, through `orchestrator
//! coordinator next` run in the background, for as long as it stays open.
//!
//! A run ends with its reply, so a session cannot answer it: the next run
//! checks the effect in the state instead, and only an interactive
//! coordinator gets answers. What a coordinator may run rests on Claude
//! Code's permission rules, not on a sandbox, and organization-level
//! instructions reach it too.

pub mod instructions;
pub mod journal;
pub mod queue;
pub mod run;
pub mod service;
pub mod state;

use anyhow::{Context, Result};
use inotify::{Inotify, WatchMask};
use prefix::admission::{self, Waiting};
use queue::Queued;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, PidfdFlags, pidfd_open};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;
use system::procfs;
use system::runtime;

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
    /// The user's instructions for coordinators, next to the configuration.
    pub instructions: PathBuf,
}

impl Paths {
    pub fn new(runtime: &Path, state: &Path, config: &Path) -> Paths {
        Paths {
            runtime: runtime.join("coordinator"),
            home: state.join("coordinator"),
            instructions: config::instructions_path(config),
        }
    }

    /// From the default runtime, state and configuration places.
    pub fn from_env() -> Result<Paths> {
        Ok(Paths::new(
            &runtime::default_dir()?,
            &system::state::default_dir()?,
            &config::default_path()?,
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

    fn queue(&self) -> PathBuf {
        self.runtime.join("queue.json")
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

    /// The queued events, oldest first.
    pub fn queue(&self) -> Vec<Queued> {
        fs::read(self.paths.queue())
            .ok()
            .and_then(|json| serde_json::from_slice(&json).ok())
            .unwrap_or_default()
    }

    /// Changes the queue with `change`, dropping old done events.
    pub fn update<T>(&self, change: impl FnOnce(&mut Vec<Queued>) -> T) -> Result<T> {
        let mut queue = self.queue();
        let out = change(&mut queue);
        queue::prune(&mut queue);
        write_json(&self.paths.queue(), &queue)?;
        Ok(out)
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

/// Adds `queued` to the queue.
pub fn enqueue(paths: &Paths, queued: Queued) -> Result<()> {
    paths.lock()?.update(|q| queue::merge(q, queued))
}

/// The jobs of the calls waiting for memory now.
pub fn waiting_jobs(admission: &admission::Paths) -> Vec<String> {
    admission::waiting(admission, false)
        .into_iter()
        .map(|w| w.job)
        .collect()
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
/// end. `proc_root` tells which processes run. Events a previous holder left
/// in progress are pending again.
pub fn acquire(paths: &Paths, proc_root: &Path, me: Holder) -> Result<()> {
    let mut told = false;
    loop {
        let locked = paths.lock()?;
        let other = locked.holder().filter(|h| *h != me && h.running(proc_root));
        let Some(other) = other else {
            locked.update(|q| queue::give_back(q))?;
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

/// For an interactive coordinator: the batch it took before is handled, and
/// the next one is the pending events, waited for until there are some. They
/// are in progress from then on.
pub fn next(paths: &Paths, admission: &admission::Paths) -> Result<Vec<Queued>> {
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
    paths.lock()?.update(|q| queue::finish(q, true))?;
    loop {
        let waiting = waiting_jobs(admission);
        let taken = paths.lock()?.update(|q| queue::take(q, &waiting))?;
        if !taken.is_empty() {
            return Ok(taken);
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
    use queue::Status;
    use watch::events::Event;

    fn pressure(available_mb: u64) -> Event {
        Event::MemoryPressure(watch::events::MemoryPressure {
            available_mb,
            stall_ms: 200,
            largest: None,
            next: vec![],
        })
    }

    fn wait(job: &str) -> Event {
        Event::AdmissionWait(watch::events::AdmissionWait {
            session: Some("alpha".into()),
            session_id: Some("a".into()),
            job: job.into(),
            command: "make".into(),
            waited_secs: 20,
            peak_mb: 3000,
            need_mb: 5000,
            free_mb: 1000,
        })
    }

    fn queued(at: u64, event: &Event) -> Queued {
        Queued::new(at, event).unwrap().unwrap()
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

    fn temp_paths() -> (tempfile::TempDir, Paths, admission::Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(
            &tmp.path().join("run"),
            &tmp.path().join("state"),
            &tmp.path().join("config/config.json"),
        );
        let admission = admission::Paths {
            cgroup_root: tmp.path().join("cgroup"),
            meminfo: tmp.path().join("meminfo"),
            runtime: tmp.path().join("run"),
        };
        (tmp, paths, admission)
    }

    fn statuses(paths: &Paths) -> Vec<Status> {
        paths
            .lock()
            .unwrap()
            .queue()
            .iter()
            .map(|q| q.status)
            .collect()
    }

    #[test]
    fn the_queue_survives_on_disk() {
        let (_tmp, paths, _) = temp_paths();
        enqueue(&paths, queued(1, &pressure(900))).unwrap();
        enqueue(&paths, queued(2, &wait("job-bash-1-1"))).unwrap();
        enqueue(&paths, queued(3, &pressure(800))).unwrap();
        let queue = paths.lock().unwrap().queue();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].count, 2);
        assert_eq!(statuses(&paths), [Status::Pending, Status::Pending]);
    }

    #[test]
    fn one_holder_at_a_time() {
        let (_tmp, paths, _) = temp_paths();
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
        // A holder that ended frees the coordinator, and gives its events
        // back: taking it does not wait.
        enqueue(&paths, queued(1, &pressure(900))).unwrap();
        let locked = paths.lock().unwrap();
        locked.set_holder(other).unwrap();
        locked.update(|q| queue::take(q, &[])).unwrap();
        drop(locked);
        assert_eq!(statuses(&paths), [Status::InProgress]);
        acquire(&paths, proc_root, me).unwrap();
        assert_eq!(paths.lock().unwrap().holder(), Some(me));
        assert_eq!(statuses(&paths), [Status::Pending]);
    }

    #[test]
    fn next_waits_for_events_and_closes_the_batch_before() {
        let (_tmp, paths, admission) = temp_paths();
        fs::create_dir_all(&paths.runtime).unwrap();
        let writer = {
            let paths = paths.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                enqueue(&paths, queued(5, &pressure(700))).unwrap();
            })
        };
        let start = std::time::Instant::now();
        let got = next(&paths, &admission).unwrap();
        writer.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].event["available_mb"], 700);
        assert_eq!(statuses(&paths), [Status::InProgress]);
        // Asking for the next batch says this one was handled.
        enqueue(&paths, queued(6, &pressure(600))).unwrap();
        let got = next(&paths, &admission).unwrap();
        assert_eq!(got[0].event["available_mb"], 600);
        assert_eq!(statuses(&paths), [Status::Done, Status::InProgress]);
    }
}
