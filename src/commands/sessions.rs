//! `orchestrator sessions`: memory per Claude session, and orphans.

use super::{Sources, sessions_dir};
use anyhow::Result;
use clap::Args;
use watch::report;

#[derive(Args)]
pub struct SessionsArgs {
    #[command(flatten)]
    sources: Sources,
    /// Show each process by the head of its command line, as events do, never its arguments.
    #[arg(long)]
    heads: bool,
}

pub fn run(args: &SessionsArgs) -> Result<()> {
    let dir = sessions_dir(&args.sources)?;
    print!(
        "{}",
        report::sessions(&args.sources.proc_root, &dir, args.heads)?
    );
    Ok(())
}
