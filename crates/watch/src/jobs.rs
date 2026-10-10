//! Finished jobs. Once every process of a job has exited, its group still
//! holds the job's memory peak. The service reads it, keeps it with the
//! command of a Bash call, then removes the group: empty groups would pile
//! up, one per hook call and status line refresh. systemd removes every
//! group of a session when the session ends, so peaks are read while it
//! lives.
//!
//! `Tracker` follows the session tree through inotify: the kernel reports each
//! new session scope, each new job group and each change of a job's
//! `cgroup.events`, so a job is measured as soon as it ends. `collect` sweeps
//! the whole tree instead. It catches up at start and after an event queue
//! overflow, and finds the rare job that ended before its watch was in place.
//!
//! While a job lives, the tracker also holds two signals on it (#24). A PSI
//! trigger on its `memory.pressure` fires when its tasks stall on memory: an
//! unprivileged process may arm one on a job group, with the same 2 s window
//! as on the machine, and it costs a descriptor, no kernel thread (measured:
//! 50 triggers, no `psimon` thread more). A watch on its `memory.events`
//! reports each process killed for lack of memory (`oom_kill`, which counts
//! a kill by any OOM killer, the machine's included); so does one on the
//! `memory.events` of each session's `main/`. The count is read once more
//! just before a group is removed, so a kill right before the end is not
//! lost. A sweep alone holds neither signal: without the tracker, no job
//! stall nor kill is reported.
//!
//! A kill during a Bash call shows in the call's result; one after it, in a
//! process the call left running, shows nowhere. So the tracker also holds a
//! pidfd on the shell of each Bash call, which becomes readable when the
//! call ends: kills counted by then belong to the call, later ones do not.
//! Asking whether the shell still lives when a kill is read instead would
//! misfile a kill right before the call's end whenever `watch` reads it
//! late, which memory pressure makes likely.

use claude_code::invocation::Kind;
use inotify::{EventMask, Events, Inotify, WatchDescriptor, WatchMask};
use learning::peaks::Measurement;
use prefix::{self, JobRecord};
use rustix::process::{Pid, PidfdFlags, pidfd_open};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirEntry};
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;
use system::cgroup;
use system::pressure::Trigger;
use system::runtime;

/// The leaf of a session's claude process, which is not a job.
pub const MAIN: &str = "main";

/// A sweep leaves a job younger than this alone: between creating its group
/// and moving into it, the prefix leaves the group empty for an instant.
const MIN_AGE_MS: u128 = 5_000;

/// Room for a batch of inotify events; one takes 16 bytes plus its name.
const EVENT_BUFFER: usize = 4096;

/// Deletes a job group: `fs::remove_dir` on cgroupfs, where the group's
/// interface files go with it.
pub type Remove = fn(&Path) -> io::Result<()>;

/// Measures and removes the finished jobs of every session in `slice`, then
/// drops the records of sessions that are gone. `records` is the root of the
/// job records.
pub fn collect(
    slice: &Path,
    records: &Path,
    now_ms: u128,
    remove: Remove,
) -> io::Result<Vec<Measurement>> {
    let mut measured = Vec::new();
    let mut live = HashSet::new();
    for scope in entries(slice)? {
        let session = name_of(&scope);
        if !is_scope(&session) {
            continue;
        }
        for job in entries(&scope.path())? {
            let name = name_of(&job);
            measured.extend(finish(
                &job.path(),
                &session,
                &name,
                records,
                now_ms,
                false,
                remove,
            ));
        }
        live.insert(session);
    }
    for dir in entries(records)? {
        if !live.contains(&name_of(&dir)) {
            let _ = fs::remove_dir_all(dir.path());
        }
    }
    Ok(measured)
}

/// Measures and removes the job group `dir` once the job is over: the group is
/// unpopulated, and was either seen becoming so (`ended`) or is old enough.
/// Returns the measurement of a Bash call. A group that cannot be removed stays
/// for a later pass, record included, so a job is measured once.
fn finish(
    dir: &Path,
    session: &str,
    name: &str,
    records: &Path,
    now_ms: u128,
    ended: bool,
    remove: Remove,
) -> Option<Measurement> {
    let (kind, started) = prefix::parse_job_name(name)?;
    let old = now_ms.saturating_sub(started) >= MIN_AGE_MS;
    if !(ended || old) || !unpopulated(dir) {
        return None;
    }
    let peak = read_peak_mb(dir);
    let record_path = records.join(session).join(format!("{name}.json"));
    let record = (kind == Kind::Bash)
        .then(|| read_record(&record_path))
        .flatten();
    remove(dir).ok()?;
    let _ = fs::remove_file(&record_path);
    let (record, peak_mb) = (record?, peak?);
    Some(Measurement {
        at: u64::try_from(now_ms / 1000).unwrap_or(u64::MAX),
        session: session.into(),
        job: name.into(),
        peak_mb,
        command: record.command,
        cwd: record.cwd,
    })
}

/// What a watch descriptor watches.
#[derive(Debug, Clone)]
enum Watched {
    /// The user manager's group, until the orchestrator slice appears in it.
    Manager,
    Slice,
    Scope(String),
    Job {
        session: String,
        name: String,
    },
    /// The `memory.events` of a job, or of a session's `MAIN`.
    Memory {
        session: String,
        job: String,
    },
}

/// What the tracker holds on a live job.
struct Live {
    /// Fires when the job's tasks stall on memory; None when it could not be
    /// armed.
    trigger: Option<Trigger>,
    /// Names the trigger and the shell to the loop.
    id: u64,
    /// `oom_kill` as last read.
    killed: u64,
    /// The pid of a Bash call's shell, until `shell` holds it: the prefix
    /// enters the group a moment after creating it.
    shell_pid: Option<u32>,
    /// A pidfd on a Bash call's shell, until the call ends.
    shell: Option<OwnedFd>,
    /// The watches on its files, removed with it.
    watches: Vec<WatchDescriptor>,
}

/// What the tracker holds on a session scope.
struct Scope {
    /// The `oom_kill` of its `MAIN` as last read, once watched.
    killed: Option<u64>,
    /// The watches on it and on its `MAIN`, removed with it.
    watches: Vec<WatchDescriptor>,
}

/// Processes of a job killed for lack of memory.
#[derive(Debug, PartialEq, Eq)]
pub struct Killed {
    /// The session's scope.
    pub session: String,
    /// The job group, or `MAIN`.
    pub job: String,
    pub killed: u64,
    /// Whether the Bash call that started the job still ran.
    pub call_running: bool,
    /// A Bash call's record, read before its group goes.
    pub record: Option<JobRecord>,
}

/// Follows the session tree through inotify, measures each job as it ends,
/// and holds the signals of each live job.
///
/// cgroupfs reports no `IN_IGNORED` when a group goes (measured), so the
/// tracker removes the watches of a group itself once the group is gone:
/// otherwise each job, a status line refresh included, would leave its
/// watches behind until `watch` exits.
pub struct Tracker {
    inotify: Inotify,
    slice: PathBuf,
    records: PathBuf,
    /// The stall a job's trigger fires on.
    stall: Duration,
    remove: Remove,
    watched: HashMap<WatchDescriptor, Watched>,
    /// By session scope.
    scopes: HashMap<String, Scope>,
    /// By session scope and job name.
    live: HashMap<(String, String), Live>,
    next_id: u64,
    kills: Vec<Killed>,
}

impl Tracker {
    /// Starts watching the tree, and returns what a first sweep measured.
    pub fn new(
        slice: PathBuf,
        records: PathBuf,
        stall: Duration,
        remove: Remove,
    ) -> io::Result<(Tracker, Vec<Measurement>)> {
        let mut tracker = Tracker {
            inotify: Inotify::init()?,
            slice,
            records,
            stall,
            remove,
            watched: HashMap::new(),
            scopes: HashMap::new(),
            live: HashMap::new(),
            next_id: 0,
            kills: Vec::new(),
        };
        let measured = tracker.resync()?;
        Ok((tracker, measured))
    }

    /// Readable when the kernel reported changes.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.inotify.as_fd()
    }

    /// The armed triggers, by id: each reports priority data when its job
    /// stalls.
    pub fn triggers(&self) -> impl Iterator<Item = (u64, BorrowedFd<'_>)> {
        self.live
            .values()
            .filter_map(|l| l.trigger.as_ref().map(|t| (l.id, t.as_fd())))
    }

    /// The job whose trigger `id` fired: its session scope and name.
    pub fn stalled(&self, id: u64) -> Option<(&str, &str)> {
        self.live
            .iter()
            .find(|(_, l)| l.id == id)
            .map(|((scope, name), _)| (scope.as_str(), name.as_str()))
    }

    /// Drops the trigger `id`, which reported something other than a stall:
    /// its group is going away.
    pub fn disarm(&mut self, id: u64) {
        if let Some(l) = self.live.values_mut().find(|l| l.id == id) {
            l.trigger = None;
        }
    }

    /// The shells of the Bash calls running, by id: each becomes readable
    /// when its call ends.
    pub fn shells(&self) -> impl Iterator<Item = (u64, BorrowedFd<'_>)> {
        self.live
            .values()
            .filter_map(|l| l.shell.as_ref().map(|s| (l.id, s.as_fd())))
    }

    /// The Bash call of the job `id` ended: the kills counted by now were
    /// the call's, later ones are not.
    pub fn call_ended(&mut self, id: u64) {
        let Some(key) = self
            .live
            .iter()
            .find(|(_, l)| l.id == id)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        self.read_kills(&key.0, &key.1);
        if let Some(l) = self.live.get_mut(&key) {
            l.shell = None;
        }
    }

    /// The kills read since the last call.
    pub fn take_kills(&mut self) -> Vec<Killed> {
        std::mem::take(&mut self.kills)
    }

    /// Handles the changes already reported, without blocking.
    pub fn read_ready(&mut self) -> io::Result<Vec<Measurement>> {
        let mut buffer = [0; EVENT_BUFFER];
        match self.inotify.read_events(&mut buffer) {
            Ok(events) => {
                let events = owned(events);
                self.handle_all(events)
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Reads the kills of every job, then sweeps the tree like `collect`.
    pub fn sweep(&mut self) -> io::Result<Vec<Measurement>> {
        let jobs: Vec<(String, String)> = self.live.keys().cloned().collect();
        let scopes: Vec<String> = self.scopes.keys().cloned().collect();
        for (scope, name) in &jobs {
            self.read_kills(scope, name);
        }
        for scope in &scopes {
            self.read_kills(scope, MAIN);
        }
        self.resync()
    }

    fn handle_all(&mut self, events: Vec<OwnedEvent>) -> io::Result<Vec<Measurement>> {
        let mut out = Vec::new();
        for (wd, mask, name) in events {
            if mask.contains(EventMask::Q_OVERFLOW) {
                out.extend(self.resync()?);
            } else {
                self.handle(&wd, mask, name.as_deref(), &mut out);
            }
        }
        Ok(out)
    }

    fn handle(
        &mut self,
        wd: &WatchDescriptor,
        mask: EventMask,
        name: Option<&OsStr>,
        out: &mut Vec<Measurement>,
    ) {
        if mask.contains(EventMask::IGNORED) {
            // The slice itself went away: wait for it to come back.
            if let Some(Watched::Slice) = self.watched.remove(wd) {
                let _ = self.watch_slice(out);
            }
            return;
        }
        let Some(watched) = self.watched.get(wd).cloned() else {
            return;
        };
        let name = name.map(|n| n.to_string_lossy().into_owned());
        let created = mask.contains(EventMask::CREATE);
        let modified = mask.contains(EventMask::MODIFY);
        match (watched, name) {
            (Watched::Manager, Some(n)) if created && n == cgroup::SLICE => {
                let _ = self.inotify.watches().remove(wd.clone());
                self.watched.remove(wd);
                let _ = self.watch_slice(out);
            }
            (Watched::Slice, Some(n)) if is_scope(&n) => {
                if created {
                    self.watch_scope(&n, out);
                } else if mask.contains(EventMask::DELETE) {
                    let _ = fs::remove_dir_all(self.records.join(&n));
                    self.forget_scope(&n);
                }
            }
            (Watched::Scope(session), Some(n)) if created => self.watch_job(&session, &n, out),
            (Watched::Job { session, name }, None) if modified => {
                self.check(&session, &name, true, out);
            }
            (Watched::Memory { session, job }, None) if modified => {
                self.read_kills(&session, &job);
            }
            _ => {}
        }
    }

    /// Sweeps the tree, then watches whatever exists, and forgets what is
    /// gone.
    fn resync(&mut self) -> io::Result<Vec<Measurement>> {
        let mut out = collect(&self.slice, &self.records, runtime::now_ms(), self.remove)?;
        self.watch_slice(&mut out)?;
        let slice = &self.slice;
        let jobs: Vec<(String, String)> = self
            .live
            .keys()
            .filter(|(scope, name)| !slice.join(scope).join(name).exists())
            .cloned()
            .collect();
        let scopes: Vec<String> = self
            .scopes
            .keys()
            .filter(|scope| !slice.join(scope).exists())
            .cloned()
            .collect();
        for key in &jobs {
            self.forget(key);
        }
        for scope in &scopes {
            self.forget_scope(scope);
        }
        Ok(out)
    }

    fn watch_slice(&mut self, out: &mut Vec<Measurement>) -> io::Result<()> {
        let slice = self.slice.clone();
        let mask = WatchMask::CREATE | WatchMask::DELETE | WatchMask::ONLYDIR;
        match self.add(&slice, mask, Watched::Slice) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // No session started yet: wait for the slice to appear.
                let manager = slice.parent().ok_or(e)?;
                let mask = WatchMask::CREATE | WatchMask::ONLYDIR;
                return self.add(manager, mask, Watched::Manager).map(|_| ());
            }
            Err(e) => return Err(e),
        }
        for scope in entries(&slice)? {
            let session = name_of(&scope);
            if is_scope(&session) {
                self.watch_scope(&session, out);
            }
        }
        Ok(())
    }

    /// A scope that vanishes meanwhile is no error: its session ended.
    fn watch_scope(&mut self, session: &str, out: &mut Vec<Measurement>) {
        let dir = self.slice.join(session);
        if !self.scopes.contains_key(session) {
            let mask = WatchMask::CREATE | WatchMask::ONLYDIR;
            let Ok(wd) = self.add(&dir, mask, Watched::Scope(session.into())) else {
                return;
            };
            let scope = Scope {
                killed: None,
                watches: vec![wd],
            };
            self.scopes.insert(session.into(), scope);
        }
        for job in entries(&dir).unwrap_or_default() {
            self.watch_job(session, &name_of(&job), out);
        }
    }

    /// Watches a job's end and its kills, and arms its trigger. `MAIN`, which
    /// `launch` creates just after the scope, gets the kills only.
    fn watch_job(&mut self, session: &str, name: &str, out: &mut Vec<Measurement>) {
        if name == MAIN {
            self.watch_main(session);
            return;
        }
        let Some((kind, _)) = prefix::parse_job_name(name) else {
            return;
        };
        let key = (session.to_string(), name.to_string());
        if !self.live.contains_key(&key) {
            let dir = self.slice.join(session).join(name);
            let what = Watched::Job {
                session: session.into(),
                name: name.into(),
            };
            let Ok(ended) = self.add(&dir.join("cgroup.events"), WatchMask::MODIFY, what) else {
                return;
            };
            let memory = Watched::Memory {
                session: session.into(),
                job: name.into(),
            };
            let watches = [
                Ok(ended),
                self.add(&dir.join("memory.events"), WatchMask::MODIFY, memory),
            ];
            self.next_id += 1;
            let live = Live {
                trigger: Trigger::new(&dir.join("memory.pressure"), self.stall).ok(),
                id: self.next_id,
                killed: read_oom_kill(&dir).unwrap_or(0),
                shell_pid: prefix::job_pid(name).filter(|_| kind == Kind::Bash),
                shell: None,
                watches: watches.into_iter().flatten().collect(),
            };
            self.live.insert(key, live);
        }
        // The job may have ended before the watch was in place.
        self.check(session, name, false, out);
    }

    fn watch_main(&mut self, session: &str) {
        if self.scopes.get(session).is_none_or(|s| s.killed.is_some()) {
            return;
        }
        let dir = self.slice.join(session).join(MAIN);
        let what = Watched::Memory {
            session: session.into(),
            job: MAIN.into(),
        };
        if let Ok(wd) = self.add(&dir.join("memory.events"), WatchMask::MODIFY, what) {
            let killed = read_oom_kill(&dir).unwrap_or(0);
            if let Some(scope) = self.scopes.get_mut(session) {
                scope.killed = Some(killed);
                scope.watches.push(wd);
            }
        }
    }

    fn check(&mut self, session: &str, name: &str, ended: bool, out: &mut Vec<Measurement>) {
        let key = (session.to_string(), name.to_string());
        self.hold_shell(&key);
        // Before `finish` may remove the group, record included.
        self.read_kills(session, name);
        let dir = self.slice.join(session).join(name);
        out.extend(finish(
            &dir,
            session,
            name,
            &self.records,
            runtime::now_ms(),
            ended,
            self.remove,
        ));
        if !dir.exists() {
            self.forget(&key);
        }
    }

    /// Holds the shell of a Bash call once it is in the job: in the group,
    /// the pid is the call's shell, not a reused one.
    fn hold_shell(&mut self, key: &(String, String)) {
        let dir = self.slice.join(&key.0).join(&key.1);
        let Some(l) = self.live.get_mut(key) else {
            return;
        };
        let Some(pid) = l.shell_pid.filter(|pid| holds(&dir, *pid)) else {
            return;
        };
        l.shell_pid = None;
        l.shell = i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .and_then(|pid| pidfd_open(pid, PidfdFlags::empty()).ok());
    }

    /// Keeps the processes of `job` killed since it was last read.
    fn read_kills(&mut self, session: &str, job: &str) {
        let Some(now) = read_oom_kill(&self.slice.join(session).join(job)) else {
            return;
        };
        let last = if job == MAIN {
            self.scopes
                .get_mut(session)
                .and_then(|s| s.killed.as_mut())
                .map(|last| (last, false))
        } else {
            self.live
                .get_mut(&(session.to_string(), job.to_string()))
                .map(|l| (&mut l.killed, l.shell.is_some()))
        };
        let Some((last, call_running)) = last else {
            return;
        };
        let killed = now.saturating_sub(*last);
        *last = now;
        if killed == 0 {
            return;
        }
        let record = (job != MAIN)
            .then(|| read_record(&self.records.join(session).join(format!("{job}.json"))))
            .flatten();
        self.kills.push(Killed {
            session: session.into(),
            job: job.into(),
            killed,
            call_running,
            record,
        });
    }

    /// Drops what the tracker holds on a job whose group is gone.
    fn forget(&mut self, key: &(String, String)) {
        if let Some(l) = self.live.remove(key) {
            for wd in l.watches {
                self.unwatch(&wd);
            }
        }
    }

    /// Drops what the tracker holds on a session whose scope is gone.
    fn forget_scope(&mut self, session: &str) {
        let jobs: Vec<(String, String)> = self
            .live
            .keys()
            .filter(|(scope, _)| scope == session)
            .cloned()
            .collect();
        for key in &jobs {
            self.forget(key);
        }
        if let Some(scope) = self.scopes.remove(session) {
            for wd in scope.watches {
                self.unwatch(&wd);
            }
        }
    }

    fn add(&mut self, path: &Path, mask: WatchMask, what: Watched) -> io::Result<WatchDescriptor> {
        let wd = self.inotify.watches().add(path, mask)?;
        self.watched.insert(wd.clone(), what);
        Ok(wd)
    }

    fn unwatch(&mut self, wd: &WatchDescriptor) {
        let _ = self.inotify.watches().remove(wd.clone());
        self.watched.remove(wd);
    }
}

type OwnedEvent = (WatchDescriptor, EventMask, Option<OsString>);

fn owned(events: Events<'_>) -> Vec<OwnedEvent> {
    events
        .map(|e| (e.wd, e.mask, e.name.map(OsStr::to_os_string)))
        .collect()
}

/// The entries of `dir`, none when it does not exist (yet, or any more).
fn entries(dir: &Path) -> io::Result<Vec<DirEntry>> {
    match fs::read_dir(dir) {
        Ok(rd) => Ok(rd.flatten().collect()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn name_of(entry: &DirEntry) -> String {
    entry.file_name().to_string_lossy().into_owned()
}

fn is_scope(name: &str) -> bool {
    Path::new(name).extension().is_some_and(|e| e == "scope")
}

fn unpopulated(job: &Path) -> bool {
    fs::read_to_string(job.join("cgroup.events"))
        .is_ok_and(|e| e.lines().any(|l| l == "populated 0"))
}

fn read_peak_mb(job: &Path) -> Option<u64> {
    let bytes: u64 = fs::read_to_string(job.join("memory.peak"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(bytes / (1024 * 1024))
}

/// Processes of the group `dir` killed by an OOM killer, the machine's
/// included.
fn read_oom_kill(dir: &Path) -> Option<u64> {
    fs::read_to_string(dir.join("memory.events"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("oom_kill "))?
        .trim()
        .parse()
        .ok()
}

/// Whether `pid` is in the group `dir`.
fn holds(dir: &Path, pid: u32) -> bool {
    fs::read_to_string(dir.join("cgroup.procs"))
        .is_ok_and(|procs| procs.lines().any(|l| l.trim().parse() == Ok(pid)))
}

fn read_record(path: &Path) -> Option<JobRecord> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const NOW: u128 = 100_000;

    struct Tree {
        _tmp: tempfile::TempDir,
        slice: PathBuf,
        records: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        let slice = tmp.path().join("orchestrator.slice");
        let records = tmp.path().join("jobs");
        fs::create_dir_all(&slice).unwrap();
        Tree {
            _tmp: tmp,
            slice,
            records,
        }
    }

    fn job(t: &Tree, scope: &str, name: &str, populated: bool, peak_bytes: u64) -> PathBuf {
        let dir = t.slice.join(scope).join(name);
        fs::create_dir_all(&dir).unwrap();
        let events = format!("populated {}\nfrozen 0\n", u8::from(populated));
        fs::write(dir.join("cgroup.events"), events).unwrap();
        fs::write(dir.join("memory.peak"), format!("{peak_bytes}\n")).unwrap();
        dir
    }

    fn record(t: &Tree, scope: &str, name: &str, command: &str) -> PathBuf {
        let dir = t.records.join(scope);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.json"));
        let rec = JobRecord {
            command: command.into(),
            cwd: "/repo".into(),
        };
        fs::write(&path, serde_json::to_vec(&rec).unwrap()).unwrap();
        path
    }

    fn collect_all(t: &Tree) -> Vec<Measurement> {
        collect(&t.slice, &t.records, NOW, |p| fs::remove_dir_all(p)).unwrap()
    }

    #[test]
    fn a_finished_bash_job_is_measured_once_then_removed() {
        let t = tree();
        let dir = job(&t, "s.scope", "job-bash-7-1000", false, 3 << 30);
        let rec = record(&t, "s.scope", "job-bash-7-1000", "eval 'pnpm typecheck'");
        let measured = collect_all(&t);
        assert_eq!(measured.len(), 1);
        assert_eq!(measured[0].peak_mb, 3072);
        assert_eq!(measured[0].session, "s.scope");
        assert_eq!(measured[0].command, "eval 'pnpm typecheck'");
        assert!(!dir.exists());
        assert!(!rec.exists());
        assert_eq!(collect_all(&t), Vec::new());
    }

    #[test]
    fn running_young_and_foreign_groups_stay() {
        let t = tree();
        let running = job(&t, "s.scope", "job-bash-7-1000", true, 1 << 20);
        let young = job(&t, "s.scope", "job-bash-8-99000", false, 1 << 20);
        let main = job(&t, "s.scope", "main", false, 1 << 20);
        assert_eq!(collect_all(&t), Vec::new());
        assert!(running.exists() && young.exists() && main.exists());
    }

    #[test]
    fn other_jobs_are_removed_without_a_measurement() {
        let t = tree();
        let dir = job(&t, "s.scope", "job-other-7-1000", false, 1 << 20);
        assert_eq!(collect_all(&t), Vec::new());
        assert!(!dir.exists());
    }

    #[test]
    fn a_group_that_cannot_be_removed_keeps_its_record() {
        let t = tree();
        let dir = job(&t, "s.scope", "job-bash-7-1000", false, 1 << 20);
        let rec = record(&t, "s.scope", "job-bash-7-1000", "x");
        let failing = |_: &Path| Err(io::Error::from(ErrorKind::ResourceBusy));
        let measured = collect(&t.slice, &t.records, NOW, failing).unwrap();
        assert_eq!(measured, Vec::new());
        assert!(dir.exists() && rec.exists());
        assert_eq!(collect_all(&t).len(), 1);
    }

    #[test]
    fn records_of_gone_sessions_are_dropped() {
        let t = tree();
        job(&t, "live.scope", "job-bash-7-99000", true, 0);
        let kept = record(&t, "live.scope", "job-bash-7-99000", "x");
        let gone = record(&t, "gone.scope", "job-bash-8-1000", "y");
        collect_all(&t);
        assert!(kept.exists());
        assert!(!gone.exists());
    }

    #[test]
    fn nothing_to_do_before_any_session() {
        let t = tree();
        fs::remove_dir(&t.slice).unwrap();
        assert_eq!(collect_all(&t), Vec::new());
    }

    fn remove_all(dir: &Path) -> io::Result<()> {
        fs::remove_dir_all(dir)
    }

    fn set_populated(dir: &Path, populated: bool) {
        let events = format!("populated {}\nfrozen 0\n", u8::from(populated));
        fs::write(dir.join("cgroup.events"), events).unwrap();
    }

    fn set_kills(dir: &Path, killed: u64) {
        let events = format!("low 0\nhigh 0\nmax 0\noom 0\noom_kill {killed}\n");
        fs::write(dir.join("memory.events"), events).unwrap();
    }

    fn track(t: &Tree) -> (Tracker, Vec<Measurement>) {
        let stall = Duration::from_millis(200);
        Tracker::new(t.slice.clone(), t.records.clone(), stall, remove_all).unwrap()
    }

    #[test]
    fn a_kill_after_the_call_ended_is_not_the_calls() {
        let t = tree();
        fs::create_dir(t.slice.join("s.scope")).unwrap();
        let (mut tracker, _) = track(&t);
        // A child of this test stands for the call's shell.
        let mut shell = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let name = prefix::job_name(Kind::Bash, shell.id(), runtime::now_ms());
        let dir = job(&t, "s.scope", &name, true, 1 << 20);
        fs::write(dir.join("cgroup.procs"), format!("{}\n", shell.id())).unwrap();
        set_kills(&dir, 0);
        tracker.read_ready().unwrap();
        let (id, _) = tracker.shells().next().unwrap();
        // Killed during the call, read as the call ends.
        set_kills(&dir, 1);
        shell.kill().unwrap();
        shell.wait().unwrap();
        tracker.call_ended(id);
        // Then a process it left behind is killed.
        set_kills(&dir, 2);
        tracker.read_ready().unwrap();
        let kills: Vec<(u64, bool)> = tracker
            .take_kills()
            .iter()
            .map(|k| (k.killed, k.call_running))
            .collect();
        assert_eq!(kills, [(1, true), (1, false)]);
        assert_eq!(tracker.shells().count(), 0);
    }

    #[test]
    fn a_kill_in_a_running_call_is_read_as_it_happens() {
        let t = tree();
        fs::create_dir(t.slice.join("s.scope")).unwrap();
        let (mut tracker, _) = track(&t);
        // This test process stands for the call's shell.
        let me = std::process::id();
        let name = prefix::job_name(Kind::Bash, me, runtime::now_ms());
        let dir = job(&t, "s.scope", &name, true, 1 << 20);
        fs::write(dir.join("cgroup.procs"), format!("{me}\n")).unwrap();
        set_kills(&dir, 0);
        record(&t, "s.scope", &name, "eval 'pnpm test'");
        tracker.read_ready().unwrap();
        assert_eq!(tracker.take_kills(), Vec::new());
        set_kills(&dir, 2);
        tracker.read_ready().unwrap();
        let kills = tracker.take_kills();
        assert_eq!(kills.len(), 1);
        assert_eq!((kills[0].job.as_str(), kills[0].killed), (name.as_str(), 2));
        assert!(kills[0].call_running);
        assert_eq!(
            kills[0].record.as_ref().map(|r| r.command.as_str()),
            Some("eval 'pnpm test'")
        );
    }

    #[test]
    fn a_kill_as_the_job_ends_is_read_before_its_group_goes() {
        let t = tree();
        fs::create_dir(t.slice.join("s.scope")).unwrap();
        let (mut tracker, _) = track(&t);
        let name = prefix::job_name(Kind::Bash, 7, runtime::now_ms());
        let dir = job(&t, "s.scope", &name, true, 1 << 20);
        set_kills(&dir, 0);
        record(&t, "s.scope", &name, "x");
        tracker.read_ready().unwrap();
        set_kills(&dir, 1);
        set_populated(&dir, false);
        assert_eq!(tracker.read_ready().unwrap().len(), 1);
        let kills = tracker.take_kills();
        assert_eq!(kills.len(), 1);
        assert!(!kills[0].call_running);
        assert!(kills[0].record.is_some());
        assert!(!dir.exists());
        // The group is gone, and so is what the tracker held on it.
        tracker.read_ready().unwrap();
        assert_eq!(tracker.live.len(), 0);
    }

    #[test]
    fn a_gone_group_leaves_no_watch_behind() {
        let t = tree();
        let scope = t.slice.join("s.scope");
        fs::create_dir(&scope).unwrap();
        let (mut tracker, _) = track(&t);
        let before = tracker.watched.len();
        let name = prefix::job_name(Kind::Other, 7, runtime::now_ms());
        let dir = job(&t, "s.scope", &name, true, 1 << 20);
        set_kills(&dir, 0);
        tracker.read_ready().unwrap();
        assert_eq!(tracker.watched.len(), before + 2);
        // The tracker removes the group it measured.
        set_populated(&dir, false);
        tracker.read_ready().unwrap();
        assert_eq!(tracker.watched.len(), before);
        // systemd removes the scope of an ended session, with its groups.
        let other = job(&t, "s.scope", &name, true, 1 << 20);
        tracker.read_ready().unwrap();
        fs::remove_dir_all(&other).unwrap();
        fs::remove_dir(&scope).unwrap();
        tracker.read_ready().unwrap();
        assert_eq!(tracker.live.len(), 0);
        assert_eq!(tracker.watched.len(), before - 1);
    }

    #[test]
    fn a_shell_is_held_once_the_prefix_entered_its_group() {
        let t = tree();
        fs::create_dir(t.slice.join("s.scope")).unwrap();
        let (mut tracker, _) = track(&t);
        let me = std::process::id();
        let name = prefix::job_name(Kind::Bash, me, runtime::now_ms());
        // The group appears empty; the prefix moves in a moment later.
        let dir = job(&t, "s.scope", &name, false, 1 << 20);
        tracker.read_ready().unwrap();
        assert_eq!(tracker.shells().count(), 0);
        fs::write(dir.join("cgroup.procs"), format!("{me}\n")).unwrap();
        set_populated(&dir, true);
        tracker.read_ready().unwrap();
        assert_eq!(tracker.shells().count(), 1);
    }

    #[test]
    fn a_kill_in_main_is_read_too() {
        let t = tree();
        let main = t.slice.join("s.scope").join(MAIN);
        fs::create_dir_all(&main).unwrap();
        set_kills(&main, 0);
        let (mut tracker, _) = track(&t);
        set_kills(&main, 1);
        tracker.read_ready().unwrap();
        let kills = tracker.take_kills();
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0].job, MAIN);
        assert!(kills[0].record.is_none() && !kills[0].call_running);
    }

    #[test]
    fn a_sweep_reads_the_kills_it_would_otherwise_lose() {
        let t = tree();
        let dir = job(&t, "s.scope", "job-other-7-1000", true, 1 << 20);
        set_kills(&dir, 0);
        let (mut tracker, _) = track(&t);
        // Written without a change reported: a sweep still finds it.
        tracker
            .inotify
            .watches()
            .remove(
                tracker
                    .watched
                    .iter()
                    .find(|(_, w)| matches!(w, Watched::Memory { .. }))
                    .map(|(wd, _)| wd.clone())
                    .unwrap(),
            )
            .unwrap();
        set_kills(&dir, 1);
        set_populated(&dir, false);
        tracker.sweep().unwrap();
        assert_eq!(tracker.take_kills().len(), 1);
        assert!(!dir.exists());
    }

    #[test]
    fn the_tracker_measures_a_job_as_soon_as_it_ends() {
        let t = tree();
        fs::remove_dir(&t.slice).unwrap();
        let (mut tracker, measured) = track(&t);
        assert_eq!(measured, Vec::new());
        // Sessions start after the service: the slice, then a scope, appear.
        fs::create_dir(&t.slice).unwrap();
        assert_eq!(tracker.read_ready().unwrap(), Vec::new());
        fs::create_dir(t.slice.join("s.scope")).unwrap();
        assert_eq!(tracker.read_ready().unwrap(), Vec::new());
        // A young job: only its end, reported by the kernel, finishes it.
        let name = prefix::job_name(Kind::Bash, 7, runtime::now_ms());
        let dir = job(&t, "s.scope", &name, true, 3 << 30);
        record(&t, "s.scope", &name, "eval 'pnpm typecheck'");
        assert_eq!(tracker.read_ready().unwrap(), Vec::new());
        set_populated(&dir, false);
        let measured = tracker.read_ready().unwrap();
        assert_eq!(measured.len(), 1);
        assert_eq!(measured[0].job, name);
        assert_eq!(measured[0].peak_mb, 3072);
        assert!(!dir.exists());
    }

    #[test]
    fn the_tracker_catches_up_at_start() {
        let t = tree();
        job(&t, "s.scope", "job-bash-7-1000", false, 1 << 20);
        record(&t, "s.scope", "job-bash-7-1000", "x");
        let (_, measured) = track(&t);
        assert_eq!(measured.len(), 1);
    }

    #[test]
    fn the_tracker_drops_the_records_of_an_ended_session() {
        let t = tree();
        let scope = t.slice.join("s.scope");
        fs::create_dir(&scope).unwrap();
        let (mut tracker, _) = track(&t);
        let rec = record(&t, "s.scope", "job-bash-7-1", "x");
        fs::remove_dir(&scope).unwrap();
        tracker.read_ready().unwrap();
        assert!(!rec.exists());
    }
}
