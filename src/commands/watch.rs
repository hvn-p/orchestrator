//! `orchestrator watch`: the long-running process. See `crate::watching`.

use super::{Sources, sessions_dir, state_dir};
use crate::watching;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;
use std::time::Duration;
use system::runtime;

#[derive(Args)]
pub struct WatchArgs {
    #[command(flatten)]
    sources: Sources,
    /// Where learned peaks are kept [default: `$XDG_STATE_HOME/orchestrator`, else `~/.local/state/orchestrator`]. The prefix always reads the default: with another directory, admission never sees what `watch` learns.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Where events and measurements are written and job records read [default: `$XDG_RUNTIME_DIR/orchestrator`, else `/run/user/<uid>/orchestrator`]. The prefix always writes its job records to the default: with another directory, no Bash call is measured.
    #[arg(long)]
    runtime_dir: Option<PathBuf>,
    /// Memory stall, within a 2 s window, that makes a pressure event, of the machine or of a job, in ms.
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u64).range(1..=2000))]
    stall_ms: u64,
    /// Minimum seconds between two memory pressure events, and between two job pressure events of one job.
    #[arg(long, default_value_t = 60)]
    cooldown_secs: u64,
    /// Seconds between two orphan scans when no session ends; 0 counts as 1.
    #[arg(long, default_value_t = 300)]
    orphan_interval_secs: u64,
}

pub fn run(args: WatchArgs) -> Result<()> {
    watching::run(&watching::Options {
        sessions_dir: sessions_dir(&args.sources)?,
        proc_root: args.sources.proc_root,
        runtime_dir: match args.runtime_dir {
            Some(dir) => dir,
            None => runtime::default_dir()?,
        },
        state_dir: state_dir(args.state_dir)?,
        config_path: config::default_path()?,
        orphan_interval: Duration::from_secs(args.orphan_interval_secs.max(1)),
        thresholds: watching::Thresholds {
            stall_ms: args.stall_ms,
            cooldown_secs: args.cooldown_secs,
        },
    })
}
