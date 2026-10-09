//! `orchestrator machine`: what the configuration is chosen from.

use super::PROC;
use anyhow::Result;
use std::path::Path;
use system::{cgroup, machine};

pub fn run() -> Result<()> {
    let m = machine::read(Path::new(PROC), Path::new(cgroup::ROOT))?;
    print!("{}", machine::describe(&m));
    Ok(())
}
