//! Events about one job: `job_pressure`, a job stalled on memory, and
//! `oom_kill`, processes of a job killed for lack of memory (#24).
//!
//! A job stalling is not the job using the memory: under machine-wide
//! pressure, a small job waits on the large one of another session. The
//! allocating job tends to stall the most, since it reclaims before it can
//! allocate, but only a tendency. So `job_pressure` gives what the job uses,
//! and `memory_pressure` the jobs using the most (`JobUsage`).

use serde::Serialize;

/// A job and what it uses now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobUsage {
    /// Its session's name and id, when the session's claude process has a
    /// session file.
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub job: String,
    /// A Bash call's label; None for another job, or a call that cannot be
    /// parsed.
    pub command: Option<String>,
    /// The memory its group is charged, `memory.current`.
    pub memory_mb: u64,
    /// Its process using the most resident memory.
    pub largest: Option<Process>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Process {
    pub pid: u32,
    pub rss_mb: u64,
    pub comm: String,
    /// See `procfs::command_head`.
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobPressure {
    #[serde(flatten)]
    pub job: JobUsage,
    /// The trigger: some task of the job stalled on memory this long within
    /// a PSI window.
    pub stall_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OomKill {
    pub session: Option<String>,
    pub session_id: Option<String>,
    /// The job group, or `main` for the session's claude process and the
    /// tools it runs itself.
    pub job: String,
    /// A Bash call's label.
    pub command: Option<String>,
    /// Processes killed since the job was last read.
    pub killed: u64,
    /// Whether the Bash call that started the job still runs: its result
    /// then shows the kill to its session.
    pub call_running: bool,
}
