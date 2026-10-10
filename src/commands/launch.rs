//! `orchestrator launch`: start a command as an orchestrated session. See
//! `crate::launch`.

use clap::Args;
use std::ffi::OsString;

#[derive(Args)]
pub struct Launched {
    /// The command and its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

/// Only returns on an error.
pub fn run(l: &Launched) -> anyhow::Error {
    crate::launch::launch(&l.command)
}
