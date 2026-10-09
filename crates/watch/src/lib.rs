//! Observing sessions and jobs: what each finished job measured, which
//! session each process belongs to, and the events that come of it.

pub mod attribution;
pub mod events;
pub mod exits;
pub mod jobs;
pub mod report;

use anyhow::{Context, Result};
use attribution::Attribution;
use claude_code::sessions;
use std::io::ErrorKind;
use std::path::Path;
use system::procfs;

/// Reads processes and sessions, then attributes. A missing sessions
/// directory means no Claude session has run yet.
pub fn scan(proc_root: &Path, sessions_dir: &Path) -> Result<Attribution> {
    let procs = procfs::read_processes(proc_root)
        .with_context(|| format!("reading {}", proc_root.display()))?;
    let sessions = match sessions::read_sessions(sessions_dir) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", sessions_dir.display())),
    };
    Ok(attribution::attribute(&procs, &sessions))
}
