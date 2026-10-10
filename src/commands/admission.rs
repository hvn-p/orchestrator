//! `orchestrator admission`: the calls waiting for memory, and the
//! reservations.

use super::watch_api;
use anyhow::Result;
use watch::report::{self, State};

pub fn run() -> Result<()> {
    print!("{}", report::admission_text(&watch_api()?.admission()?));
    Ok(())
}
