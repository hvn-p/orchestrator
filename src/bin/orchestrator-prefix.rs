//! `orchestrator-prefix '<command>'`: the shell prefix Claude Code runs every
//! command through. See `orchestrator::prefix`.

use std::ffi::OsString;
use std::process::ExitCode;

// claude-code: shell-prefix-argument
fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let err = match args.as_slice() {
        [command] => orchestrator::prefix::run(command),
        other => match orchestrator::prefix::run_unexpected(other) {
            Some(err) => err,
            None => return ExitCode::SUCCESS,
        },
    };
    eprintln!("orchestrator-prefix: {err:#}");
    ExitCode::from(127)
}
