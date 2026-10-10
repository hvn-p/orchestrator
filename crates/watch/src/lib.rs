//! Observing sessions and jobs: what each finished job measured, which
//! session each process belongs to, and the events that come of it; and the
//! local API serving that state (`api`).

pub mod api;
pub mod attribution;
pub mod events;
pub mod exits;
pub mod jobs;
pub mod report;

use anyhow::{Context, Result};
use attribution::Attribution;
use claude_code::sessions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use system::procfs;

/// Where the state is read.
#[derive(Debug, Clone)]
pub struct Places {
    pub proc_root: PathBuf,
    pub sessions_dir: PathBuf,
    pub admission: prefix::admission::Paths,
    pub state_dir: PathBuf,
    pub config: PathBuf,
}

impl Places {
    /// The places the read commands use.
    pub fn from_env() -> Result<Places> {
        let proc_root = PathBuf::from("/proc");
        Ok(Places {
            sessions_dir: claude_code::sessions::default_dir()
                .ok_or_else(|| anyhow::anyhow!("neither CLAUDE_CONFIG_DIR nor HOME is set"))?,
            admission: prefix::admission::Paths {
                cgroup_root: PathBuf::from(system::cgroup::ROOT),
                meminfo: proc_root.join("meminfo"),
                runtime: system::runtime::default_dir()?,
            },
            proc_root,
            state_dir: system::state::default_dir()?,
            config: config::default_path()?,
        })
    }
}

/// Reads processes and sessions, then attributes. A missing sessions
/// directory means no Claude session has run yet.
pub fn scan(proc_root: &Path, sessions_dir: &Path) -> Result<Attribution> {
    let procs = procfs::read_processes(proc_root, sessions::ID_VAR)
        .with_context(|| format!("reading {}", proc_root.display()))?;
    let sessions = match sessions::read_sessions(sessions_dir) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", sessions_dir.display())),
    };
    Ok(attribution::attribute(&procs, &sessions))
}
