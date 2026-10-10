//! `orchestrator sessions`: memory per Claude session, and orphans.

use super::watch_api;
use anyhow::Result;
use clap::Args;
use watch::report::{self, State};

#[derive(Args)]
pub struct SessionsArgs {
    /// Show each process by the head of its command line, as events do, never its arguments.
    #[arg(long)]
    heads: bool,
}

pub fn run(args: &SessionsArgs) -> Result<()> {
    print!(
        "{}",
        report::sessions_text(&watch_api()?.sessions(args.heads)?)
    );
    Ok(())
}
