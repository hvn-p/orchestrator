//! `orchestrator-prefix '<command>'`: the shell prefix Claude Code runs every
//! command through. See the `prefix` crate.
//!
//! A binary of its own rather than a subcommand of `orchestrator`: Claude
//! Code runs the prefix as one quoted path, so a prefix holding an argument
//! is not found.

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let err = match claude_code::invocation::command(&args) {
        Some(command) => prefix::run(command),
        None => match prefix::run_unexpected(&args) {
            Some(err) => err,
            None => return ExitCode::SUCCESS,
        },
    };
    eprintln!("orchestrator-prefix: {err:#}");
    ExitCode::from(127)
}
