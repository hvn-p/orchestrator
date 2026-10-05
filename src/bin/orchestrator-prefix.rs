//! `orchestrator-prefix '<command>'`: the shell prefix Claude Code runs every
//! command through. See `orchestrator::prefix`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(command), None) = (args.next(), args.next()) else {
        eprintln!("usage: orchestrator-prefix '<command>'");
        return ExitCode::from(2);
    };
    let err = orchestrator::prefix::run(&command);
    eprintln!("orchestrator-prefix: {err:#}");
    ExitCode::from(127)
}
