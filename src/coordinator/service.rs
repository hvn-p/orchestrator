//! The coordinator as `watch` drives it. Once the configuration has a
//! `coordinator` section, `watch` queues the events that need judgment and
//! starts a run for them as soon as no coordinator is running, and reports a
//! Bash call that admission holds back long. A run lasts at most the
//! configured time: past it, `watch` stops its process group.

use super::{Holder, Paths, Pending, Waits, pidfd, run};
use crate::admission;
use crate::config::{self, Coordinator};
use crate::events::{self, Event};
use crate::{cgroup, runtime, sessions};
use anyhow::{Context, Result};
use inotify::{Inotify, WatchMask};
use rustix::process::{Pid, Signal, kill_process_group};
use std::ffi::OsString;
use std::fs::{self, File};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

/// Where holders are looked up: always the real processes, whatever the
/// proc root `watch` reads sessions from.
const PROC: &str = "/proc";
/// Between asking a run to stop and killing it.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// Room for a batch of inotify events; one takes 16 bytes plus its name.
const EVENT_BUFFER: usize = 4096;

/// What woke `watch` for the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// A call started or stopped waiting for memory.
    Waiting,
    /// The run `watch` started ended.
    RunEnded,
    /// The coordinator someone else started ended.
    HolderEnded,
}

/// A run `watch` started.
struct Running {
    child: Child,
    pidfd: OwnedFd,
    holder: Holder,
    at: u64,
    started: Instant,
    events: usize,
    deadline: Instant,
    stop: Stop,
}

/// Where a run stands with its time limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Within,
    /// Asked to stop, at this instant.
    Asked(Instant),
    Killed,
}

pub struct Service {
    paths: Paths,
    admission: admission::Paths,
    config: PathBuf,
    sessions_dir: PathBuf,
    events: PathBuf,
    /// This binary's directory, first on a run's `PATH`.
    bin: PathBuf,
    /// The program started as a coordinator: `claude`, but in tests.
    claude: OsString,
    waiting: Option<Inotify>,
    waits: Waits,
    /// When the next call will have waited long enough, in ms since the epoch.
    next_wait: Option<u64>,
    running: Option<Running>,
    /// A coordinator someone else started, until it ends.
    busy: Option<OwnedFd>,
}

impl Service {
    pub fn new(
        paths: Paths,
        admission: admission::Paths,
        config: PathBuf,
        sessions_dir: PathBuf,
        events: PathBuf,
    ) -> Service {
        let dir = admission::waiting_dir(&admission.runtime);
        let waiting = fs::create_dir_all(&dir)
            .and_then(|()| Inotify::init())
            .and_then(|inotify| {
                let mask = WatchMask::CREATE
                    | WatchMask::CLOSE_WRITE
                    | WatchMask::MOVED_TO
                    | WatchMask::DELETE
                    | WatchMask::ONLYDIR;
                inotify.watches().add(&dir, mask)?;
                Ok(inotify)
            })
            .map_err(|e| eprintln!("orchestrator: not watching {}: {e}", dir.display()))
            .ok();
        let bin = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        Service {
            paths,
            admission,
            config,
            sessions_dir,
            events,
            bin,
            claude: "claude".into(),
            waiting,
            waits: Waits::default(),
            next_wait: None,
            running: None,
            busy: None,
        }
    }

    /// The descriptors to wait on, with what each reports.
    pub fn fds(&self) -> Vec<(BorrowedFd<'_>, Wake)> {
        let mut fds = Vec::new();
        if let Some(inotify) = &self.waiting {
            fds.push((inotify.as_fd(), Wake::Waiting));
        }
        if let Some(r) = &self.running {
            fds.push((r.pidfd.as_fd(), Wake::RunEnded));
        }
        if let Some(fd) = &self.busy {
            fds.push((fd.as_fd(), Wake::HolderEnded));
        }
        fds
    }

    /// When `on_time` has something to do.
    pub fn deadline(&self) -> Option<Instant> {
        let run = self.running.as_ref().and_then(|r| match r.stop {
            Stop::Within => Some(r.deadline),
            Stop::Asked(at) => Some(at + STOP_GRACE),
            Stop::Killed => None,
        });
        let wait = self.next_wait.map(|at| {
            let now = now_ms();
            Instant::now() + Duration::from_millis(at.saturating_sub(now))
        });
        [run, wait].into_iter().flatten().min()
    }

    pub fn on_wake(&mut self, wake: Wake) {
        match wake {
            Wake::Waiting => {
                if let Some(inotify) = self.waiting.as_mut() {
                    let mut buffer = [0; EVENT_BUFFER];
                    while inotify
                        .read_events(&mut buffer)
                        .is_ok_and(|mut e| e.next().is_some())
                    {}
                }
                self.check_waits();
            }
            Wake::RunEnded => self.finish(),
            Wake::HolderEnded => {
                self.busy = None;
                self.start_if_due();
            }
        }
    }

    /// Stops a run past its time, and reports long waits.
    pub fn on_time(&mut self) {
        let now = Instant::now();
        if let Some(r) = self.running.as_mut() {
            let group = Pid::from_child(&r.child);
            match r.stop {
                Stop::Within if now >= r.deadline => {
                    eprintln!("orchestrator: the coordinator ran past its time limit; stopping it");
                    let _ = kill_process_group(group, Signal::TERM);
                    r.stop = Stop::Asked(now);
                }
                Stop::Asked(at) if now >= at + STOP_GRACE => {
                    let _ = kill_process_group(group, Signal::KILL);
                    r.stop = Stop::Killed;
                }
                _ => {}
            }
        }
        if self.next_wait.is_some_and(|at| at <= now_ms()) {
            self.check_waits();
        }
    }

    /// At start: what came while `watch` was not running.
    pub fn start(&mut self) {
        self.check_waits();
        self.start_if_due();
    }

    /// Queues an event `watch` wrote at `at`, if it needs judgment, and
    /// starts a coordinator for it when none runs.
    pub fn enqueue(&mut self, at: u64, event: &Event) {
        if config::coordinator(&self.config).is_some() {
            self.queue(at, event);
            self.start_if_due();
        }
    }

    fn queue(&self, at: u64, event: &Event) {
        let queued = Pending::new(at, event).and_then(|p| match p {
            Some(p) => super::enqueue(&self.paths, p),
            None => Ok(()),
        });
        if let Err(e) = queued {
            eprintln!("orchestrator: queueing an event for the coordinator: {e:#}");
        }
    }

    /// Reports each call that has waited long, naming its session as
    /// messages address it.
    // claude-code: cross-session-message
    fn check_waits(&mut self) {
        let Some(cfg) = config::coordinator(&self.config) else {
            self.next_wait = None;
            return;
        };
        let waiting = admission::waiting(&self.admission, true);
        let now = now_ms();
        let (due, next) = self
            .waits
            .due(&waiting, now, cfg.wait_secs.saturating_mul(1000));
        self.next_wait = next;
        if due.is_empty() {
            return;
        }
        let free_mb = admission::snapshot(&self.admission).map_or(0, |s| s.free_mb());
        let sessions = sessions::read_sessions(&self.sessions_dir).unwrap_or_default();
        let at = now / 1000;
        for w in due {
            let session = cgroup::session_of(&w.group)
                .and_then(|scope| cgroup::main_pid(&self.admission.cgroup_root, scope))
                .and_then(|pid| sessions.iter().find(|s| s.pid == pid));
            let event = Event::AdmissionWait {
                session: session.map(|s| s.name.clone()),
                session_id: session.map(|s| s.session_id.clone()),
                job: w.job.clone(),
                command: w.label.clone(),
                waited_secs: now.saturating_sub(w.since_ms) / 1000,
                peak_mb: w.peak_mb,
                need_mb: w.need_mb,
                free_mb,
            };
            if let Err(e) = events::append(&self.events, at, &event) {
                eprintln!("orchestrator: {e:#}");
            }
            self.queue(at, &event);
        }
        self.start_if_due();
    }

    /// Starts a run for the pending events when the coordinator is enabled
    /// and free; when someone else holds it, waits for that one to end.
    fn start_if_due(&mut self) {
        if self.running.is_some() || self.busy.is_some() {
            return;
        }
        let Some(cfg) = config::coordinator(&self.config) else {
            return;
        };
        if let Err(e) = self.try_start(&cfg) {
            eprintln!("orchestrator: starting the coordinator: {e:#}");
        }
    }

    fn try_start(&mut self, cfg: &Coordinator) -> Result<()> {
        let locked = self.paths.lock()?;
        if let Some(other) = locked.holder().filter(|h| h.running(Path::new(PROC))) {
            // Without a pidfd, it ended meanwhile: the coordinator is free.
            if let Ok(fd) = pidfd(other.pid) {
                self.busy = Some(fd);
                return Ok(());
            }
        }
        let batch = super::take_due(&locked, &self.admission)?;
        if batch.is_empty() {
            return Ok(());
        }
        let at = now_ms() / 1000;
        let spawned = run::write_role(&self.paths).and_then(|()| self.spawn(cfg, &batch, at));
        let watched = spawned.and_then(|(mut child, holder)| match pidfd(holder.pid) {
            Ok(fd) => Ok((child, holder, fd)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e).context("watching the coordinator's process")
            }
        });
        let (child, holder, pidfd) = match watched {
            Ok(started) => started,
            Err(e) => {
                // Kept for the next try.
                locked.set_pending(&batch)?;
                return Err(e);
            }
        };
        locked.set_holder(holder)?;
        self.running = Some(Running {
            child,
            pidfd,
            holder,
            at,
            started: Instant::now(),
            events: batch.len(),
            deadline: Instant::now() + Duration::from_secs(cfg.max_minutes.saturating_mul(60)),
            stop: Stop::Within,
        });
        Ok(())
    }

    fn spawn(&self, cfg: &Coordinator, batch: &[Pending], at: u64) -> Result<(Child, Holder)> {
        let out = File::create(self.paths.last_run()).context("creating the run's output")?;
        let err = File::create(self.paths.last_errors()).context("creating the run's errors")?;
        let mode = run::Mode::Batch(batch);
        let child = run::command_of(&self.claude, &mode, cfg, &self.paths, &self.bin, at)
            .stdout(out)
            .stderr(err)
            // Its own process group, stopped whole at the time limit.
            .process_group(0)
            .spawn()
            .context("starting claude")?;
        let holder = Holder::of(Path::new(PROC), child.id()).unwrap_or(Holder {
            pid: child.id(),
            start_time: 0,
        });
        Ok((child, holder))
    }

    /// The run ended: records it, frees the coordinator, and starts the next
    /// run if events came meanwhile.
    fn finish(&mut self) {
        let Some(mut r) = self.running.take() else {
            return;
        };
        let status = r.child.wait();
        let output = fs::read(self.paths.last_run()).unwrap_or_default();
        let record = run::RunRecord::new(
            r.at,
            r.events,
            r.started.elapsed().as_secs(),
            status
                .as_ref()
                .ok()
                .and_then(std::process::ExitStatus::code),
            r.stop != Stop::Within,
            &output,
        );
        if let Some(signal) = status.as_ref().ok().and_then(ExitStatusExt::signal) {
            eprintln!("orchestrator: the coordinator ended on signal {signal}");
        }
        if let Err(e) = events::append_line(&self.paths.runs(), &record) {
            eprintln!("orchestrator: {e:#}");
        }
        match self.paths.lock() {
            Ok(locked) => {
                if let Err(e) = locked.clear_holder(r.holder) {
                    eprintln!("orchestrator: {e:#}");
                }
            }
            Err(e) => eprintln!("orchestrator: {e:#}"),
        }
        self.start_if_due();
    }
}

fn now_ms() -> u64 {
    u64::try_from(runtime::now_ms()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use std::os::unix::fs::PermissionsExt;

    /// A `claude` that keeps its prompt, then prints a result as a run does.
    const FAKE: &str = r#"#!/bin/sh
for last; do :; done
printf '%s' "$last" > "prompt-$$"
printf '{"type":"result","is_error":false,"num_turns":1,"total_cost_usd":0.001,"result":"done"}'
"#;

    struct Setup {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        service: Service,
    }

    fn setup(enabled: bool) -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let config = base.join("config/orchestrator/config.json");
        if enabled {
            let c = Config {
                admission: None,
                coordinator: Some(Coordinator::default()),
            };
            config::save(&config, &c).unwrap();
        }
        let fake = base.join("claude");
        fs::write(&fake, FAKE).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(base.join("meminfo"), "MemAvailable: 8192000 kB\n").unwrap();
        let runtime = base.join("run");
        let mut service = Service::new(
            Paths::new(&runtime, &base.join("state"), &config),
            admission::Paths {
                cgroup_root: base.join("cgroup"),
                meminfo: base.join("meminfo"),
                runtime,
            },
            config,
            base.join("sessions"),
            base.join("events.jsonl"),
        );
        service.claude = fake.into();
        Setup {
            _tmp: tmp,
            base,
            service,
        }
    }

    fn pressure(available_mb: u64) -> Event {
        Event::MemoryPressure {
            available_mb,
            stall_ms: 200,
            largest: None,
            next: vec![],
        }
    }

    /// Waits for the running coordinator to end, then lets the service
    /// handle it.
    fn end_run(s: &mut Service) {
        let fd = s
            .fds()
            .into_iter()
            .find(|(_, w)| *w == Wake::RunEnded)
            .map(|(fd, _)| fd)
            .expect("a run");
        let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
        let timeout = Timespec::try_from(Duration::from_secs(10)).unwrap();
        assert_eq!(poll(&mut fds, Some(&timeout)).unwrap(), 1, "the run ended");
        s.on_wake(Wake::RunEnded);
    }

    fn runs(s: &Setup) -> Vec<serde_json::Value> {
        fs::read_to_string(s.service.paths.runs())
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn prompts(s: &Setup) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(&s.service.paths.home)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("prompt-")
            })
            .map(|p| fs::read_to_string(p).unwrap())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn events_wait_for_the_running_coordinator_then_go_together() {
        let mut s = setup(true);
        s.service.enqueue(10, &pressure(900));
        assert!(s.service.running.is_some(), "a run started");
        // Two more while it runs: they merge and wait.
        s.service.enqueue(11, &pressure(800));
        s.service.enqueue(12, &pressure(700));
        assert_eq!(s.service.paths.lock().unwrap().pending().len(), 1);
        end_run(&mut s.service);
        // The first run is recorded and the next one started at once.
        assert!(s.service.running.is_some(), "the next run started");
        end_run(&mut s.service);
        assert!(s.service.running.is_none());
        let runs = runs(&s);
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[0]["reply"], "done");
        assert_eq!(runs[0]["events"], 1);
        assert_eq!(runs[0]["stopped"], false);
        let prompts = prompts(&s);
        assert_eq!(prompts.len(), 2);
        assert!(prompts.iter().any(|p| p.contains(r#""available_mb":900"#)));
        let merged = prompts
            .iter()
            .find(|p| p.contains(r#""available_mb":700"#))
            .expect("the merged event");
        assert!(merged.contains(r#""count":2"#), "{merged}");
        let locked = s.service.paths.lock().unwrap();
        assert_eq!(locked.holder(), None, "the coordinator is free");
        assert_eq!(locked.pending(), []);
    }

    #[test]
    fn without_consent_nothing_is_queued_nor_started() {
        let mut s = setup(false);
        s.service.enqueue(10, &pressure(900));
        assert!(s.service.running.is_none());
        assert!(!s.base.join("run/coordinator/pending.json").exists());
    }

    #[test]
    fn a_coordinator_held_elsewhere_gets_the_events() {
        let mut s = setup(true);
        let me = Holder::of(Path::new(PROC), std::process::id()).unwrap();
        s.service.paths.lock().unwrap().set_holder(me).unwrap();
        s.service.enqueue(10, &pressure(900));
        assert!(s.service.running.is_none());
        assert!(s.service.fds().iter().any(|(_, w)| *w == Wake::HolderEnded));
        assert_eq!(s.service.paths.lock().unwrap().pending().len(), 1);
    }
}
