//! Admission: whether a Bash call starts at once or waits for memory. When
//! the prefix decides, it has only the command line, which says nothing
//! about memory: `pnpm typecheck` itself uses little, the `tsc` processes it
//! starts use gigabytes. Admission therefore learns from what it measures,
//! with no list of commands to maintain.
//!
//! The prefix looks the call's commands up in the learned peaks. The call's
//! expected peak is the largest of its commands', never their sum: a call has
//! a single peak, which bounds each of its commands, so a filter such as
//! `tail -1` learned only beside a heavy command carries that same peak. For
//! the same reason the notices name the call after its first heavy command
//! that has run alone, whose peak was measured, else after its first heavy
//! command. A call whose expected peak is unknown or under the threshold
//! starts at once and reserves nothing. A heavy call waits until free memory
//! covers its expected peak plus a margin, for a bounded time; past it, the
//! call is refused rather than run short of memory, which would only move
//! the shortage onto every session.
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
//! notification, nor a change of the calls ahead of it. It writes one notice
//! to standard error when it starts
//! waiting and one when it runs or is refused: the call's output goes back
//! to Claude.
//!
//! A call waits `max_wait_secs` at most, under a Bash call's default
//! timeout: past the timeout, Claude Code moves the call to the background
//! and its result holds none of its output, so a refusal would reach Claude
//! only through the output file. Past that wait the call is refused, exit
//! code `REFUSED`, its notice saying how to wait longer: run it again in the
//! background with `ORCHESTRATOR_BACKGROUND=wait-for-memory` before the
//! command. A call carrying `ORCHESTRATOR_BACKGROUND` waits
//! `max_background_wait_secs` at most, then is refused too. The prefix cannot
//! see whether Claude Code runs a call in the background, so Claude says so
//! in the command; the value states the purpose because Claude Code's auto
//! mode classifier sees the command and never the refusal, and denied
//! relaunches carrying `=1` (#16).
//!
//! Waiting calls are served in order: those given priority, in the order
//! given, then the others by arrival (#21). Each check hands free memory out
//! in that order. A call ahead that fits takes its expected peak, as its
//! reservation will once it runs; one that does not fit is passed, so that a
//! call never waits for a larger one it could run beside, unless that one
//! has priority: no call passes a call given priority. A call runs when what
//! the calls ahead leave covers its need. Priority is a judgment, left to
//! the coordinator: `orchestrator admission priority` replaces the list,
//! kept as `<runtime>/priority.json`, and a call leaves it once it runs or
//! is refused. The default order needs none.
//!
//! A waiting call is recorded in the runtime directory from its first wait,
//! under the lock, so that every later check counts it ahead, and so that
//! `orchestrator admission` can show it and `watch` can wake the coordinator
//! when it waits long. Reading the waiting calls and the reservations
//! outside a check takes no lock: what a reader shows may be a check behind.
//!
//! There is no fixed number of slots: two heavy calls run together when
//! memory holds both. systemd's own slots (`ConcurrencySoftMax`) work on
//! units, and moving a job into one would take it out of its session's
//! group. The margin has to exceed how much `MemAvailable` drifts while
//! other sessions work: a few hundred MB within seconds.
//!
//! A heavy call that leaves
//! a process running, such as a server started in the background, keeps
//! its reservation, net of what its group uses, until that process ends.
//! What admission cannot foresee (a first run, a form of the command it
//! does not recognise) starts at once; if memory then runs short, `watch`
//! reports the pressure.

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

/// The variable a call carries to wait in the background, the longer wait.
pub const BACKGROUND_VAR: &str = "ORCHESTRATOR_BACKGROUND";
/// The value the refusal tells Claude to give it: why the call runs in the
/// background, in the command, where Claude Code's classifier reads it.
pub const BACKGROUND_VALUE: &str = "wait-for-memory";
/// The exit code of a refused call: a temporary failure, worth trying again
/// later (`EX_TEMPFAIL`).
pub const REFUSED: i32 = 75;

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
/// from its first wait until it runs or is refused.
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
    /// In the order memory goes to them.
    pub waiting: Vec<Waiting>,
    /// How many of the first `waiting` have priority.
    pub priority: usize,
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
    /// The call has waited as long as it may: it is refused.
    Refuse,
    /// The call waits, at most this long before the next check.
    Wait(Duration),
}

/// What admission decides for a heavy call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It runs, its peak reserved.
    Run,
    /// It does not run: the prefix exits with `REFUSED`.
    Refused,
}

/// What the calls waiting ahead of a call leave it of free memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ahead {
    /// The `calls` of them that fit take `mb` first; none may.
    Take { calls: usize, mb: u64 },
    /// One of them has priority and does not fit: no call passes it.
    Priority,
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
    ahead: Ahead,
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
        Step::Refuse
    } else {
        Step::Wait(max_wait.saturating_sub(waited).min(recheck))
    }
}

/// Sorts `waiting` in the order memory goes to the calls: those in
/// `priority`, in its order, then the others by arrival. Returns how many
/// come first by priority.
pub fn in_order(waiting: &mut [Waiting], priority: &[String]) -> usize {
    let rank = |w: &Waiting| {
        priority
            .iter()
            .position(|job| *job == w.job)
            .unwrap_or(usize::MAX)
    };
    waiting.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then(a.since_ms.cmp(&b.since_ms))
            .then_with(|| a.job.cmp(&b.job))
    });
    waiting.iter().take_while(|w| rank(w) != usize::MAX).count()
}

/// What `free_mb` leaves a call once the calls `ahead` of it, in order, the
/// first `priority` of them given priority, have taken theirs. One that fits
/// takes its expected peak, as its reservation will once it runs; one that
/// does not is passed, unless it has priority.
pub fn share(free_mb: u64, ahead: &[Waiting], priority: usize) -> (u64, Ahead) {
    let mut left = free_mb;
    let mut calls = 0;
    for (i, w) in ahead.iter().enumerate() {
        if w.need_mb <= left {
            left = left.saturating_sub(w.peak_mb);
            calls += 1;
        } else if i < priority {
            return (0, Ahead::Priority);
        }
    }
    let mb = free_mb.saturating_sub(left);
    (left, Ahead::Take { calls, mb })
}

/// Whether Claude runs the Bash call `invocation` in the background to wait
/// longer: the command it wrote assigns `BACKGROUND_VAR`, whatever the
/// value. A mention that assigns nothing, in an `echo` for instance, counts
/// too; it only lengthens the wait.
pub fn waits_in_background(invocation: &str) -> bool {
    claude_code::invocation::written(invocation)
        .is_some_and(|script| script.contains(&format!("{BACKGROUND_VAR}=")))
}

/// The longest the call may wait, in seconds.
fn longest_wait_secs(cfg: &Admission, background: bool) -> u64 {
    if background {
        cfg.max_background_wait_secs
    } else {
        cfg.max_wait_secs
    }
}

/// Lets the heavy `call` run in `job` once free memory covers it, reserving
/// its peak, or refuses it once it has waited its longest wait: the
/// background one when Claude runs it in the background. Notices go to
/// `out`. On an error the call runs at once, unreserved.
pub fn admit(
    paths: &Paths,
    cfg: &Admission,
    job: &Job,
    call: &Heavy,
    background: bool,
    out: &mut dyn Write,
) -> Result<Outcome> {
    admit_every(paths, cfg, job, call, background, RECHECK, out)
}

fn admit_every(
    paths: &Paths,
    cfg: &Admission,
    job: &Job,
    call: &Heavy,
    background: bool,
    recheck: Duration,
    out: &mut dyn Write,
) -> Result<Outcome> {
    let start = Instant::now();
    let me = Waiting {
        job: job.name.clone(),
        group: job.group.clone(),
        label: call.label.clone(),
        peak_mb: call.peak_mb,
        need_mb: need_mb(call.peak_mb, cfg),
        since_ms: u64::try_from(runtime::now_ms()).unwrap_or(u64::MAX),
    };
    let longest = longest_wait_secs(cfg, background);
    let max_wait = Duration::from_secs(longest);
    let mut recorded: Option<Recorded> = None;
    let result = loop {
        let waited = start.elapsed();
        let decide = |room| step(room, me.need_mb, waited, max_wait, recheck);
        let check = match check(paths, &me, decide) {
            Ok(check) => check,
            Err(e) => break Err(e),
        };
        match check.step {
            Step::Run => break Ok((Outcome::Run, check)),
            Step::Refuse => break Ok((Outcome::Refused, check)),
            Step::Wait(timeout) => {
                if recorded.is_none() {
                    let _ = writeln!(out, "{}", waiting_notice(call, cfg, &check, longest));
                    recorded = Some(Recorded(record_path(&paths.runtime, &me.job)));
                }
                sleep_until_change(&check.holders, timeout);
            }
        }
    };
    let waited = recorded.take().is_some();
    match &result {
        Ok((Outcome::Run, _)) if waited => {
            let _ = writeln!(out, "{}", running_notice(call, start.elapsed()));
        }
        Ok((Outcome::Refused, check)) => {
            let notice = refused_notice(call, cfg, background, check, me.need_mb);
            let _ = writeln!(out, "{notice}");
        }
        _ => {}
    }
    result.map(|(outcome, _)| outcome)
}

/// The record of a waiting call, which the check that runs or refuses it
/// removes; removed when it drops too, for a prefix that fails while it
/// waits.
struct Recorded(PathBuf);

impl Drop for Recorded {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn record_path(runtime: &Path, job: &str) -> PathBuf {
    waiting_dir(runtime).join(format!("{job}.json"))
}

/// Records `call` as waiting. A record that cannot be written goes unseen,
/// and later calls do not count it ahead.
fn record(runtime: &Path, call: &Waiting) {
    let dir = waiting_dir(runtime);
    let _ = fs::create_dir_all(&dir)
        .ok()
        .and_then(|()| serde_json::to_vec(call).ok())
        .map(|json| fs::write(record_path(runtime, &call.job), json));
}

/// Counts free memory net of reservations, removing those that hold nothing,
/// and what the calls waiting ahead of `me` leave of it; `decide` is given
/// what is left. Reserves the call's expected peak when it runs, records it
/// when it first waits, and removes its record when it stops. All under the
/// lock.
fn check(paths: &Paths, me: &Waiting, decide: impl FnOnce(u64) -> Step) -> Result<Check> {
    let dir = reservations_dir(&paths.runtime);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let _lock = lock(&lock_path(&paths.runtime), LOCK_PATIENCE)?;
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
    let mut queue = waiting(paths, true);
    let recorded = queue.iter().any(|w| w.job == me.job);
    if !recorded {
        queue.push(me.clone());
    }
    let priority = read_priority(&paths.runtime);
    let first = in_order(&mut queue, &priority);
    let at = queue
        .iter()
        .position(|w| w.job == me.job)
        .unwrap_or(queue.len());
    let (room, ahead) = share(free, &queue[..at], first.min(at));
    let step = decide(room);
    match step {
        Step::Run => {
            let path = dir.join(format!("{}.json", me.job));
            let reservation = Reservation {
                group: me.group.clone(),
                peak_mb: me.peak_mb,
                label: me.label.clone(),
            };
            let json = serde_json::to_vec(&reservation).context("serializing a reservation")?;
            fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        }
        Step::Wait(_) if !recorded => record(&paths.runtime, me),
        Step::Wait(_) | Step::Refuse => {}
    }
    let stops = !matches!(step, Step::Wait(_));
    if stops && recorded {
        let _ = fs::remove_file(record_path(&paths.runtime, &me.job));
    }
    // The list keeps the calls still waiting.
    let kept: Vec<&str> = priority
        .iter()
        .map(String::as_str)
        .filter(|job| queue.iter().any(|w| w.job == *job) && !(stops && *job == me.job))
        .collect();
    if kept.len() != priority.len() {
        let _ = write_priority(&paths.runtime, &kept);
    }
    Ok(Check {
        step,
        free_mb: free,
        held_mb: held,
        holders,
        ahead,
    })
}

fn lock_path(runtime: &Path) -> PathBuf {
    runtime.join("admission.lock")
}

/// Where the calls given priority are listed, in their order.
fn priority_path(runtime: &Path) -> PathBuf {
    runtime.join("priority.json")
}

/// The jobs given priority, in their order; none when the list cannot be
/// read.
fn read_priority(runtime: &Path) -> Vec<String> {
    fs::read(priority_path(runtime))
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default()
}

fn write_priority(runtime: &Path, jobs: &[&str]) -> Result<()> {
    let path = priority_path(runtime);
    let json = serde_json::to_vec(jobs).context("serializing the priority list")?;
    fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

/// Gives the waiting calls `jobs` priority, in this order, in place of those
/// that had it: none when `jobs` is empty. A job named twice keeps its first
/// place. Refuses, changing nothing, when one of them does not wait. Returns
/// the calls given priority.
pub fn set_priority(paths: &Paths, jobs: &[String]) -> Result<Vec<Waiting>> {
    fs::create_dir_all(&paths.runtime)
        .with_context(|| format!("creating {}", paths.runtime.display()))?;
    let _lock = lock(&lock_path(&paths.runtime), LOCK_PATIENCE)?;
    let waiting = waiting(paths, true);
    let mut given: Vec<Waiting> = Vec::new();
    for job in jobs {
        if given.iter().any(|w| w.job == *job) {
            continue;
        }
        let Some(w) = waiting.iter().find(|w| w.job == *job) else {
            bail!(
                "{job} does not wait for memory: `orchestrator admission` lists the calls that do"
            );
        };
        given.push(w.clone());
    }
    let names: Vec<&str> = given.iter().map(|w| w.job.as_str()).collect();
    write_priority(&paths.runtime, &names)?;
    Ok(given)
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
    let mut waiting = waiting(paths, false);
    let priority = in_order(&mut waiting, &read_priority(&paths.runtime));
    Ok(Snapshot {
        available_mb: memory::available_mb(&paths.meminfo)?,
        held_mb: reserved_calls
            .iter()
            .fold(0, |sum, r| sum.saturating_add(r.held_mb)),
        waiting,
        priority,
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

/// A wait of `secs` as notices show it: in minutes when it is whole minutes.
fn span(secs: u64) -> String {
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// `longest` is the longest the call may wait, in seconds.
fn waiting_notice(call: &Heavy, cfg: &Admission, check: &Check, longest: u64) -> String {
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
        "{}. It starts as soon as memory frees up; past {}, it is refused.",
        ahead_shown(check.ahead),
        span(longest)
    );
    notice
}

/// What the calls waiting ahead take of the free memory, as notices show it
/// after the memory free: nothing when they take none.
fn ahead_shown(ahead: Ahead) -> String {
    match ahead {
        Ahead::Take { calls: 0, .. } => String::new(),
        Ahead::Take { calls, mb } => format!(
            "; {mb} MB of them go first to {calls} call{} waiting ahead of it",
            if calls == 1 { "" } else { "s" }
        ),
        Ahead::Priority => {
            "; a call given priority waits ahead of it, and no call passes it".into()
        }
    }
}

fn running_notice(call: &Heavy, waited: Duration) -> String {
    format!(
        "orchestrator: running {} after waiting {:.1} s for memory.",
        shown(call),
        waited.as_secs_f64()
    )
}

/// A call refused in the foreground learns how to wait longer: in the
/// background, where it no longer holds the conversation.
fn refused_notice(
    call: &Heavy,
    cfg: &Admission,
    background: bool,
    check: &Check,
    need: u64,
) -> String {
    let longest = longest_wait_secs(cfg, background);
    let after = if longest == 0 {
        "at once".to_string()
    } else {
        format!("after {}", span(longest))
    };
    let mut notice = format!(
        "orchestrator: refused {} {after}: {} MB are free and it needs {need} MB{}.",
        shown(call),
        check.free_mb,
        ahead_shown(check.ahead)
    );
    if !background {
        let assignment = format!("{BACKGROUND_VAR}={BACKGROUND_VALUE}");
        let _ = write!(
            notice,
            " To let it wait up to {}, {}.",
            span(cfg.max_background_wait_secs),
            claude_code::call::in_background(&assignment)
        );
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
        max_background_wait_secs: 1800,
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
        assert_eq!(step(0, 3500, max, max, SEC), Step::Refuse);
        // Fitting wins even when overdue.
        assert_eq!(step(4000, 3500, 2 * max, max, SEC), Step::Run);
        // No wait at all.
        assert_eq!(
            step(0, 3500, Duration::ZERO, Duration::ZERO, SEC),
            Step::Refuse
        );
    }

    #[test]
    fn a_call_assigning_the_variable_waits_in_the_background() {
        let bg = |script: &str| waits_in_background(&invocation(script));
        assert!(bg("ORCHESTRATOR_BACKGROUND=wait-for-memory pnpm typecheck"));
        assert!(bg(
            "cd web && ORCHESTRATOR_BACKGROUND=1 pnpm typecheck; echo $?"
        ));
        assert!(!bg("pnpm typecheck"));
        assert!(!bg("echo $ORCHESTRATOR_BACKGROUND"));
        assert!(!waits_in_background("not an invocation"));
        assert_eq!(longest_wait_secs(&CFG, false), 30);
        assert_eq!(longest_wait_secs(&CFG, true), 1800);
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

        /// A call of `peak_mb` run as `name`, arrived at `since_ms`, as a
        /// check sees it; its job group running.
        fn call(&self, name: &str, peak_mb: u64, since_ms: u64) -> Waiting {
            let job = self.job(name, 0);
            Waiting {
                job: job.name,
                group: job.group,
                label: "make".into(),
                peak_mb,
                need_mb: need_mb(peak_mb, &CFG),
                since_ms,
            }
        }

        /// One check for `call`, which may wait.
        fn check(&self, call: &Waiting) -> Check {
            check(&self.paths, call, |room| {
                step(room, call.need_mb, Duration::ZERO, 30 * SEC, SEC)
            })
            .unwrap()
        }

        /// One check for a call of `peak_mb`, run as `name`, arriving now.
        fn try_admit(&self, name: &str, peak_mb: u64) -> Check {
            let now = u64::try_from(runtime::now_ms()).unwrap();
            self.check(&self.call(name, peak_mb, now))
        }

        /// A call of `peak_mb` waiting since `since_ms`, as its first check
        /// records it.
        fn waits(&self, name: &str, peak_mb: u64, since_ms: u64) -> Waiting {
            let call = self.call(name, peak_mb, since_ms);
            record(&self.paths.runtime, &call);
            call
        }

        fn waiting(&self) -> Vec<String> {
            waiting(&self.paths, false)
                .into_iter()
                .map(|w| w.job)
                .collect()
        }

        fn priority(&self) -> Vec<String> {
            read_priority(&self.paths.runtime)
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
                    let call = m.call(&format!("job-bash-{i}-1"), 2000, i);
                    barrier.wait();
                    m.check(&call).step
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
        let outcome = admit_every(
            &m.paths,
            &CFG,
            &job,
            &typecheck(3000),
            false,
            60 * SEC,
            &mut out,
        )
        .unwrap();
        assert_eq!(outcome, Outcome::Run);
        ender.join().unwrap();
        assert!(start.elapsed() < 10 * SEC, "{:?}", start.elapsed());
        let out = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(lines[0].starts_with(
            "orchestrator: waiting for memory before running `pnpm typecheck`. This call is expected to peak at 3000 MB; with the 500 MB margin it needs 3500 MB free, and 1000 MB are free, net of 3000 MB kept for 1 heavy command already running."
        ), "{}", lines[0]);
        assert!(
            lines[0].ends_with("It starts as soon as memory frees up; past 30 s, it is refused."),
            "{}",
            lines[0]
        );
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
                let call = typecheck(3000);
                admit_every(&m.paths, &CFG, &job, &call, false, 60 * SEC, &mut out).unwrap();
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

    fn w(job: &str, peak_mb: u64, since_ms: u64) -> Waiting {
        Waiting {
            job: job.into(),
            group: format!("{SESSION}/{job}"),
            label: "make".into(),
            peak_mb,
            need_mb: need_mb(peak_mb, &CFG),
            since_ms,
        }
    }

    fn jobs(calls: &[Waiting]) -> Vec<&str> {
        calls.iter().map(|c| c.job.as_str()).collect()
    }

    fn named(jobs: &[&str]) -> Vec<String> {
        jobs.iter().map(|j| (*j).to_string()).collect()
    }

    #[test]
    fn memory_goes_to_the_calls_in_order_passing_those_that_do_not_fit() {
        let mut queue = vec![w("c", 1000, 3), w("a", 6000, 1), w("b", 2000, 2)];
        assert_eq!(in_order(&mut queue, &[]), 0);
        assert_eq!(jobs(&queue), ["a", "b", "c"]);
        // With 5000 MB free, `a` (6500 needed) is passed; `b` takes its peak.
        assert_eq!(
            share(5000, &queue[..2], 0),
            (3000, Ahead::Take { calls: 1, mb: 2000 })
        );
        assert_eq!(
            share(1000, &queue[..2], 0),
            (1000, Ahead::Take { calls: 0, mb: 0 })
        );
        // Priority first, in its order; a priority that does not fit is never
        // passed, one that fits takes its share.
        assert_eq!(in_order(&mut queue, &named(&["c", "a"])), 2);
        assert_eq!(jobs(&queue), ["c", "a", "b"]);
        assert_eq!(share(5000, &queue[..2], 2), (0, Ahead::Priority));
        assert_eq!(
            share(9000, &queue[..2], 2),
            (2000, Ahead::Take { calls: 2, mb: 7000 })
        );
        // A job in the list that no longer waits takes no place.
        assert_eq!(in_order(&mut queue, &named(&["gone", "b"])), 1);
        assert_eq!(jobs(&queue), ["b", "a", "c"]);
    }

    #[test]
    fn an_older_call_that_fits_goes_first() {
        let m = Machine::new(4000);
        let older = m.waits("job-bash-1-1", 3000, 1);
        let late = m.try_admit("job-bash-2-1", 3000);
        assert_eq!(
            (late.step, late.free_mb, late.ahead),
            (Step::Wait(SEC), 4000, Ahead::Take { calls: 1, mb: 3000 })
        );
        assert_eq!(m.waiting(), ["job-bash-1-1", "job-bash-2-1"]);
        assert_eq!(m.check(&older).step, Step::Run);
        assert_eq!(m.reserved(), ["job-bash-1-1.json"]);
        assert_eq!(m.waiting(), ["job-bash-2-1"]);
    }

    #[test]
    fn a_call_that_fits_passes_a_larger_one() {
        let m = Machine::new(4000);
        m.waits("job-bash-1-1", 6000, 1);
        let small = m.try_admit("job-bash-2-1", 2000);
        assert_eq!(
            (small.step, small.ahead),
            (Step::Run, Ahead::Take { calls: 0, mb: 0 })
        );
        assert_eq!(m.waiting(), ["job-bash-1-1"]);
    }

    #[test]
    fn no_call_passes_a_call_given_priority() {
        let m = Machine::new(4000);
        let big = m.waits("job-bash-1-1", 6000, 1);
        let small = m.waits("job-bash-2-1", 2000, 2);
        let given = set_priority(&m.paths, &named(&["job-bash-1-1"])).unwrap();
        assert_eq!(given, std::slice::from_ref(&big));
        let blocked = m.check(&small);
        assert_eq!(
            (blocked.step, blocked.ahead),
            (Step::Wait(SEC), Ahead::Priority)
        );
        // Refused, a call leaves the list, and the others pass again.
        let refused = check(&m.paths, &big, |room| {
            step(room, big.need_mb, SEC, Duration::ZERO, SEC)
        })
        .unwrap();
        assert_eq!(refused.step, Step::Refuse);
        assert_eq!(m.priority(), Vec::<String>::new());
        assert_eq!(m.waiting(), ["job-bash-2-1"]);
        assert_eq!(m.check(&small).step, Step::Run);
    }

    #[test]
    fn calls_given_priority_go_in_the_order_given_and_leave_once_they_run() {
        let m = Machine::new(4000);
        let a = m.waits("job-bash-1-1", 3000, 1);
        let b = m.waits("job-bash-2-1", 3000, 2);
        set_priority(&m.paths, &named(&["job-bash-2-1", "job-bash-1-1"])).unwrap();
        let first = m.check(&a);
        assert_eq!(
            (first.step, first.ahead),
            (Step::Wait(SEC), Ahead::Take { calls: 1, mb: 3000 })
        );
        assert_eq!(m.check(&b).step, Step::Run);
        assert_eq!(m.priority(), ["job-bash-1-1"]);
    }

    #[test]
    fn priority_goes_only_to_waiting_calls_and_replaces_the_list() {
        let m = Machine::new(4000);
        let one = m.waits("job-bash-1-1", 3000, 1);
        m.waits("job-bash-2-1", 3000, 2);
        let err = set_priority(&m.paths, &named(&["job-bash-2-1", "job-bash-9-1"])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "job-bash-9-1 does not wait for memory: `orchestrator admission` lists the calls that do"
        );
        assert_eq!(m.priority(), Vec::<String>::new());
        let given = set_priority(
            &m.paths,
            &named(&["job-bash-2-1", "job-bash-1-1", "job-bash-2-1"]),
        )
        .unwrap();
        assert_eq!(jobs(&given), ["job-bash-2-1", "job-bash-1-1"]);
        let snap = snapshot(&m.paths).unwrap();
        assert_eq!(
            (jobs(&snap.waiting), snap.priority),
            (vec!["job-bash-2-1", "job-bash-1-1"], 2)
        );
        set_priority(&m.paths, &[]).unwrap();
        assert_eq!(m.priority(), Vec::<String>::new());
        let snap = snapshot(&m.paths).unwrap();
        assert_eq!(
            (jobs(&snap.waiting), snap.priority),
            (vec!["job-bash-1-1", "job-bash-2-1"], 0)
        );
        // A call that ended while it waited leaves the list at the next check.
        set_priority(&m.paths, &named(&["job-bash-1-1"])).unwrap();
        m.end(&one.group);
        assert_eq!(m.try_admit("job-bash-3-1", 100).step, Step::Run);
        assert_eq!(m.priority(), Vec::<String>::new());
    }

    #[test]
    fn notices_say_what_the_calls_ahead_take() {
        let call = typecheck(3000);
        let mut check = Check {
            step: Step::Wait(SEC),
            free_mb: 4000,
            held_mb: 0,
            holders: Vec::new(),
            ahead: Ahead::Take { calls: 2, mb: 3000 },
        };
        assert_eq!(
            waiting_notice(&call, &CFG, &check, 30),
            "orchestrator: waiting for memory before running `pnpm typecheck`. This call is expected to peak at 3000 MB; with the 500 MB margin it needs 3500 MB free, and 4000 MB are free; 3000 MB of them go first to 2 calls waiting ahead of it. It starts as soon as memory frees up; past 30 s, it is refused."
        );
        check.ahead = Ahead::Priority;
        assert_eq!(
            refused_notice(&call, &CFG, true, &check, 3500),
            "orchestrator: refused `pnpm typecheck` after 30 min: 4000 MB are free and it needs 3500 MB; a call given priority waits ahead of it, and no call passes it."
        );
    }

    #[test]
    fn a_record_left_by_a_killed_prefix_is_pruned() {
        let m = Machine::new(4000);
        let job = m.job("job-bash-1-1", 0);
        let call = Waiting {
            job: job.name.clone(),
            group: job.group.clone(),
            label: "make".into(),
            peak_mb: 3000,
            need_mb: 3500,
            since_ms: 1,
        };
        record(&m.paths.runtime, &call);
        assert_eq!(waiting(&m.paths, true), [call]);
        m.end(&job.group);
        assert_eq!(waiting(&m.paths, false), []);
        let dir = waiting_dir(&m.paths.runtime);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(waiting(&m.paths, true), []);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// The notices of a call admission holds `cfg`'s longest wait on a
    /// machine where it never fits, and what it decided.
    fn refused(cfg: &Admission, background: bool) -> (Outcome, Vec<String>, Machine) {
        let m = Machine::new(1000);
        let job = m.job("job-bash-1-1", 0);
        let mut out = Vec::new();
        let start = Instant::now();
        let call = typecheck(3000);
        let recheck = Duration::from_millis(100);
        let outcome = admit_every(&m.paths, cfg, &job, &call, background, recheck, &mut out);
        assert!(start.elapsed() >= Duration::from_secs(longest_wait_secs(cfg, background)));
        let out = String::from_utf8(out).unwrap();
        (outcome.unwrap(), out.lines().map(String::from).collect(), m)
    }

    #[test]
    fn after_the_longest_wait_a_call_is_refused_with_how_to_wait_longer() {
        let cfg = Admission {
            max_wait_secs: 1,
            max_background_wait_secs: 120,
            ..CFG
        };
        let (outcome, lines, m) = refused(&cfg, false);
        assert_eq!(outcome, Outcome::Refused);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].ends_with("and 1000 MB are free. It starts as soon as memory frees up; past 1 s, it is refused."),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[1],
            "orchestrator: refused `pnpm typecheck` after 1 s: 1000 MB are free and it needs 3500 MB. To let it wait up to 2 min, run it in the background with ORCHESTRATOR_BACKGROUND=wait-for-memory before the command."
        );
        // A refused call reserves nothing, and stops being recorded as waiting.
        assert_eq!(m.reserved(), Vec::<String>::new());
        assert_eq!(waiting(&m.paths, false), []);
    }

    #[test]
    fn in_the_background_a_call_waits_its_own_longest_wait_then_is_refused() {
        let cfg = Admission {
            max_wait_secs: 0,
            max_background_wait_secs: 1,
            ..CFG
        };
        let (outcome, lines, m) = refused(&cfg, true);
        assert_eq!(outcome, Outcome::Refused);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].ends_with("past 1 s, it is refused."),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[1],
            "orchestrator: refused `pnpm typecheck` after 1 s: 1000 MB are free and it needs 3500 MB."
        );
        assert_eq!(m.reserved(), Vec::<String>::new());
        // Without a foreground wait, a call is refused at once, saying nothing else.
        let (outcome, lines, _) = refused(&cfg, false);
        assert_eq!(outcome, Outcome::Refused);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("orchestrator: refused `pnpm typecheck` at once: "),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn a_call_that_fits_says_nothing() {
        let m = Machine::new(8000);
        let job = m.job("job-bash-1-1", 0);
        let mut out = Vec::new();
        let outcome = admit(&m.paths, &CFG, &job, &typecheck(3000), false, &mut out).unwrap();
        assert_eq!(outcome, Outcome::Run);
        assert_eq!(out, b"");
        assert_eq!(m.reserved(), ["job-bash-1-1.json"]);
    }

    #[test]
    fn without_meminfo_the_call_runs_unreserved() {
        let m = Machine::new(8000);
        fs::remove_file(&m.paths.meminfo).unwrap();
        let job = m.job("job-bash-1-1", 0);
        let mut out = Vec::new();
        assert!(admit(&m.paths, &CFG, &job, &typecheck(3000), false, &mut out).is_err());
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
            ahead: Ahead::Take { calls: 0, mb: 0 },
        };
        let notices = [
            waiting_notice(&call, &CFG, &check, 30),
            running_notice(&call, SEC),
            refused_notice(&call, &CFG, false, &check, 2000),
            refused_notice(&call, &CFG, true, &check, 2000),
        ];
        for n in notices {
            assert!(n.contains("`python3 -c b = bytearray(1500 << 20)`"), "{n}");
        }
    }
}
