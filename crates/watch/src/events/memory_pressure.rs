//! `memory_pressure`: tasks stalled on memory, with the sessions and the jobs
//! using the most.

use super::JobUsage;
use crate::attribution::{Attribution, SessionUsage};
use serde::Serialize;

/// Sessions listed after the largest one.
const NEXT: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryPressure {
    pub available_mb: u64,
    /// The trigger: some task stalled on memory this long within a PSI
    /// window.
    pub stall_ms: u64,
    pub largest: Option<SessionSummary>,
    pub next: Vec<SessionBrief>,
    /// The live jobs using the most memory, across sessions, most first.
    pub jobs: Vec<JobUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionSummary {
    pub session: String,
    pub session_id: String,
    pub rss_mb: u64,
    /// The session's largest process.
    pub process_pid: u32,
    pub process_rss_mb: u64,
    pub process_comm: String,
    pub process_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionBrief {
    pub session: String,
    pub rss_mb: u64,
}

impl MemoryPressure {
    pub fn new(available_mb: u64, stall_ms: u64, att: &Attribution, jobs: Vec<JobUsage>) -> Self {
        MemoryPressure {
            available_mb,
            stall_ms,
            largest: att.sessions.first().map(summary),
            next: att
                .sessions
                .iter()
                .skip(1)
                .take(NEXT)
                .map(|s| SessionBrief {
                    session: s.name.clone(),
                    rss_mb: s.rss_kb / 1024,
                })
                .collect(),
            jobs,
        }
    }
}

fn summary(s: &SessionUsage) -> SessionSummary {
    SessionSummary {
        session: s.name.clone(),
        session_id: s.session_id.clone(),
        rss_mb: s.rss_kb / 1024,
        process_pid: s.largest.pid,
        process_rss_mb: s.largest.rss_kb / 1024,
        process_comm: s.largest.comm.clone(),
        process_command: s.largest.command_head.clone(),
    }
}
