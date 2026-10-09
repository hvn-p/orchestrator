//! Finished jobs. Once every process of a job has exited, its group still
//! holds the job's memory peak. The service reads it, keeps it with the
//! command of a Bash call, then removes the group. systemd removes every group
//! of a session when the session ends, so peaks are read while it lives.
//!
//! `Tracker` follows the session tree through inotify: the kernel reports each
//! new session scope, each new job group and each change of a job's
//! `cgroup.events`, so a job is measured as soon as it ends. `collect` sweeps
//! the whole tree instead. It catches up at start and after an event queue
//! overflow, and finds the rare job that ended before its watch was in place.

use claude_code::invocation::Kind;
use inotify::{EventMask, Events, Inotify, WatchDescriptor, WatchMask};
use learning::peaks::Measurement;
use prefix::{self, JobRecord};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirEntry};
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use system::cgroup;
use system::runtime;

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
}

/// Follows the session tree through inotify and measures each job as it ends.
pub struct Tracker {
    inotify: Inotify,
    slice: PathBuf,
    records: PathBuf,
    remove: Remove,
    watched: HashMap<WatchDescriptor, Watched>,
}

impl Tracker {
    /// Starts watching the tree, and returns what a first sweep measured.
    pub fn new(
        slice: PathBuf,
        records: PathBuf,
        remove: Remove,
    ) -> io::Result<(Tracker, Vec<Measurement>)> {
        let mut tracker = Tracker {
            inotify: Inotify::init()?,
            slice,
            records,
            remove,
            watched: HashMap::new(),
        };
        let measured = tracker.resync()?;
        Ok((tracker, measured))
    }

    /// Readable when the kernel reported changes.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.inotify.as_fd()
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
                }
            }
            (Watched::Scope(session), Some(n)) if created => self.watch_job(&session, &n, out),
            (Watched::Job { session, name }, None) if mask.contains(EventMask::MODIFY) => {
                self.check(&session, &name, true, out);
            }
            _ => {}
        }
    }

    /// Sweeps the tree, then watches whatever exists.
    fn resync(&mut self) -> io::Result<Vec<Measurement>> {
        let mut out = collect(&self.slice, &self.records, runtime::now_ms(), self.remove)?;
        self.watch_slice(&mut out)?;
        Ok(out)
    }

    fn watch_slice(&mut self, out: &mut Vec<Measurement>) -> io::Result<()> {
        let slice = self.slice.clone();
        let mask = WatchMask::CREATE | WatchMask::DELETE | WatchMask::ONLYDIR;
        match self.add(&slice, mask, Watched::Slice) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // No session started yet: wait for the slice to appear.
                let manager = slice.parent().ok_or(e)?;
                let mask = WatchMask::CREATE | WatchMask::ONLYDIR;
                return self.add(manager, mask, Watched::Manager);
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
        let mask = WatchMask::CREATE | WatchMask::ONLYDIR;
        if self
            .add(&dir, mask, Watched::Scope(session.into()))
            .is_err()
        {
            return;
        }
        for job in entries(&dir).unwrap_or_default() {
            self.watch_job(session, &name_of(&job), out);
        }
    }

    fn watch_job(&mut self, session: &str, name: &str, out: &mut Vec<Measurement>) {
        if prefix::parse_job_name(name).is_none() {
            return;
        }
        let events = self.slice.join(session).join(name).join("cgroup.events");
        let what = Watched::Job {
            session: session.into(),
            name: name.into(),
        };
        if self.add(&events, WatchMask::MODIFY, what).is_ok() {
            // The job may have ended before the watch was in place.
            self.check(session, name, false, out);
        }
    }

    fn check(&self, session: &str, name: &str, ended: bool, out: &mut Vec<Measurement>) {
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
    }

    fn add(&mut self, path: &Path, mask: WatchMask, what: Watched) -> io::Result<()> {
        let wd = self.inotify.watches().add(path, mask)?;
        self.watched.insert(wd, what);
        Ok(())
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

    #[test]
    fn the_tracker_measures_a_job_as_soon_as_it_ends() {
        let t = tree();
        fs::remove_dir(&t.slice).unwrap();
        let (mut tracker, measured) =
            Tracker::new(t.slice.clone(), t.records.clone(), remove_all).unwrap();
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
        let (_, measured) = Tracker::new(t.slice.clone(), t.records.clone(), remove_all).unwrap();
        assert_eq!(measured.len(), 1);
    }

    #[test]
    fn the_tracker_drops_the_records_of_an_ended_session() {
        let t = tree();
        let scope = t.slice.join("s.scope");
        fs::create_dir(&scope).unwrap();
        let (mut tracker, _) =
            Tracker::new(t.slice.clone(), t.records.clone(), remove_all).unwrap();
        let rec = record(&t, "s.scope", "job-bash-7-1", "x");
        fs::remove_dir(&scope).unwrap();
        tracker.read_ready().unwrap();
        assert!(!rec.exists());
    }
}
