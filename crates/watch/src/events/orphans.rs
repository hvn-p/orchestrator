//! `orphans`: processes left by sessions that ended.

use crate::attribution::Orphan;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Orphans {
    pub orphans: Vec<OrphanSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrphanSummary {
    pub pid: u32,
    pub session_id: String,
    pub rss_mb: u64,
    pub processes: usize,
    pub comm: String,
    pub command: String,
}

impl Orphans {
    pub fn new(orphans: &[&Orphan]) -> Self {
        Orphans {
            orphans: orphans
                .iter()
                .map(|o| OrphanSummary {
                    pid: o.root.pid,
                    session_id: o.session_id.clone(),
                    rss_mb: o.rss_kb / 1024,
                    processes: o.processes,
                    comm: o.root.comm.clone(),
                    command: o.root.command_head.clone(),
                })
                .collect(),
        }
    }
}
