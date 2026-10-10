//! orchestrator keeps the Claude Code sessions running in parallel on one
//! Linux machine working within its finite resources. It schedules their
//! work rather than refusing it: a job may take longer, it is not prevented.
//!
//! Anything that can be decided without judgment is done by code and spends
//! no tokens. Judgment (priorities, exceptions, negotiating with a session)
//! is left to the coordinator, a Claude Code session that reads
//! orchestrator's events. Rust was chosen for a strict compiler, a start
//! time around a millisecond (the shell prefix runs before every command of
//! every session) and a resident `watch` of a few MB.
//!
//! # Components
//!
//! A Cargo workspace. This crate holds the two binaries, `orchestrator` and
//! `orchestrator-prefix`, installed side by side; each component is a crate
//! of `crates/`, which the compiler keeps from reaching into the others.
//!
//! - `system`: cgroups, `/proc`, memory and memory pressure, and where
//!   orchestrator keeps its files. It knows nothing of Claude Code.
//! - `claude-code`: everything orchestrator relies on in Claude Code, in
//!   orchestrator's terms. No other crate reads or writes a Claude Code
//!   format.
//! - `config`: the configuration, a section per feature.
//! - `learning`: recognising a command and learning its memory peak, per
//!   repository.
//! - `prefix`: the shell prefix every command of a session goes through, and
//!   admission, which holds a heavy Bash call back until memory covers it.
//! - `watch`: observing sessions and jobs, the events, the reports, and the
//!   local API serving them.
//! - `coordinator`: a fresh Claude Code session for each batch of events
//!   that need judgment.
//! - This crate: a module per subcommand (`commands`), `launch`, which
//!   starts a session in a cgroup of its own, and the loop of
//!   `orchestrator watch` (`watching`), which ties observation to the
//!   coordinator.
//!
//! A session's cgroup tree:
//!
//! ```text
//! orchestrator-<pid>-<ms>.scope   a delegated systemd user scope
//! ├─ main/                        claude itself
//! ├─ job-bash-<pid>-<ms>/         one Bash call, with every process it starts
//! └─ job-other-<pid>-<ms>/        one hook, status line refresh or MCP server
//! ```
//!
//! The prefix talks to the rest through files in the runtime directory: it
//! writes job records, reservations and waiting calls, and reads the learned
//! peaks and the configuration, so that a command never waits on another
//! process. `orchestrator watch` owns the state: it measures and learns,
//! writes the events, and serves the state and the events over a local API
//! (`watch::api`). Every other read of the state goes through it: without a
//! `watch`, nothing answers. A coordinator acts through the commands it is
//! allowed to run. Runtime data lives in `$XDG_RUNTIME_DIR/orchestrator/`,
//! learned peaks in `$XDG_STATE_HOME/orchestrator/`, the configuration in
//! `$XDG_CONFIG_HOME/orchestrator/`.
//!
//! # Principles
//!
//! - **Never block work by failing**: outside an orchestrated session,
//!   without a configuration, or on any cgroup error, the prefix runs the
//!   command unchanged. When `launch` cannot get the session its scope, the
//!   session starts unorchestrated.
//! - **Commands typed outside Claude Code never wait**: they run outside any
//!   orchestrated session. A `!` command typed inside Claude Code is a Bash
//!   call like any other.
//! - **Hooks, the status line and MCP servers never queue**: they get a job
//!   group of their own and start at once.
//! - **Commands are known by what they used, never by name**: a hook or a
//!   shim in `PATH` sees `pnpm typecheck`, not the `tsc` processes it
//!   starts, and `node_modules/.bin` bypasses shims. The kernel puts every
//!   descendant of a command in its job group, whatever its name, language
//!   or depth.
//! - **No full command line in an event**: arguments read from `/proc` can
//!   hold credentials, and the coordinator hands what it reads to a model
//!   (see `watch::events`).
//! - **Claude Code is today's only host, in one crate**: `claude-code` lists
//!   each contract orchestrator relies on, and holds the code relying on it.
//!
//! Decisions and measurements made before the GitHub issues held them are
//! in the design document as it stood when it was removed:
//! <https://github.com/hvn-p/orchestrator/blob/957d2f9f119c7d0fb59c4e13552aeb6ce8a2b1fc/docs/design.md>.

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
    Peaks,
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
        Command::Peaks => commands::peaks::run(),
        Command::Admission => commands::admission::run(),
        Command::Machine => commands::machine::run(),
        Command::Config(args) => commands::config::run(args),
        Command::Setup => commands::setup::run(),
        Command::Coordinator(args) => commands::coordinator::run(args),
        Command::Launch(l) => Err(commands::launch::run(&l)),
    }
}
