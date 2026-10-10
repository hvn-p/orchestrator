//! `orchestrator machine`: what the configuration is chosen from.

use super::watch_api;
use anyhow::Result;
use system::machine;
use watch::report::State;

pub fn run() -> Result<()> {
    print!("{}", machine::describe(&watch_api()?.machine()?));
    Ok(())
}
