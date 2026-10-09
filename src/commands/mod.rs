//! One module per subcommand of `orchestrator`: its arguments and what it
//! runs. What several of them share lives here.

pub mod admission;
pub mod config;
pub mod coordinator;
pub mod launch;
pub mod machine;
pub mod peaks;
pub mod sessions;
pub mod setup;
pub mod watch;

use anyhow::{Context, Result};
use clap::Args;
use std::path::PathBuf;
use system::{runtime, state};

/// Where this process's own and other processes' state is read.
pub const PROC: &str = "/proc";

#[derive(Args)]
pub struct Sources {
    /// The proc file system to read: processes and meminfo, and for `watch` also pressure/memory and its own cgroup.
    #[arg(long, default_value = PROC)]
    pub proc_root: PathBuf,
    /// Claude Code's sessions directory [default: `$CLAUDE_CONFIG_DIR/sessions`, else `~/.claude/sessions`].
    #[arg(long)]
    pub sessions_dir: Option<PathBuf>,
}

pub fn sessions_dir(sources: &Sources) -> Result<PathBuf> {
    match &sources.sessions_dir {
        Some(dir) => Ok(dir.clone()),
        None => claude_code::sessions::default_dir()
            .context("neither CLAUDE_CONFIG_DIR nor HOME is set"),
    }
}

pub fn state_dir(arg: Option<PathBuf>) -> Result<PathBuf> {
    match arg {
        Some(dir) => Ok(dir),
        None => state::default_dir(),
    }
}

pub fn now_secs() -> u64 {
    u64::try_from(runtime::now_ms() / 1000).unwrap_or(u64::MAX)
}
