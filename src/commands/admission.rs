//! `orchestrator admission`: the calls waiting for memory, and the
//! reservations; or which waiting calls have priority.

use super::watch_api;
use anyhow::Result;
use clap::{Args, Subcommand};
use prefix::admission;
use std::fmt::Write as _;
use watch::Places;
use watch::report::{self, State};

#[derive(Args)]
pub struct AdmissionArgs {
    #[command(subcommand)]
    command: Option<AdmissionCommand>,
}

#[derive(Subcommand)]
enum AdmissionCommand {
    /// Give these waiting calls priority, in this order, in place of those that had it; none without a job. No call passes a call given priority, which keeps it until it runs or is refused.
    Priority {
        /// The job of each call, as `orchestrator admission` shows it.
        jobs: Vec<String>,
    },
}

pub fn run(args: AdmissionArgs) -> Result<()> {
    match args.command {
        None => print!("{}", report::admission_text(&watch_api()?.admission()?)),
        Some(AdmissionCommand::Priority { jobs }) => {
            let given = admission::set_priority(&Places::from_env()?.admission, &jobs)?;
            print!("{}", priority_text(&given));
        }
    }
    Ok(())
}

fn priority_text(given: &[admission::Waiting]) -> String {
    if given.is_empty() {
        return "No call has priority: waiting calls go by arrival.\n".into();
    }
    let mut out = String::from(
        "Priority, in this order; no call passes these, and the others follow by arrival:\n",
    );
    for (i, w) in given.iter().enumerate() {
        let _ = writeln!(out, "  {}. {} `{}`", i + 1, w.job, w.label);
    }
    out
}
