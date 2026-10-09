//! `orchestrator admission`: the calls waiting for memory, and the
//! reservations.

use anyhow::Result;
use coordinator::state::Places;
use watch::report;

pub fn run() -> Result<()> {
    let places = Places::from_env()?;
    print!(
        "{}",
        report::admission(&places.admission, &places.config, &places.sessions_dir)?
    );
    Ok(())
}
