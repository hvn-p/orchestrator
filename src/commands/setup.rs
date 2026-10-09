//! `orchestrator setup`: set orchestrator up in a conversation with the
//! coordinator.

use super::coordinator::become_coordinator;
use anyhow::Result;
use coordinator::run;

/// Only returns on an error.
pub fn run() -> Result<()> {
    let configured = config::load(&config::default_path()?)?.is_some();
    Err(become_coordinator(&run::Mode::Setup { configured }))
}
