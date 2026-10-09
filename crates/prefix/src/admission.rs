//! Admission: whether a Bash call starts at once or waits for memory. See
//! docs/design.md, "Admission".
//!
//! The prefix looks the call's commands up in the learned peaks. The call's
//! expected peak is the largest of its commands', never their sum: a call has
//! a single peak, which bounds each of its commands, so a filter such as
//! `tail -1` learned only beside a heavy command carries that same peak. For
//! the same reason the notices name the call after its first heavy command
//! that has run alone, whose peak was measured, else after its first heavy
//! command. A call whose expected peak is unknown or under the threshold
//! starts at once and reserves nothing. A heavy call waits until free memory
//! covers its expected peak plus a margin.
//!
//! Free memory is the kernel's `MemAvailable` net of reservations. From its
//! admission until its job group empties, a heavy call reserves its expected
//! peak minus what the group already uses: a call still climbing to its peak
//! keeps a second one from counting on the same memory. A reservation is a
//! file in the runtime directory. Counting and reserving happen under one
//! file lock, so concurrent prefixes never count on the same memory. A
//! reservation whose group is gone or empty holds nothing; the next check
//! removes it.
//!
//! A waiting call checks again as soon as inotify reports a change in the
//! `cgroup.events` of a job holding a reservation, which is how memory mostly
//! frees up, and every `RECHECK` otherwise, since available memory has no
//! notification. It writes one notice to standard error when it starts
//! waiting and one when it proceeds: the call's output goes back to Claude.
//! After the configured longest wait, it runs anyway, reserving its peak.
//!
//! While it waits, a call is also recorded in the runtime directory, so that
//! `orchestrator admission` can show it and `watch` can wake the coordinator
//! when it waits long. Reading the waiting calls and the reservations takes
//! no lock: what a reader shows may be a check behind.

use anyhow::{Context, Result, bail};
use config::Admission;
use inotify::{Inotify, WatchMask};
use learning::peaks;
use learning::recognise;
use learning::repository;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use system::memory;
use system::runtime;

/// Between two checks of a waiting call when no job holding a reservation
/// reports a change.
pub const RECHECK: Duration = Duration::from_secs(1);
/// The longest a prefix tries to take the lock, which a check holds for a few
/// reads and a write. Past it, something is stuck, a frozen holder for
/// instance, and the call runs.
const LOCK_PATIENCE: Duration = Duration::from_secs(1);
/// Between two attempts at the lock.
const LOCK_RETRY: Duration = Duration::from_millis(5);

/// A command of a call with a learned peak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    /// What the notices show of the command: its learned label.
    pub label: String,
    pub peak_mb: u64,
    /// It ran alone in one of its latest calls, so its peak was measured
    /// rather than shared with other commands.
    pub alone: bool,
}

/// A heavy call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heavy {
    /// The label of the command it goes by.
    pub label: String,
    /// Its expected peak.
    pub peak_mb: u64,
}

/// Where admission reads and keeps its state.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The cgroup v2 root.
    pub cgroup_root: PathBuf,
    pub meminfo: PathBuf,
    /// Holds the lock and the reservations.
    pub runtime: PathBuf,
}

/// The job group a call runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Its path from the cgroup root.
    pub group: String,
    /// Its name, unique among live jobs.
    pub name: String,
}

/// One admitted heavy call, kept as `<runtime>/reservations/<job>.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Reservation {
    /// The job group, from the cgroup root.
    group: String,
    peak_mb: u64,
    /// The call's label; empty in a reservation written before labels.
    #[serde(default)]
    label: String,
}

/// A heavy call waiting for memory, kept as `<runtime>/waiting/<job>.json`
/// from its first wait until it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    /// Its job's name.
    pub job: String,
    /// Its job group, from the cgroup root.
    pub group: String,
    pub label: String,
    pub peak_mb: u64,
    /// The free memory it waits for.
    pub need_mb: u64,
    /// When it started waiting, in ms since the Unix epoch.
    pub since_ms: u64,
}

/// A heavy call running with a reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reserved {
    pub job: String,
    /// Its job group, from the cgroup root.
    pub group: String,
    pub label: String,
    pub peak_mb: u64,
    /// What its group uses now, None without a memory controller.
    pub current_mb: Option<u64>,
    /// What its reservation still holds.
    pub held_mb: u64,
}

/// Admission's state as a reader sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub available_mb: u64,
    /// What the reservations still hold, together.
    pub held_mb: u64,
    /// Oldest first.
    pub waiting: Vec<Waiting>,
    pub reserved: Vec<Reserved>,
}

impl Snapshot {
    /// Free memory as admission counts it.
    pub fn free_mb(&self) -> u64 {
        free_mb(self.available_mb, self.held_mb)
    }
}

/// What a check decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Free memory covers the call: it runs.
    Run,
    /// The call has waited as long as it may: it runs anyway.
    RunOverdue,
    /// The call waits, at most this long before the next check.
    Wait(Duration),
}

/// What one check found.
#[derive(Debug)]
struct Check {
    step: Step,
    free_mb: u64,
    /// What the reservations of running heavy calls still hold.
    held_mb: u64,
    /// The job groups holding a reservation.
    holders: Vec<PathBuf>,
}

/// The commands of the Bash call `invocation`, run from `cwd`, that have a
/// learned peak, in the call's order. Empty when the call cannot be parsed.
pub fn known(invocation: &str, cwd: &Path, home: Option<&Path>, peaks_dir: &Path) -> Vec<Known> {
    let Some(commands) = claude_code::invocation::written(invocation)
        .and_then(|script| recognise::commands(&script, cwd, home))
    else {
        return Vec::new();
    };
    let mut keys: Vec<(&Path, PathBuf)> = Vec::new();
    let mut known = Vec::new();
    for c in &commands {
        let Some(dir) = c.dir.as_deref() else {
            continue;
        };
        let key = if let Some((_, key)) = keys.iter().find(|(d, _)| *d == dir) {
            key.clone()
        } else {
            let key = repository::key(dir);
            keys.push((dir, key.clone()));
            key
        };
        if let Some(entry) = peaks::entry(peaks_dir, &key, c) {
            known.push(Known {
                label: entry.label,
                peak_mb: entry.peak_mb,
                alone: entry.recent.iter().any(|call| call.1),
            });
        }
    }
    known
}

/// The call made of the `known` commands, if it is heavy: its expected peak
/// is the largest of theirs, never their sum. It goes by its first heavy
/// command that has run alone, else by its first heavy command.
pub fn heavy(known: &[Known], cfg: &Admission) -> Option<Heavy> {
    let peak_mb = known.iter().map(|k| k.peak_mb).max()?;
    let mut heavy = known.iter().filter(|k| is_heavy(k.peak_mb, cfg));
    let first = heavy.clone().find(|k| k.alone).or_else(|| heavy.next())?;
    Some(Heavy {
        label: first.label.clone(),
        peak_mb,
    })
}

/// A call this heavy waits for memory.
pub fn is_heavy(peak_mb: u64, cfg: &Admission) -> bool {
    peak_mb >= cfg.heavy_mb
}

/// What a reservation still holds: the expected peak its job does not use yet.
pub fn held_mb(peak_mb: u64, current_mb: u64) -> u64 {
    peak_mb.saturating_sub(current_mb)
}

/// Available memory net of what reservations hold.
pub fn free_mb(available_mb: u64, held_mb: u64) -> u64 {
    available_mb.saturating_sub(held_mb)
}

/// Free memory a heavy call needs before it starts.
pub fn need_mb(peak_mb: u64, cfg: &Admission) -> u64 {
    peak_mb.saturating_add(cfg.margin_mb)
}

/// Whether a call that needs `need_mb` runs now, `waited` after it arrived.
pub fn step(
    free_mb: u64,
    need_mb: u64,
    waited: Duration,
    max_wait: Duration,
    recheck: Duration,
) -> Step {
    if free_mb >= need_mb {
        Step::Run
    } else if waited >= max_wait {
        Step::RunOverdue
    } else {
        Step::Wait(max_wait.saturating_sub(waited).min(recheck))
    }
}

/// Lets the heavy `call` run in `job` once free memory covers it, or
/// once it has waited `max_wait_secs`, and reserves its peak. Notices go to
/// `out`. On an error the call runs at once, unreserved.
pub fn admit(
    paths: &Paths,
    cfg: &Admission,
    job: &Job,
    call: &Heavy,
    out: &mut dyn Write,
) -> Result<()> {
    admit_every(paths, cfg, job, call, RECHECK, out)
}

fn admit_every(
    paths: &Paths,
    cfg: &Admission,
    job: &Job,
    call: &Heavy,
    recheck: Duration,
    out: &mut dyn Write,
) -> Result<()> {
    let start = Instant::now();
    let need = need_mb(call.peak_mb, cfg);
    let max_wait = Duration::from_secs(cfg.max_wait_secs);
    let mut waiting: Option<Recorded> = None;
    let result = loop {
        let waited = start.elapsed();
        let decide = |free| step(free, need, waited, max_wait, recheck);
        let check = match check(paths, job, call, decide) {
            Ok(check) => check,
            Err(e) => break Err(e),
        };
        match check.step {
            Step::Run => break Ok(None),
            Step::RunOverdue => break Ok(Some(check.free_mb)),
            Step::Wait(timeout) => {
                if waiting.is_none() {
                    let _ = writeln!(out, "{}", waiting_notice(call, cfg, &check));
                    let record = Waiting {
                        job: job.name.clone(),
                        group: job.group.clone(),
                        label: call.label.clone(),
                        peak_mb: call.peak_mb,
                        need_mb: need,
                        since_ms: u64::try_from(runtime::now_ms()).unwrap_or(u64::MAX),
                    };
                    waiting = Some(Recorded::write(&paths.runtime, &record));
                }
                sleep_until_change(&check.holders, timeout);
            }
        }
    };
    if waiting.take().is_some() {
        let still_short = result.as_ref().ok().copied().flatten();
        let notice = running_notice(call, start.elapsed(), still_short.map(|f| (f, need)));
        let _ = writeln!(out, "{notice}");
    }
    result.map(|_| ())
}

/// The record of a waiting call, removed when the call stops waiting, the
/// prefix included when it fails. A record that cannot be written only goes
/// unseen.
struct Recorded(Option<PathBuf>);

impl Recorded {
    fn write(runtime: &Path, record: &Waiting) -> Recorded {
        let dir = waiting_dir(runtime);
        let path = dir.join(format!("{}.json", record.job));
        let written = fs::create_dir_all(&dir)
            .ok()
            .and_then(|()| serde_json::to_vec(record).ok())
            .and_then(|json| fs::write(&path, json).ok());
        Recorded(written.map(|()| path))
    }
}

impl Drop for Recorded {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// Counts free memory net of reservations, removing those that hold nothing,
/// and reserves the call's expected peak for `job` when `decide` lets it run.
/// All under the lock.
fn check(
    paths: &Paths,
    job: &Job,
    call: &Heavy,
    decide: impl FnOnce(u64) -> Step,
) -> Result<Check> {
    let dir = reservations_dir(&paths.runtime);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let _lock = lock(&paths.runtime.join("admission.lock"), LOCK_PATIENCE)?;
    let mut held = 0;
    let mut holders = Vec::new();
    let entries = fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
    for path in entries.flatten().map(|e| e.path()) {
        if let Some(r) = reserved(&paths.cgroup_root, &path) {
            held = r.held_mb.saturating_add(held);
            holders.push(paths.cgroup_root.join(r.group.trim_start_matches('/')));
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    let free = free_mb(memory::available_mb(&paths.meminfo)?, held);
    let step = decide(free);
    if matches!(step, Step::Run | Step::RunOverdue) {
        let path = dir.join(format!("{}.json", job.name));
        let reservation = Reservation {
            group: job.group.clone(),
            peak_mb: call.peak_mb,
            label: call.label.clone(),
        };
        let json = serde_json::to_vec(&reservation).context("serializing a reservation")?;
        fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(Check {
        step,
        free_mb: free,
        held_mb: held,
        holders,
    })
}

/// Where reservations are kept.
pub fn reservations_dir(runtime: &Path) -> PathBuf {
    runtime.join("reservations")
}

/// Where waiting calls are recorded.
pub fn waiting_dir(runtime: &Path) -> PathBuf {
    runtime.join("waiting")
}

/// The call a reservation file names, with what it still holds. None when it
/// holds nothing: the file cannot be read, or its group is gone or empty.
fn reserved(cgroup_root: &Path, file: &Path) -> Option<Reserved> {
    let r: Reservation = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    let group = cgroup_root.join(r.group.trim_start_matches('/'));
    if !populated(&group) {
        return None;
    }
    let current_mb = current_mb(&group);
    Some(Reserved {
        job: file.file_stem()?.to_string_lossy().into_owned(),
        // Without a memory controller, count the whole peak.
        held_mb: current_mb.map_or(r.peak_mb, |current| held_mb(r.peak_mb, current)),
        group: r.group,
        label: r.label,
        peak_mb: r.peak_mb,
        current_mb,
    })
}

/// The calls waiting for memory now, oldest first. A record whose group is
/// gone or empty belongs to a call that ended while it waited, its prefix
/// killed: with `prune`, it is removed.
pub fn waiting(paths: &Paths, prune: bool) -> Vec<Waiting> {
    let Ok(entries) = fs::read_dir(waiting_dir(&paths.runtime)) else {
        return Vec::new();
    };
    let mut waiting = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        let record = fs::read(&path)
            .ok()
            .and_then(|json| serde_json::from_slice::<Waiting>(&json).ok());
        // A record being written reads as unparsable for an instant.
        let Some(w) = record else { continue };
        if populated(&paths.cgroup_root.join(w.group.trim_start_matches('/'))) {
            waiting.push(w);
        } else if prune {
            let _ = fs::remove_file(&path);
        }
    }
    waiting.sort_by(|a, b| a.since_ms.cmp(&b.since_ms).then_with(|| a.job.cmp(&b.job)));
    waiting
}

/// The waiting calls, the reservations and the memory free for admission.
pub fn snapshot(paths: &Paths) -> Result<Snapshot> {
    let mut reserved_calls = Vec::new();
    if let Ok(entries) = fs::read_dir(reservations_dir(&paths.runtime)) {
        for path in entries.flatten().map(|e| e.path()) {
            reserved_calls.extend(reserved(&paths.cgroup_root, &path));
        }
    }
    reserved_calls.sort_by(|a, b| b.held_mb.cmp(&a.held_mb).then_with(|| a.job.cmp(&b.job)));
    Ok(Snapshot {
        available_mb: memory::available_mb(&paths.meminfo)?,
        held_mb: reserved_calls
            .iter()
            .fold(0, |sum, r| sum.saturating_add(r.held_mb)),
        waiting: waiting(paths, false),
        reserved: reserved_calls,
    })
}

fn populated(group: &Path) -> bool {
    fs::read_to_string(group.join("cgroup.events"))
        .is_ok_and(|e| e.lines().any(|l| l == "populated 1"))
}

fn current_mb(group: &Path) -> Option<u64> {
    let bytes: u64 = fs::read_to_string(group.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(bytes / (1024 * 1024))
}

/// Takes the lock file at `path`, trying for `patience`. It is released when
/// the returned file closes, at the latest when the process ends.
pub fn lock(path: &Path, patience: Duration) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let start = Instant::now();
    loop {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(file),
            Err(Errno::WOULDBLOCK) if start.elapsed() < patience => {
                std::thread::sleep(LOCK_RETRY);
            }
            Err(Errno::WOULDBLOCK) => bail!("{} stayed locked for {patience:?}", path.display()),
            Err(e) => return Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }
}

/// Sleeps until a job group of `groups` reports a change, normally its end,
/// or `timeout` runs out.
fn sleep_until_change(groups: &[PathBuf], timeout: Duration) {
    if groups.is_empty() {
        return std::thread::sleep(timeout);
    }
    let Ok(inotify) = Inotify::init() else {
        return std::thread::sleep(timeout);
    };
    for group in groups {
        let events = group.join("cgroup.events");
        if inotify
            .watches()
            .add(&events, WatchMask::MODIFY | WatchMask::DELETE_SELF)
            .is_err()
        {
            // A group gone meanwhile has ended: check again at once. Any other
            // failure falls back to waiting out the timeout.
            if populated(group) {
                std::thread::sleep(timeout);
            }
            return;
        }
    }
    // A job may have ended before its watch was in place.
    if !groups.iter().all(|g| populated(g)) {
        return;
    }
    let mut fds = [PollFd::from_borrowed_fd(inotify.as_fd(), PollFlags::IN)];
    let timeout = Timespec::try_from(timeout).unwrap_or(Timespec {
        tv_sec: i64::MAX,
        tv_nsec: 0,
    });
    let _ = poll(&mut fds, Some(&timeout));
}

/// The command as notices show it: its label.
fn shown(call: &Heavy) -> String {
    if call.label.is_empty() {
        "this command".into()
    } else {
        format!("`{}`", call.label)
    }
}

fn waiting_notice(call: &Heavy, cfg: &Admission, check: &Check) -> String {
    let mut notice = format!(
        "orchestrator: waiting for memory before running {}. This call is expected to peak at {} MB; with the {} MB margin it needs {} MB free, and {} MB are free",
        shown(call),
        call.peak_mb,
        cfg.margin_mb,
        need_mb(call.peak_mb, cfg),
        check.free_mb,
    );
    if let [_, more @ ..] = check.holders.as_slice() {
        let _ = write!(
            notice,
            ", net of {} MB kept for {} heavy command{} already running",
            check.held_mb,
            1 + more.len(),
            if more.is_empty() { "" } else { "s" },
        );
    }
    let _ = write!(
        notice,
        ". It starts as soon as memory frees up, after {} s at most.",
        cfg.max_wait_secs
    );
    notice
}

/// `still_short` holds the free and needed memory of a call that runs only
/// because it waited as long as it may.
fn running_notice(call: &Heavy, waited: Duration, still_short: Option<(u64, u64)>) -> String {
    let mut notice = format!(
        "orchestrator: running {} after waiting {:.1} s",
        shown(call),
        waited.as_secs_f64()
    );
    match still_short {
        Some((free, need)) => {
            let _ = write!(
                notice,
                ", the longest admission waits; memory is still short: {free} MB free, {need} MB needed."
            );
        }
        None => notice.push_str(" for memory."),
    }
    notice
}

#[cfg(test)]
mod tests {
    use super::*;
    use learning::peaks::Measurement;
    use std::sync::{Arc, Barrier};

    const CFG: Admission = Admission {
        heavy_mb: 1000,
        margin_mb: 500,
        max_wait_secs: 30,
    };
    const SESSION: &str = "/u/orchestrator.slice/s.scope";
    const SEC: Duration = Duration::from_secs(1);

    fn k(label: &str, peak_mb: u64, alone: bool) -> Known {
        Known {
            label: label.into(),
            peak_mb,
            alone,
        }
    }

    fn h(label: &str, peak_mb: u64) -> Heavy {
        Heavy {
            label: label.into(),
            peak_mb,
        }
    }

    #[test]
    fn a_heavy_call_expects_its_largest_peak() {
        let call = [
            k("git status", 5, true),
            k("pnpm typecheck", 3000, true),
            k("tail", 3100, false),
        ];
        assert_eq!(heavy(&call, &CFG), Some(h("pnpm typecheck", 3100)));
        // Two commands carrying the same call's peak: not twice that peak.
        let call = [k("make", 3000, false), k("tail", 3000, false)];
        assert_eq!(heavy(&call, &CFG), Some(h("make", 3000)));
        assert_eq!(
            heavy(&[k("ls", 5, true), k("tail", 999, false)], &CFG),
            None
        );
        assert_eq!(heavy(&[], &CFG), None);
    }

    #[test]
    fn a_heavy_call_goes_by_a_command_measured_alone() {
        // `date` only ever ran beside the heavy python: it carries its peak.
        let call = [k("date", 405, false), k("python3", 404, true)];
        let cfg = Admission {
            heavy_mb: 200,
            ..CFG
        };
        assert_eq!(heavy(&call, &cfg), Some(h("python3", 405)));
        // A light command run alone does not name a heavy call.
        let call = [
            k("ls", 5, true),
            k("make", 3000, false),
            k("tail", 3000, false),
        ];
        assert_eq!(heavy(&call, &CFG), Some(h("make", 3000)));
    }

    #[test]
    fn heavy_from_the_threshold_up() {
        assert!(!is_heavy(999, &CFG));
        assert!(is_heavy(1000, &CFG));
        assert!(is_heavy(5000, &CFG));
    }

    #[test]
    fn free_memory_is_net_of_what_reservations_hold() {
        // A reservation shrinks as its job climbs to its peak.
        assert_eq!(held_mb(3000, 0), 3000);
        assert_eq!(held_mb(3000, 1200), 1800);
        assert_eq!(held_mb(3000, 3500), 0);
        assert_eq!(free_mb(8000, 1800 + 3000), 3200);
        assert_eq!(free_mb(2000, 3000), 0);
        assert_eq!(need_mb(3000, &CFG), 3500);
        assert_eq!(need_mb(u64::MAX, &CFG), u64::MAX);
    }

    #[test]
    fn a_call_runs_when_it_fits_else_waits_up_to_the_longest_wait() {
        let max = 30 * SEC;
        assert_eq!(step(3500, 3500, Duration::ZERO, max, SEC), Step::Run);
        assert_eq!(step(3499, 3500, Duration::ZERO, max, SEC), Step::Wait(SEC));
        // The last wait ends at the longest wait.
        let late = max.saturating_sub(Duration::from_millis(300));
        assert_eq!(
            step(0, 3500, late, max, SEC),
            Step::Wait(Duration::from_millis(300))
        );
        assert_eq!(step(0, 3500, max, max, SEC), Step::RunOverdue);
        // Fitting wins even when overdue.
        assert_eq!(step(4000, 3500, 2 * max, max, SEC), Step::Run);
        // No wait at all.
        assert_eq!(
            step(0, 3500, Duration::ZERO, Duration::ZERO, SEC),
            Step::RunOverdue
        );
    }

    /// A cgroup tree, a meminfo and a runtime directory, all temporary.
    struct Machine {
        _tmp: tempfile::TempDir,
        paths: Paths,
    }

    impl Machine {
        fn new(available_mb: u64) -> Machine {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths {
                cgroup_root: tmp.path().join("cgroup"),
                meminfo: tmp.path().join("meminfo"),
                runtime: tmp.path().join("runtime"),
            };
            let m = Machine { _tmp: tmp, paths };
            m.set_available(available_mb);
            m
        }

        fn set_available(&self, mb: u64) {
            let text = format!("MemTotal: 32000000 kB\nMemAvailable: {} kB\n", mb * 1024);
            fs::write(&self.paths.meminfo, text).unwrap();
        }

        /// A running job group using `current_mb`.
        fn job(&self, name: &str, current_mb: u64) -> Job {
            let job = Job {
                group: format!("{SESSION}/{name}"),
                name: name.into(),
            };
            let dir = self.dir(&job.group);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
            self.set_current(&job.group, current_mb);
            job
        }

        fn set_current(&self, group: &str, mb: u64) {
            let bytes = mb * 1024 * 1024;
            fs::write(self.dir(group).join("memory.current"), format!("{bytes}\n")).unwrap();
        }

        fn end(&self, group: &str) {
            fs::write(
                self.dir(group).join("cgroup.events"),
                "populated 0\nfrozen 0\n",
            )
            .unwrap();
        }

        fn dir(&self, group: &str) -> PathBuf {
            self.paths.cgroup_root.join(group.trim_start_matches('/'))
        }

        /// One check for a call of `peak_mb`, run as `name`, that may not
        /// wait.
        fn try_admit(&self, name: &str, peak_mb: u64) -> Check {
            let job = self.job(name, 0);
            let need = need_mb(peak_mb, &CFG);
            check(&self.paths, &job, &h("make", peak_mb), |free| {
                step(free, need, Duration::ZERO, 30 * SEC, SEC)
            })
            .unwrap()
        }

        fn reserved(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(reservations_dir(&self.paths.runtime))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    #[test]
    fn heavy_calls_run_together_while_memory_holds_them() {
        let m = Machine::new(9000);
        // Each needs 3500 MB free and reserves 3000.
        assert_eq!(m.try_admit("job-bash-1-1", 3000).step, Step::Run);
        let second = m.try_admit("job-bash-2-1", 3000);
        assert_eq!((second.step, second.free_mb), (Step::Run, 6000));
        let third = m.try_admit("job-bash-3-1", 3000);
        assert_eq!(third.step, Step::Wait(SEC));
        assert_eq!((third.free_mb, third.held_mb), (3000, 6000));
        assert_eq!(third.holders.len(), 2);
        assert_eq!(m.reserved(), ["job-bash-1-1.json", "job-bash-2-1.json"]);
    }

    #[test]
    fn a_reservation_shrinks_as_its_job_climbs() {
        let m = Machine::new(9000);
        let first = format!("{SESSION}/job-bash-1-1");
        assert_eq!(m.try_admit("job-bash-1-1", 6000).step, Step::Run);
        let held = |current_mb, available_mb| {
            m.set_current(&first, current_mb);
            m.set_available(available_mb);
            let again = m.try_admit("job-bash-2-1", 3000);
            assert_eq!(again.step, Step::Wait(SEC));
            (again.held_mb, again.free_mb)
        };
        assert_eq!(held(0, 9000), (6000, 3000));
        // The first job now uses 5000 MB, which the kernel no longer counts as
        // available: it still holds 1000.
        assert_eq!(held(5000, 4000), (1000, 3000));
        // Past its expected peak, it holds nothing.
        assert_eq!(held(6500, 2500), (0, 2500));
    }

    #[test]
    fn ended_gone_and_unreadable_reservations_hold_nothing_and_are_removed() {
        let m = Machine::new(4000);
        assert_eq!(m.try_admit("job-bash-1-1", 3000).step, Step::Run);
        assert_eq!(m.try_admit("job-bash-2-1", 3000).step, Step::Wait(SEC));
        m.end(&format!("{SESSION}/job-bash-1-1"));
        assert_eq!(m.try_admit("job-bash-2-1", 3000).step, Step::Run);
        assert_eq!(m.reserved(), ["job-bash-2-1.json"]);
        fs::remove_dir_all(m.dir(&format!("{SESSION}/job-bash-2-1"))).unwrap();
        let dir = reservations_dir(&m.paths.runtime);
        fs::write(dir.join("job-bash-9-1.json"), "{not json").unwrap();
        assert_eq!(m.try_admit("job-bash-3-1", 3000).step, Step::Run);
        assert_eq!(m.reserved(), ["job-bash-3-1.json"]);
    }

    #[test]
    fn concurrent_checks_never_count_on_the_same_memory() {
        let m = Arc::new(Machine::new(10_000));
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let (m, barrier) = (Arc::clone(&m), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    let job = m.job(&format!("job-bash-{i}-1"), 0);
                    barrier.wait();
                    let need = need_mb(2000, &CFG);
                    check(&m.paths, &job, &h("make", 2000), |free| {
                        step(free, need, Duration::ZERO, 30 * SEC, SEC)
                    })
                    .unwrap()
                    .step
                })
            })
            .collect();
        let ran = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|s| *s == Step::Run)
            .count();
        // 10000 MB hold four calls reserving 2000 and needing 2500.
        assert_eq!(ran, 4);
        assert_eq!(m.reserved().len(), 4);
    }

    #[test]
    fn a_stuck_lock_gives_up() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("admission.lock");
        let held = lock(&path, Duration::ZERO).unwrap();
        let start = Instant::now();
        assert!(lock(&path, Duration::from_millis(50)).is_err());
        assert!(start.elapsed() >= Duration::from_millis(50));
        drop(held);
        // A process another test starts at this instant holds a copy of the
        // descriptor until it execs, and the lock with it: allow for that.
        assert!(lock(&path, Duration::from_secs(1)).is_ok());
    }

    fn typecheck(peak_mb: u64) -> Heavy {
        h("pnpm typecheck", peak_mb)
    }

    #[test]
    fn a_waiting_call_runs_as_soon_as_a_holder_ends() {
        let m = Machine::new(4000);
        assert_eq!(m.try_admit("job-bash-1-1", 3000).step, Step::Run);
        let first = format!("{SESSION}/job-bash-1-1");
        let job = m.job("job-bash-2-1", 0);
        let ender = {
            let dir = m.dir(&first);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                fs::write(dir.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
            })
        };
        let start = Instant::now();
        let mut out = Vec::new();
        // Checking only every 60 s, the wake comes from inotify.
        admit_every(&m.paths, &CFG, &job, &typecheck(3000), 60 * SEC, &mut out).unwrap();
        ender.join().unwrap();
        assert!(start.elapsed() < 10 * SEC, "{:?}", start.elapsed());
        let out = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(lines[0].starts_with(
            "orchestrator: waiting for memory before running `pnpm typecheck`. This call is expected to peak at 3000 MB; with the 500 MB margin it needs 3500 MB free, and 1000 MB are free, net of 3000 MB kept for 1 heavy command already running."
        ), "{}", lines[0]);
        assert!(lines[0].ends_with("after 30 s at most."), "{}", lines[0]);
        assert!(lines[1].starts_with("orchestrator: running `pnpm typecheck` after waiting "));
        assert!(lines[1].ends_with(" s for memory."), "{}", lines[1]);
        assert_eq!(m.reserved(), ["job-bash-2-1.json"]);
    }

    #[test]
    fn a_call_is_recorded_while_it_waits() {
        let m = Arc::new(Machine::new(4000));
        assert_eq!(m.try_admit("job-bash-1-1", 3000).step, Step::Run);
        let first = format!("{SESSION}/job-bash-1-1");
        m.set_current(&first, 1000);
        let job = m.job("job-bash-2-1", 0);
        let waiter = {
            let m = Arc::clone(&m);
            let job = job.clone();
            std::thread::spawn(move || {
                let mut out = Vec::new();
                admit_every(&m.paths, &CFG, &job, &typecheck(3000), 60 * SEC, &mut out).unwrap();
            })
        };
        let start = Instant::now();
        let seen = loop {
            let found = waiting(&m.paths, false);
            if !found.is_empty() || start.elapsed() > 5 * SEC {
                break found;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(seen.len(), 1, "{seen:?}");
        let w = &seen[0];
        assert_eq!(
            (w.job.as_str(), w.group.as_str(), w.label.as_str()),
            ("job-bash-2-1", job.group.as_str(), "pnpm typecheck")
        );
        assert_eq!((w.peak_mb, w.need_mb), (3000, 3500));
        let snap = snapshot(&m.paths).unwrap();
        assert_eq!(snap.waiting, seen);
        assert_eq!(
            snap.reserved,
            [Reserved {
                job: "job-bash-1-1".into(),
                group: first.clone(),
                label: "make".into(),
                peak_mb: 3000,
                current_mb: Some(1000),
                held_mb: 2000,
            }]
        );
        assert_eq!((snap.held_mb, snap.free_mb()), (2000, 2000));
        m.end(&first);
        waiter.join().unwrap();
        assert_eq!(waiting(&m.paths, false), []);
        let snap = snapshot(&m.paths).unwrap();
        assert_eq!(snap.reserved.len(), 1);
        assert_eq!(snap.reserved[0].label, "pnpm typecheck");
    }

    #[test]
    fn a_record_left_by_a_killed_prefix_is_pruned() {
        let m = Machine::new(4000);
        let job = m.job("job-bash-1-1", 0);
        let record = Waiting {
            job: job.name.clone(),
            group: job.group.clone(),
            label: "make".into(),
            peak_mb: 3000,
            need_mb: 3500,
            since_ms: 1,
        };
        let kept = Recorded::write(&m.paths.runtime, &record);
        assert_eq!(waiting(&m.paths, true), [record]);
        std::mem::forget(kept);
        m.end(&job.group);
        assert_eq!(waiting(&m.paths, false), []);
        let dir = waiting_dir(&m.paths.runtime);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(waiting(&m.paths, true), []);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn after_the_longest_wait_a_call_runs_anyway_and_reserves() {
        let m = Machine::new(1000);
        let job = m.job("job-bash-1-1", 0);
        let cfg = Admission {
            max_wait_secs: 1,
            ..CFG
        };
        let mut out = Vec::new();
        let start = Instant::now();
        admit_every(
            &m.paths,
            &cfg,
            &job,
            &typecheck(3000),
            Duration::from_millis(100),
            &mut out,
        )
        .unwrap();
        assert!(start.elapsed() >= SEC);
        let out = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(
            lines[0].contains("and 1000 MB are free. It starts"),
            "{}",
            lines[0]
        );
        assert!(lines[1].ends_with(
            ", the longest admission waits; memory is still short: 1000 MB free, 3500 MB needed."
        ), "{}", lines[1]);
        assert_eq!(m.reserved(), ["job-bash-1-1.json"]);
    }

    #[test]
    fn a_call_that_fits_says_nothing() {
        let m = Machine::new(8000);
        let job = m.job("job-bash-1-1", 0);
        let mut out = Vec::new();
        admit(&m.paths, &CFG, &job, &typecheck(3000), &mut out).unwrap();
        assert_eq!(out, b"");
        assert_eq!(m.reserved(), ["job-bash-1-1.json"]);
    }

    #[test]
    fn without_meminfo_the_call_runs_unreserved() {
        let m = Machine::new(8000);
        fs::remove_file(&m.paths.meminfo).unwrap();
        let job = m.job("job-bash-1-1", 0);
        let mut out = Vec::new();
        assert!(admit(&m.paths, &CFG, &job, &typecheck(3000), &mut out).is_err());
        assert_eq!(out, b"");
        assert_eq!(m.reserved(), Vec::<String>::new());
    }

    /// A Bash call Claude wrote as `script`, as Claude Code hands it over.
    fn invocation(script: &str) -> String {
        let quoted = format!("'{}'", script.replace('\'', r#"'"'"'"#));
        format!(
            "source /home/u/.claude/shell-snapshots/snapshot-bash-1-x.sh 2>/dev/null || true && eval {quoted} && pwd -P >| /tmp/claude-ab12-cwd"
        )
    }

    struct Store {
        _tmp: tempfile::TempDir,
        peaks: PathBuf,
        repo: PathBuf,
    }

    fn store(calls: &[(&str, u64)]) -> Store {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("app");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("web")).unwrap();
        let peaks = root.join("state/peaks");
        for (i, (script, peak_mb)) in calls.iter().enumerate() {
            let m = Measurement {
                at: 1000 + u64::try_from(i).unwrap(),
                session: "s.scope".into(),
                job: "job-bash-7-1".into(),
                peak_mb: *peak_mb,
                command: invocation(script),
                cwd: repo.display().to_string(),
            };
            peaks::learn(&peaks, &m, None).unwrap();
        }
        Store {
            _tmp: tmp,
            peaks,
            repo,
        }
    }

    #[test]
    fn looks_a_call_s_commands_up() {
        let s = store(&[
            ("pnpm typecheck", 3000),
            ("cat build.log | tail -1", 4),
            ("pnpm lint 2>&1 | tail -1", 1200),
        ]);
        let look = |script: &str| known(&invocation(script), &s.repo, None, &s.peaks);
        assert_eq!(
            look("timeout 300 pnpm typecheck 2>&1 | tail -1; git status"),
            [k("pnpm typecheck", 3000, true), k("tail -1", 4, false)]
        );
        // `pnpm lint` only ever ran beside `tail -1`, which a light call
        // showed is light: the lint carries 1200 MB.
        assert_eq!(look("pnpm lint"), [k("pnpm lint", 1200, false)]);
        assert_eq!(
            look("cd web && pnpm typecheck"),
            [k("pnpm typecheck", 3000, true)]
        );
        assert_eq!(look("tail -1 x"), []);
        assert_eq!(look("echo 'open"), []);
        assert_eq!(look("cd \"$D\" && pnpm typecheck"), []);
        // Never seen in a light call, `tail -1` carries the peak of the build
        // it followed; the call still goes by the build.
        let s = store(&[("pnpm build 2>&1 | tail -1", 2000)]);
        let look = |script: &str| known(&invocation(script), &s.repo, None, &s.peaks);
        let call = look("pnpm build | tail -1");
        assert_eq!(
            call,
            [k("pnpm build", 2000, false), k("tail -1", 2000, false)]
        );
        assert_eq!(heavy(&call, &CFG), Some(h("pnpm build", 2000)));
    }

    #[test]
    fn notices_name_the_call_by_its_label() {
        let s = store(&[
            (
                "python3 -c 'b = bytearray(1500 << 20)' 2>&1 | tail -1",
                1500,
            ),
            ("cat build.log | tail -1", 4),
        ]);
        let script = "timeout 60 python3 -c 'b = bytearray(1500 << 20)' | tail -1";
        let call = known(&invocation(script), &s.repo, None, &s.peaks);
        let call = heavy(&call, &CFG).unwrap();
        let check = Check {
            step: Step::Wait(SEC),
            free_mb: 10,
            held_mb: 0,
            holders: Vec::new(),
        };
        let notices = [
            waiting_notice(&call, &CFG, &check),
            running_notice(&call, SEC, None),
            running_notice(&call, SEC, Some((10, 2000))),
        ];
        for n in notices {
            assert!(n.contains("`python3 -c b = bytearray(1500 << 20)`"), "{n}");
        }
    }
}
