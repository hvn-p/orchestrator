//! What `orchestrator`'s read commands show: each report as data, which the
//! API serves, then as text for a reader, a human or a coordinator. `watch`
//! hands the same text to a coordinator when it wakes one, so that it need
//! not run the commands itself.

use crate::Places;
use anyhow::Result;
use claude_code::messages;
use claude_code::sessions;
use learning::peaks;
use prefix::admission;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use system::machine::{self, Machine};
use system::{cgroup, memory, procfs, runtime};

/// Characters of a command line shown by `orchestrator sessions`.
const DISPLAY_CMDLINE: usize = 70;

/// Memory per session with its largest process, and the orphaned processes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionsReport {
    pub available_mb: u64,
    /// Largest first.
    pub sessions: Vec<Session>,
    pub orphans: Vec<OrphanGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// What messages address it by.
    pub name: String,
    pub session_id: String,
    pub rss_mb: u64,
    pub largest: Process,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    pub rss_mb: u64,
    /// Its command line, shortened, or with `heads` its head alone.
    pub command: String,
}

/// Processes left by a session that ended, folded into their topmost one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanGroup {
    pub rss_mb: u64,
    pub processes: usize,
    pub session_id: String,
    pub root: Process,
}

/// With `heads`, processes show as events name them: no argument that could
/// hold a credential, for a reader that hands the output to a model.
pub fn sessions(proc_root: &Path, sessions_dir: &Path, heads: bool) -> Result<SessionsReport> {
    let available_mb = memory::available_mb(&proc_root.join("meminfo"))?;
    let att = crate::scan(proc_root, sessions_dir)?;
    let process = |p: &crate::attribution::ProcRef| Process {
        pid: p.pid,
        rss_mb: p.rss_kb / 1024,
        command: if heads {
            p.command_head.clone()
        } else {
            procfs::truncate(&p.cmdline, DISPLAY_CMDLINE)
        },
    };
    Ok(SessionsReport {
        available_mb,
        sessions: att
            .sessions
            .iter()
            .map(|s| Session {
                name: s.name.clone(),
                session_id: s.session_id.clone(),
                rss_mb: s.rss_kb / 1024,
                largest: process(&s.largest),
            })
            .collect(),
        orphans: att
            .orphans
            .iter()
            .map(|o| OrphanGroup {
                rss_mb: o.rss_kb / 1024,
                processes: o.processes,
                session_id: o.session_id.clone(),
                root: process(&o.root),
            })
            .collect(),
    })
}

pub fn sessions_text(r: &SessionsReport) -> String {
    let mut out = format!("Available memory: {} MB\n\n", r.available_mb);
    let _ = writeln!(out, "{:<34} {:>7}  LARGEST PROCESS", "SESSION", "RSS MB");
    for s in &r.sessions {
        let _ = writeln!(
            out,
            "{:<34} {:>7}  {} MB  pid {}  {}",
            s.name, s.rss_mb, s.largest.rss_mb, s.largest.pid, s.largest.command,
        );
    }
    if r.orphans.is_empty() {
        out.push_str("\nNo orphaned process.\n");
    } else {
        out.push_str("\nORPHANS (session gone)\n");
        for o in &r.orphans {
            let _ = writeln!(
                out,
                "{:>7} MB  {} process(es)  pid {}  session {}  {}",
                o.rss_mb,
                o.processes,
                o.root.pid,
                o.session_id.chars().take(8).collect::<String>(),
                o.root.command,
            );
        }
    }
    out
}

/// The configuration file and where the coordinators' instructions are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigReport {
    pub path: PathBuf,
    /// None when there is no file.
    pub config: Option<config::Config>,
    /// None when there is no instructions file.
    pub instructions: Option<PathBuf>,
}

pub fn config(path: &Path) -> Result<ConfigReport> {
    let instructions = config::instructions_path(path);
    Ok(ConfigReport {
        path: path.to_path_buf(),
        config: config::load(path)?,
        instructions: instructions.exists().then_some(instructions),
    })
}

pub fn config_text(r: &ConfigReport) -> Result<String> {
    let mut out = match &r.config {
        Some(config) => format!(
            "{}\n{}\n",
            r.path.display(),
            serde_json::to_string_pretty(config)?
        ),
        None => format!("No configuration at {}.\n", r.path.display()),
    };
    if let Some(instructions) = &r.instructions {
        let _ = writeln!(
            out,
            "The coordinators' instructions: {}",
            instructions.display()
        );
    }
    Ok(out)
}

/// The admission thresholds, the calls waiting for memory and the
/// reservations of running heavy calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionReport {
    pub thresholds: Thresholds,
    pub available_mb: u64,
    /// What running heavy calls still hold of it.
    pub held_mb: u64,
    pub free_mb: u64,
    /// Oldest first.
    pub waiting: Vec<WaitingCall>,
    pub reserved: Vec<ReservedCall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Thresholds {
    On(config::Admission),
    /// Why nothing waits.
    Off(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingCall {
    /// Its session, by what messages address it by and the start of its id,
    /// else by its scope.
    pub session: String,
    pub waited_secs: u64,
    pub peak_mb: u64,
    /// The free memory it waits for.
    pub need_mb: u64,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservedCall {
    /// As in `WaitingCall`.
    pub session: String,
    pub held_mb: u64,
    pub peak_mb: u64,
    /// What its group uses now, None without a memory controller.
    pub current_mb: Option<u64>,
    /// Empty for a reservation written before labels.
    pub command: String,
}

pub fn admission(
    paths: &admission::Paths,
    config_path: &Path,
    sessions_dir: &Path,
) -> Result<AdmissionReport> {
    let snap = admission::snapshot(paths)?;
    let thresholds = match config::load(config_path) {
        Ok(Some(config::Config {
            admission: Some(a), ..
        })) => Thresholds::On(a),
        Ok(_) => Thresholds::Off(format!("no admission section in {}", config_path.display())),
        Err(e) => Thresholds::Off(format!("{e:#}")),
    };
    let known = sessions::read_sessions(sessions_dir).unwrap_or_default();
    let label = |group: &str| session_label(group, &paths.cgroup_root, &known);
    let now = u64::try_from(runtime::now_ms()).unwrap_or(u64::MAX);
    Ok(AdmissionReport {
        thresholds,
        available_mb: snap.available_mb,
        held_mb: snap.held_mb,
        free_mb: snap.free_mb(),
        waiting: snap
            .waiting
            .iter()
            .map(|w| WaitingCall {
                session: label(&w.group),
                waited_secs: now.saturating_sub(w.since_ms) / 1000,
                peak_mb: w.peak_mb,
                need_mb: w.need_mb,
                command: w.label.clone(),
            })
            .collect(),
        reserved: snap
            .reserved
            .iter()
            .map(|r| ReservedCall {
                session: label(&r.group),
                held_mb: r.held_mb,
                peak_mb: r.peak_mb,
                current_mb: r.current_mb,
                command: r.label.clone(),
            })
            .collect(),
    })
}

pub fn admission_text(r: &AdmissionReport) -> String {
    let mut out = match &r.thresholds {
        Thresholds::On(a) => format!(
            "Admission: a call expected to peak at {} MB or more waits until free memory covers its peak plus {} MB, {} s at most.\n",
            a.heavy_mb, a.margin_mb, a.max_wait_secs
        ),
        Thresholds::Off(why) => format!("Admission: off, {why}.\n"),
    };
    let _ = writeln!(
        out,
        "Available memory: {} MB; running heavy calls still hold {} MB of it: {} MB free for admission.\n",
        r.available_mb, r.held_mb, r.free_mb
    );
    if r.waiting.is_empty() {
        out.push_str("No call waits for memory.\n");
    } else {
        out.push_str("WAITING FOR MEMORY\n");
        let _ = writeln!(
            out,
            "  {:<40} {:>7} {:>8} {:>9}  COMMAND",
            "SESSION", "WAITED", "PEAK MB", "NEEDS MB"
        );
        for w in &r.waiting {
            let _ = writeln!(
                out,
                "  {:<40} {:>5} s {:>8} {:>9}  {}",
                w.session, w.waited_secs, w.peak_mb, w.need_mb, w.command
            );
        }
    }
    out.push('\n');
    if r.reserved.is_empty() {
        out.push_str("No heavy call runs with a reservation.\n");
    } else {
        out.push_str("RESERVED BY RUNNING HEAVY CALLS\n");
        let _ = writeln!(
            out,
            "  {:<40} {:>8} {:>8} {:>8}  COMMAND",
            "SESSION", "HOLDS MB", "PEAK MB", "USES MB"
        );
        for c in &r.reserved {
            let _ = writeln!(
                out,
                "  {:<40} {:>8} {:>8} {:>8}  {}",
                c.session,
                c.held_mb,
                c.peak_mb,
                c.current_mb.map_or("?".to_string(), |mb| mb.to_string()),
                if c.command.is_empty() {
                    "?"
                } else {
                    &c.command
                }
            );
        }
    }
    out
}

/// The session a job group belongs to, by what messages address it by and
/// the start of its id, else by its scope.
fn session_label(group: &str, cgroup_root: &Path, known: &[sessions::ClaudeSession]) -> String {
    let scope = cgroup::session_of(group);
    let session = scope
        .and_then(|s| cgroup::main_pid(cgroup_root, s))
        .and_then(|pid| known.iter().find(|s| s.pid == pid));
    match (session, scope) {
        (Some(s), _) => format!(
            "{} ({})",
            messages::address(s),
            s.session_id.chars().take(8).collect::<String>()
        ),
        (None, Some(scope)) => scope.rsplit('/').next().unwrap_or(scope).to_string(),
        (None, None) => group.to_string(),
    }
}

/// The learned peaks, by repository, heaviest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeaksReport {
    /// Only the commands this heavy, when set.
    pub at_least_mb: Option<u64>,
    /// By repository name.
    pub repositories: Vec<RepositoryPeaks>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryPeaks {
    pub repository: String,
    /// Heaviest first.
    pub commands: Vec<CommandPeak>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandPeak {
    pub peak_mb: u64,
    pub id: String,
    pub label: String,
    /// Its latest calls, oldest first: the call's peak in MB, and whether
    /// the command ran alone in it.
    pub recent: Vec<(u64, bool)>,
}

/// With `at_least_mb`, only the commands that heavy; with `most`, only that
/// many of the heaviest.
pub fn peaks(state: &Path, at_least_mb: Option<u64>, most: Option<usize>) -> Result<PeaksReport> {
    let mut chosen: Vec<(String, peaks::Entry)> = peaks::list(&peaks::dir(state))?
        .into_iter()
        .flat_map(|repo| {
            let name = repo.repository;
            repo.commands.into_iter().map(move |c| (name.clone(), c))
        })
        .filter(|(_, c)| at_least_mb.is_none_or(|mb| c.peak_mb >= mb))
        .collect();
    let by_peak = |a: &(String, peaks::Entry), b: &(String, peaks::Entry)| {
        b.1.peak_mb
            .cmp(&a.1.peak_mb)
            .then_with(|| a.1.id.cmp(&b.1.id))
    };
    chosen.sort_by(by_peak);
    if let Some(most) = most {
        chosen.truncate(most);
    }
    chosen.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| by_peak(a, b)));
    let repositories = chosen
        .chunk_by(|a, b| a.0 == b.0)
        .map(|commands| RepositoryPeaks {
            repository: commands[0].0.clone(),
            commands: commands
                .iter()
                .map(|(_, c)| CommandPeak {
                    peak_mb: c.peak_mb,
                    id: c.id.clone(),
                    label: c.label.clone(),
                    recent: c
                        .recent
                        .iter()
                        .map(|peaks::Call(mb, alone)| (*mb, *alone))
                        .collect(),
                })
                .collect(),
        })
        .collect();
    Ok(PeaksReport {
        at_least_mb,
        repositories,
    })
}

/// A command shows as its label; its id's start tells apart two commands
/// with the same label.
pub fn peaks_text(r: &PeaksReport) -> String {
    if r.repositories.is_empty() {
        return match r.at_least_mb {
            Some(mb) => format!("No command learned at {mb} MB or more.\n"),
            None => "No peak learned yet.\n".to_string(),
        };
    }
    let mut out = String::new();
    for repo in &r.repositories {
        let width = repo
            .commands
            .iter()
            .map(|c| c.label.chars().count().min(peaks::LABEL_MAX))
            .fold("COMMAND".len(), usize::max);
        let _ = writeln!(out, "{}", repo.repository);
        let _ = writeln!(
            out,
            "  {:>7}  {:<8}  {:<width$}  LATEST CALLS, MB (* ALONE)",
            "PEAK MB", "ID", "COMMAND",
        );
        for c in &repo.commands {
            let calls: Vec<String> = c
                .recent
                .iter()
                .rev()
                .map(|(mb, alone)| format!("{mb}{}", if *alone { "*" } else { "" }))
                .collect();
            let _ = writeln!(
                out,
                "  {:>7}  {:<8}  {:<width$}  {}",
                c.peak_mb,
                c.id.get(..8).unwrap_or(&c.id),
                procfs::truncate(&c.label, peaks::LABEL_MAX),
                calls.join(" "),
            );
        }
        out.push('\n');
    }
    out
}

/// What the read commands show, wherever it comes from: read here, by
/// `watch`, or asked of `watch` over its API.
pub trait State {
    fn sessions(&self, heads: bool) -> Result<SessionsReport>;
    fn admission(&self) -> Result<AdmissionReport>;
    fn peaks(&self, at_least_mb: Option<u64>, most: Option<usize>) -> Result<PeaksReport>;
    fn machine(&self) -> Result<Machine>;
    fn config(&self) -> Result<ConfigReport>;
}

/// The state as `watch` reads it, from the machine and the files.
pub struct Local(pub Places);

impl State for Local {
    fn sessions(&self, heads: bool) -> Result<SessionsReport> {
        sessions(&self.0.proc_root, &self.0.sessions_dir, heads)
    }

    fn admission(&self) -> Result<AdmissionReport> {
        admission(&self.0.admission, &self.0.config, &self.0.sessions_dir)
    }

    fn peaks(&self, at_least_mb: Option<u64>, most: Option<usize>) -> Result<PeaksReport> {
        peaks(&self.0.state_dir, at_least_mb, most)
    }

    fn machine(&self) -> Result<Machine> {
        machine::read(&self.0.proc_root, &self.0.admission.cgroup_root)
    }

    fn config(&self) -> Result<ConfigReport> {
        config(&self.0.config)
    }
}
