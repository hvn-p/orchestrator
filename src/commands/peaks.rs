//! `orchestrator peaks`: the memory peaks learned.

use super::state_dir;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;
use watch::report;

#[derive(Args)]
pub struct StateDir {
    /// Where learned peaks are kept [default: `$XDG_STATE_HOME/orchestrator`, else `~/.local/state/orchestrator`].
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

pub fn run(dir: StateDir) -> Result<()> {
    print!("{}", report::peaks(&state_dir(dir.state_dir)?, None, None)?);
    Ok(())
}
