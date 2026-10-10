//! The watch loop. It sleeps until the kernel reports something: memory
//! pressure on the machine or in a job (a PSI trigger each), the end of a
//! Claude session's process (a pidfd per session, found through inotify on
//! the sessions directory), or a change in the job groups of orchestrated
//! sessions (inotify), a process killed for lack of memory included. Slow
//! sweeps of jobs and orphans catch what these signals cannot show. Each
//! measured Bash call teaches its peak to the learned peaks. Once the
//! configuration enables the coordinator, the events that need judgment go
//! to it (see `coordinator`).
//!
//! What a job uses is read only when an event names it (`watch::usage`): a
//! job stalls, or the machine does and the event lists the jobs using the
//! most. Between events, watching reads nothing.

use anyhow::{Context, Result};
use claude_code::sessions;
use coordinator::{self, service::Service, service::Wake};
use learning::peaks;
use prefix::admission;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use system::cgroup;
use system::memory;
use system::pressure::Trigger;
use system::runtime;
use watch::Places;
use watch::api;
use watch::attribution::Attribution;
use watch::events::Hub;
use watch::events::{self, Event, JobUsage};
use watch::exits::Exits;
use watch::jobs::{self, Killed, Tracker};
use watch::report::Local;
use watch::usage::Reader;

/// Between two sweeps of the job groups while the tracker runs.
const JOB_SWEEP: Duration = Duration::from_secs(60);
/// Between two sweeps of the job groups once the tracker has stopped.
const JOB_SWEEP_UNTRACKED: Duration = Duration::from_secs(2);
/// Time a session's processes get to exit after the session ends, before what
/// is left counts as orphaned.
const ORPHAN_GRACE: Duration = Duration::from_secs(5);
/// Jobs a memory pressure event lists, using the most first.
const LARGEST_JOBS: usize = 3;

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Memory stall within a PSI window that makes a pressure event.
    pub stall_ms: u64,
    pub cooldown_secs: u64,
}

pub struct Options {
    pub proc_root: PathBuf,
    pub sessions_dir: PathBuf,
    pub runtime_dir: PathBuf,
    /// Where learned peaks are kept, across reboots.
    pub state_dir: PathBuf,
    /// The configuration file: whether the coordinator is enabled, and its
    /// bounds.
    pub config_path: PathBuf,
    /// Between two orphan scans when no session ends.
    pub orphan_interval: Duration,
    pub thresholds: Thresholds,
}

/// Decides which events to emit. Holds no IO, so its rules are tested alone.
pub struct Watcher {
    thresholds: Thresholds,
    last_pressure: Option<u64>,
    /// When each job last made a pressure event, by session scope and job.
    last_job_pressure: HashMap<(String, String), u64>,
    /// Orphan groups already reported, by root (pid, start time).
    reported: HashSet<(u32, u64)>,
}

impl Watcher {
    pub fn new(thresholds: Thresholds) -> Self {
        Watcher {
            thresholds,
            last_pressure: None,
            last_job_pressure: HashMap::new(),
            reported: HashSet::new(),
        }
    }

    /// The kernel reported memory pressure. `scan` runs only when an event is
    /// due, and gives the sessions and the jobs using the most.
    pub fn on_pressure(
        &mut self,
        available_mb: u64,
        now: u64,
        scan: impl FnOnce() -> Result<(Attribution, Vec<JobUsage>)>,
    ) -> Result<Option<Event>> {
        let cooled = self
            .last_pressure
            .is_none_or(|t| now.saturating_sub(t) >= self.thresholds.cooldown_secs);
        if !cooled {
            return Ok(None);
        }
        let (att, jobs) = scan()?;
        self.last_pressure = Some(now);
        Ok(Some(Event::memory_pressure(
            available_mb,
            self.thresholds.stall_ms,
            &att,
            jobs,
        )))
    }

    /// The kernel reported the job `job` of the session scope `scope`
    /// stalling: whether an event is due, at most one per job per cooldown.
    pub fn on_job_pressure(&mut self, scope: &str, job: &str, now: u64) -> bool {
        let cooldown = self.thresholds.cooldown_secs;
        self.last_job_pressure
            .retain(|_, t| now.saturating_sub(*t) < cooldown);
        let key = (scope.to_string(), job.to_string());
        if self.last_job_pressure.contains_key(&key) {
            return false;
        }
        self.last_job_pressure.insert(key, now);
        true
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

/// Where finished jobs are found, and where their measurements go.
struct JobPaths {
    slice: PathBuf,
    records: PathBuf,
    measurements: PathBuf,
    peaks: PathBuf,
}

/// The kernel's signals, each optional: a source that cannot be set up is
/// reported once and left to the sweeps.
struct Sources {
    trigger: Option<Trigger>,
    exits: Option<Exits>,
    tracker: Option<Tracker>,
}

/// What a wait reports.
#[derive(Clone, Copy)]
enum Signal {
    Pressure,
    TriggerBroken,
    SessionsChanged,
    JobsChanged,
    /// The trigger of a job, by its id in the tracker, fired.
    JobStalled(u64),
    /// The trigger of a job reported something other than a stall.
    JobTriggerBroken(u64),
    /// The Bash call of a job, by its id in the tracker, ended.
    CallEnded(u64),
    /// The claude process of a session exited.
    Exited(u32),
    Coordinator(Wake),
}

pub fn run(cfg: &Options) -> Result<()> {
    std::fs::create_dir_all(&cfg.runtime_dir)
        .with_context(|| format!("creating {}", cfg.runtime_dir.display()))?;
    let meminfo = cfg.proc_root.join("meminfo");
    // Fail fast on a wrong proc root rather than logging the same error forever.
    memory::available_mb(&meminfo)?;
    let events_path = cfg.runtime_dir.join("events.jsonl");
    let hub = Hub::default();
    let listener = api::bind(&api::socket(&cfg.runtime_dir))?;
    api::serve(
        listener,
        Arc::new(Local(places(cfg, &meminfo))),
        hub.clone(),
    );
    let job_paths = job_paths(cfg);
    let jp = job_paths.as_ref();
    let mut src = sources(cfg, jp);
    let mut watcher = Watcher::new(cfg.thresholds);
    let mut coordinator = Service::new(places(cfg, &meminfo), events_path.clone(), hub.clone());
    coordinator.start();
    let mut next_orphan_scan = Instant::now();
    let mut next_job_sweep = Instant::now() + JOB_SWEEP;
    loop {
        let mut deadline = if job_paths.is_some() {
            next_orphan_scan.min(next_job_sweep)
        } else {
            next_orphan_scan
        };
        if let Some(d) = coordinator.deadline() {
            deadline = deadline.min(d);
        }
        let waited = wait(
            &src,
            &coordinator,
            deadline.saturating_duration_since(Instant::now()),
        );
        let signals = match waited {
            Ok(signals) => signals,
            Err(e) => {
                // Never spin on a failing wait.
                eprintln!("orchestrator: waiting: {e}");
                std::thread::sleep(Duration::from_secs(1));
                Vec::new()
            }
        };
        for signal in signals {
            match signal {
                Signal::Pressure => queue(
                    &mut coordinator,
                    on_pressure(cfg, &meminfo, jp, &events_path, &hub, &mut watcher),
                ),
                Signal::JobStalled(_) | Signal::JobTriggerBroken(_) | Signal::CallEnded(_) => {
                    if let (Some(tracker), Some(p)) = (src.tracker.as_mut(), jp) {
                        let out = (events_path.as_path(), &hub, &mut coordinator);
                        on_job_signal(signal, cfg, p, tracker, &mut watcher, out);
                    }
                }
                Signal::Coordinator(wake) => coordinator.on_wake(wake),
                Signal::TriggerBroken => {
                    eprintln!(
                        "orchestrator: the memory pressure trigger broke; no more pressure events"
                    );
                    src.trigger = None;
                }
                Signal::SessionsChanged => {
                    if let Some(exits) = src.exits.as_mut() {
                        log(exits.sessions_changed().context("reading session changes"));
                    }
                }
                Signal::Exited(pid) => {
                    if let Some(exits) = src.exits.as_mut() {
                        exits.release(pid);
                    }
                    next_orphan_scan = next_orphan_scan.min(Instant::now() + ORPHAN_GRACE);
                }
                Signal::JobsChanged => {
                    if let Some(p) = jp {
                        let out = (events_path.as_path(), &hub, &mut coordinator);
                        jobs_changed(cfg, p, &mut src.tracker, out);
                    }
                }
            }
        }
        coordinator.on_time();
        let now = Instant::now();
        if let Some(p) = jp.filter(|_| now >= next_job_sweep) {
            let out = (events_path.as_path(), &hub, &mut coordinator);
            next_job_sweep = now + sweep_jobs(cfg, p, src.tracker.as_mut(), out);
        }
        if now >= next_orphan_scan {
            log(scan_orphans(cfg, &events_path, &hub, &mut watcher));
            next_orphan_scan = now + cfg.orphan_interval;
        }
    }
}

/// Where finished jobs are found and measured, None outside a systemd user
/// manager.
fn job_paths(cfg: &Options) -> Option<JobPaths> {
    let job_paths = own_slice(&cfg.proc_root).map(|slice| JobPaths {
        slice,
        records: prefix::records_root(&cfg.runtime_dir),
        measurements: cfg.runtime_dir.join("measurements.jsonl"),
        peaks: peaks::dir(&cfg.state_dir),
    });
    if job_paths.is_none() {
        eprintln!(
            "orchestrator: no systemd user manager above this process; jobs are not collected"
        );
    }
    job_paths
}

/// Where `watch` reads the state, for its API and its coordinators.
fn places(cfg: &Options, meminfo: &Path) -> Places {
    Places {
        proc_root: cfg.proc_root.clone(),
        sessions_dir: cfg.sessions_dir.clone(),
        admission: admission::Paths {
            cgroup_root: PathBuf::from(cgroup::ROOT),
            meminfo: meminfo.to_path_buf(),
            runtime: cfg.runtime_dir.clone(),
        },
        state_dir: cfg.state_dir.clone(),
        config: cfg.config_path.clone(),
    }
}

/// Sets up each kernel signal, reporting the ones that cannot be.
fn sources(cfg: &Options, job_paths: Option<&JobPaths>) -> Sources {
    Sources {
        trigger: report(
            "memory pressure",
            Trigger::new(
                &cfg.proc_root.join("pressure/memory"),
                Duration::from_millis(cfg.thresholds.stall_ms),
            ),
        ),
        exits: report(
            "session ends",
            Exits::new(cfg.sessions_dir.clone(), cfg.proc_root.clone()),
        ),
        tracker: job_paths.and_then(|p| {
            let (tracker, measured) = report(
                "job ends",
                Tracker::new(
                    p.slice.clone(),
                    p.records.clone(),
                    Duration::from_millis(cfg.thresholds.stall_ms),
                    remove_group,
                ),
            )?;
            keep(p, &measured);
            Some(tracker)
        }),
    }
}

/// Sleeps until a source has something to report or `timeout` runs out.
fn wait(src: &Sources, coordinator: &Service, timeout: Duration) -> std::io::Result<Vec<Signal>> {
    let mut fds = Vec::new();
    let mut which = Vec::new();
    for (fd, wake) in coordinator.fds() {
        fds.push(PollFd::from_borrowed_fd(fd, PollFlags::IN));
        which.push(Signal::Coordinator(wake));
    }
    if let Some(t) = &src.trigger {
        fds.push(PollFd::new(t, PollFlags::PRI));
        which.push(Signal::Pressure);
    }
    if let Some(t) = &src.tracker {
        fds.push(PollFd::from_borrowed_fd(t.fd(), PollFlags::IN));
        which.push(Signal::JobsChanged);
        for (id, fd) in t.triggers() {
            fds.push(PollFd::from_borrowed_fd(fd, PollFlags::PRI));
            which.push(Signal::JobStalled(id));
        }
        for (id, fd) in t.shells() {
            fds.push(PollFd::from_borrowed_fd(fd, PollFlags::IN));
            which.push(Signal::CallEnded(id));
        }
    }
    if let Some(e) = &src.exits {
        fds.push(PollFd::from_borrowed_fd(e.sessions_fd(), PollFlags::IN));
        which.push(Signal::SessionsChanged);
        for (pid, fd) in e.held() {
            fds.push(PollFd::from_borrowed_fd(fd, PollFlags::IN));
            which.push(Signal::Exited(pid));
        }
    }
    let timeout = Timespec::try_from(timeout).unwrap_or(Timespec {
        tv_sec: i64::MAX,
        tv_nsec: 0,
    });
    match poll(&mut fds, Some(&timeout)) {
        Ok(_) => {}
        Err(Errno::INTR) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    }
    let signals = fds.iter().zip(which).filter_map(|(fd, signal)| {
        let revents = fd.revents();
        match signal {
            _ if revents.is_empty() => None,
            // A trigger only ever reports priority data; anything else is an error.
            Signal::Pressure if !revents.contains(PollFlags::PRI) => Some(Signal::TriggerBroken),
            // A job's group gone, its trigger reports priority data and an
            // error, at every wait: taken for a stall, it would spin (#24).
            Signal::JobStalled(id) if revents != PollFlags::PRI => {
                Some(Signal::JobTriggerBroken(id))
            }
            signal => Some(signal),
        }
    });
    Ok(signals.collect())
}

/// Writes a memory pressure event when one is due, and returns it.
fn on_pressure(
    cfg: &Options,
    meminfo: &Path,
    job_paths: Option<&JobPaths>,
    events_path: &Path,
    hub: &Hub,
    watcher: &mut Watcher,
) -> Result<Option<(u64, Event)>> {
    let now = now_secs();
    let available = memory::available_mb(meminfo)?;
    let event = watcher.on_pressure(available, now, || {
        let att = watch::scan(&cfg.proc_root, &cfg.sessions_dir)?;
        let jobs = job_paths.map_or_else(Vec::new, |p| {
            let (sessions, home) = (read_sessions(cfg), home());
            reader(cfg, p, &sessions, home.as_deref()).largest_jobs(LARGEST_JOBS)
        });
        Ok((att, jobs))
    })?;
    if let Some(event) = &event {
        events::emit(events_path, hub, now, event)?;
    }
    Ok(event.map(|e| (now, e)))
}

/// Handles what a job's own descriptors report: its trigger, or the end of
/// its Bash call.
fn on_job_signal(
    signal: Signal,
    cfg: &Options,
    p: &JobPaths,
    tracker: &mut Tracker,
    watcher: &mut Watcher,
    out: Out<'_>,
) {
    match signal {
        Signal::JobStalled(id) => {
            let (events_path, hub, coordinator) = out;
            let written = on_job_pressure(cfg, p, tracker, id, watcher, events_path, hub);
            queue(coordinator, written);
        }
        Signal::JobTriggerBroken(id) => tracker.disarm(id),
        Signal::CallEnded(id) => {
            tracker.call_ended(id);
            report_kills(cfg, p, tracker, out);
        }
        _ => {}
    }
}

/// Writes a job pressure event for the trigger `id` when one is due, and
/// returns it.
fn on_job_pressure(
    cfg: &Options,
    p: &JobPaths,
    tracker: &Tracker,
    id: u64,
    watcher: &mut Watcher,
    events_path: &Path,
    hub: &Hub,
) -> Result<Option<(u64, Event)>> {
    let now = now_secs();
    let Some((scope, job)) = tracker.stalled(id) else {
        return Ok(None);
    };
    if !watcher.on_job_pressure(scope, job, now) {
        return Ok(None);
    }
    let (sessions, home) = (read_sessions(cfg), home());
    let Some(usage) = reader(cfg, p, &sessions, home.as_deref()).job(scope, job) else {
        return Ok(None);
    };
    let event = Event::JobPressure(events::JobPressure {
        job: usage,
        stall_ms: cfg.thresholds.stall_ms,
    });
    events::emit(events_path, hub, now, &event)?;
    Ok(Some((now, event)))
}

/// Hands an event just written to the coordinator, which queues it if it
/// needs judgment.
fn queue(coordinator: &mut Service, written: Result<Option<(u64, Event)>>) {
    match written {
        Ok(Some((at, event))) => coordinator.enqueue(at, &event),
        Ok(None) => {}
        Err(e) => eprintln!("orchestrator: {e:#}"),
    }
}

/// Where events go: the events file, the API's streams, the coordinator.
type Out<'a> = (&'a Path, &'a Hub, &'a mut Service);

/// Writes an event for each kill the tracker read, and hands it to the
/// coordinator.
fn report_kills(cfg: &Options, p: &JobPaths, tracker: &mut Tracker, out: Out<'_>) {
    let (events_path, hub, coordinator) = out;
    let killed: Vec<Killed> = tracker.take_kills();
    if killed.is_empty() {
        return;
    }
    let now = now_secs();
    let (sessions, home) = (read_sessions(cfg), home());
    let reader = reader(cfg, p, &sessions, home.as_deref());
    for k in killed {
        let session = reader.session(&k.session);
        let event = Event::OomKill(events::OomKill {
            session: session.map(|s| claude_code::messages::address(s).to_string()),
            session_id: session.map(|s| s.session_id.clone()),
            command: k.record.as_ref().and_then(|r| reader.label_of(r)),
            job: k.job,
            killed: k.killed,
            call_running: k.call_running,
        });
        match events::emit(events_path, hub, now, &event) {
            Ok(()) => coordinator.enqueue(now, &event),
            Err(e) => eprintln!("orchestrator: {e:#}"),
        }
    }
}

fn reader<'a>(
    cfg: &'a Options,
    p: &'a JobPaths,
    sessions: &'a [sessions::ClaudeSession],
    home: Option<&'a Path>,
) -> Reader<'a> {
    Reader {
        slice: &p.slice,
        records: &p.records,
        proc_root: &cfg.proc_root,
        sessions,
        home,
    }
}

/// The sessions to name jobs by; none when they cannot be read.
fn read_sessions(cfg: &Options) -> Vec<sessions::ClaudeSession> {
    sessions::read_sessions(&cfg.sessions_dir).unwrap_or_default()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn scan_orphans(cfg: &Options, events_path: &Path, hub: &Hub, watcher: &mut Watcher) -> Result<()> {
    let att = watch::scan(&cfg.proc_root, &cfg.sessions_dir)?;
    if let Some(event) = watcher.check_orphans(&att) {
        events::emit(events_path, hub, now_secs(), &event)?;
    }
    Ok(())
}

/// Handles what the tracker reports; stops it on an error, leaving the jobs
/// to the sweeps.
fn jobs_changed(cfg: &Options, p: &JobPaths, tracker: &mut Option<Tracker>, out: Out<'_>) {
    let Some(t) = tracker.as_mut() else {
        return;
    };
    match t.read_ready() {
        Ok(measured) => {
            keep(p, &measured);
            report_kills(cfg, p, t, out);
        }
        Err(e) => {
            eprintln!("orchestrator: job tracking stopped, sweeping instead: {e}");
            *tracker = None;
        }
    }
}

/// Sweeps the jobs, through the tracker when there is one, so that it reads
/// the kills of the jobs the sweep removes. Returns when the next sweep is
/// due.
fn sweep_jobs(
    cfg: &Options,
    p: &JobPaths,
    tracker: Option<&mut Tracker>,
    out: Out<'_>,
) -> Duration {
    let (measured, every) = match tracker {
        Some(t) => {
            let measured = t.sweep();
            report_kills(cfg, p, t, out);
            (measured, JOB_SWEEP)
        }
        None => (
            jobs::collect(&p.slice, &p.records, runtime::now_ms(), remove_group),
            JOB_SWEEP_UNTRACKED,
        ),
    };
    match measured {
        Ok(measured) => keep(p, &measured),
        Err(e) => eprintln!("orchestrator: sweeping jobs in {}: {e}", p.slice.display()),
    }
    every
}

fn remove_group(dir: &Path) -> std::io::Result<()> {
    std::fs::remove_dir(dir)
}

/// Writes each measurement down and learns from it. A failure is reported,
/// never fatal.
fn keep(p: &JobPaths, measured: &[learning::peaks::Measurement]) {
    let home = home();
    for m in measured {
        if let Err(e) = events::append_line(&p.measurements, m) {
            eprintln!("orchestrator: {e:#}");
        }
        if let Err(e) = peaks::learn(&p.peaks, m, home.as_deref()) {
            eprintln!("orchestrator: learning a peak: {e:#}");
        }
    }
}

/// A daemon outlives transient errors: a process vanishing mid-read, a
/// sessions file being rewritten.
fn log(result: Result<()>) {
    if let Err(e) = result {
        eprintln!("orchestrator: {e:#}");
    }
}

fn report<T>(what: &str, source: std::io::Result<T>) -> Option<T> {
    source
        .map_err(|e| eprintln!("orchestrator: no kernel signal for {what}, sweeps only: {e}"))
        .ok()
}

fn now_secs() -> u64 {
    u64::try_from(runtime::now_ms() / 1000).unwrap_or(u64::MAX)
}

/// The orchestrator slice of the user manager this process runs under.
fn own_slice(proc_root: &Path) -> Option<PathBuf> {
    let own = std::fs::read_to_string(proc_root.join("self/cgroup")).ok()?;
    let slice = cgroup::slice_path(cgroup::own_path(&own)?)?;
    Some(Path::new(cgroup::ROOT).join(slice.trim_start_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use watch::attribution::{Orphan, ProcRef};

    const T: Thresholds = Thresholds {
        stall_ms: 200,
        cooldown_secs: 60,
    };

    fn orphan(pid: u32, start_time: u64) -> Orphan {
        Orphan {
            root: ProcRef {
                pid,
                rss_kb: 1024,
                comm: "node".into(),
                cmdline: "node dev".into(),
                command_head: "node dev".into(),
            },
            start_time,
            session_id: "dead".into(),
            rss_kb: 1024,
            processes: 1,
        }
    }

    #[test]
    fn memory_pressure_respects_the_cooldown() {
        let mut w = Watcher::new(T);
        let empty = || Ok((Attribution::default(), Vec::new()));
        assert!(w.on_pressure(2000, 10, empty).unwrap().is_some());
        assert!(w.on_pressure(2000, 69, empty).unwrap().is_none());
        assert!(w.on_pressure(2000, 70, empty).unwrap().is_some());
    }

    #[test]
    fn each_job_has_its_own_cooldown() {
        let mut w = Watcher::new(T);
        assert!(w.on_job_pressure("s.scope", "job-bash-1-1", 10));
        assert!(!w.on_job_pressure("s.scope", "job-bash-1-1", 69));
        assert!(w.on_job_pressure("s.scope", "job-bash-2-1", 69));
        assert!(w.on_job_pressure("s.scope", "job-bash-1-1", 70));
    }

    #[test]
    fn no_scan_during_the_cooldown() {
        let mut w = Watcher::new(T);
        let empty = || Ok((Attribution::default(), Vec::new()));
        assert!(w.on_pressure(2000, 0, empty).unwrap().is_some());
        let event = w
            .on_pressure(2000, 1, || anyhow::bail!("scan must not run"))
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
            Some(Event::Orphans(o)) => assert_eq!(o.orphans.len(), 1),
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
