//! `orchestrator setup`: set orchestrator up in a conversation with the
//! coordinator.

use super::coordinator::become_coordinator;
use super::watch_api;
use anyhow::Result;
use coordinator::run;
use watch::report::State;

/// Only returns on an error.
pub fn run() -> Result<()> {
    let configured = watch_api()?.config()?.config.is_some();
    Err(become_coordinator(&run::Mode::Setup { configured }))
}
