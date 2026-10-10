//! `orchestrator peaks`: the memory peaks learned.

use super::watch_api;
use anyhow::Result;
use watch::report::{self, State};

pub fn run() -> Result<()> {
    print!("{}", report::peaks_text(&watch_api()?.peaks(None, None)?));
    Ok(())
}
