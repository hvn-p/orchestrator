//! orchestrator: schedules the work of the Claude Code sessions running in
//! parallel on one machine, so development keeps going. See docs/design.md.

mod commands;
mod launch;
mod watching;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    version,
    about = "Keep the parallel Claude Code sessions of this machine within its memory"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Watch memory pressure, session ends and job ends, append events, and learn the memory peak of each Bash call.
    Watch(commands::watch::WatchArgs),
    /// Print memory per Claude session and the orphaned processes.
    Sessions(commands::sessions::SessionsArgs),
    /// Print the memory peaks learned per repository and command.
    Peaks(commands::peaks::StateDir),
    /// Print the Bash calls waiting for memory and the memory reserved by running ones.
    Admission,
    /// Print what the configuration is chosen from: memory, swap, CPUs, cgroup delegation.
    Machine,
    /// Print the configuration, or set a section of it.
    Config(commands::config::ConfigArgs),
    /// Set orchestrator up in a conversation with the coordinator.
    Setup,
    /// Open an interactive coordinator, which receives the events while it stays open; without a configuration, set orchestrator up first.
    Coordinator(commands::coordinator::CoordinatorArgs),
    /// Start a command, normally `claude`, as an orchestrated session.
    Launch(commands::launch::Launched),
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Watch(args) => commands::watch::run(args),
        Command::Sessions(args) => commands::sessions::run(&args),
        Command::Peaks(dir) => commands::peaks::run(dir),
        Command::Admission => commands::admission::run(),
        Command::Machine => commands::machine::run(),
        Command::Config(args) => commands::config::run(args),
        Command::Setup => commands::setup::run(),
        Command::Coordinator(args) => commands::coordinator::run(args),
        Command::Launch(l) => Err(commands::launch::run(&l)),
    }
}
