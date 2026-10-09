//! `orchestrator coordinator`: open an interactive coordinator, wait for its
//! next events, or note in its journal.

use super::{PROC, now_secs};
use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use config::Coordinator;
use config::language;
use coordinator::state::{self as briefing, Places};
use coordinator::{Holder, journal, run};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct CoordinatorArgs {
    #[command(subcommand)]
    command: Option<CoordinatorCommand>,
}

#[derive(Subcommand)]
enum CoordinatorCommand {
    /// Close the batch taken before, wait until events are pending, then print them with the state.
    Next,
    /// Add a line to the coordinator's journal.
    Note {
        /// What to note; each line becomes a line of the journal.
        #[arg(required = true)]
        text: Vec<String>,
    },
}

pub fn run(args: CoordinatorArgs) -> Result<()> {
    match args.command {
        None => {
            // A first-time user needs this one command: without a
            // configuration, setting up comes first.
            let mode = if config::load(&config::default_path()?)?.is_some() {
                run::Mode::Interactive
            } else {
                run::Mode::Setup { configured: false }
            };
            Err(become_coordinator(&mode))
        }
        Some(CoordinatorCommand::Next) => next_events(),
        Some(CoordinatorCommand::Note { text }) => {
            let paths = coordinator::Paths::from_env()?;
            journal::note(&paths.journal(), &text.join(" "), now_secs())
        }
    }
}

/// Opens a coordinator in `mode` for the user at this terminal: takes the
/// coordinator, waiting for a running one to end, then replaces this
/// process with `claude`. The holder's pid and start time stay this
/// process's, so the coordinator frees itself when claude exits. Only
/// returns on an error.
pub fn become_coordinator(mode: &run::Mode<'_>) -> anyhow::Error {
    let started = || -> Result<std::process::Command> {
        let places = Places::from_env()?;
        let paths = briefing::paths(&places);
        let proc_root = Path::new(PROC);
        let me =
            Holder::of(proc_root, std::process::id()).context("reading this process's start")?;
        coordinator::acquire(&paths, proc_root, me)?;
        run::write_role(&paths, mode)?;
        let prompt = run::prompt(
            mode,
            &paths,
            now_secs(),
            &briefing::briefing(&places, &paths, &system_language()),
        );
        // An interactive coordinator runs with the user's model.
        let cfg = Coordinator::default();
        Ok(run::command(mode, &cfg, &paths, &bin_dir()?, &prompt))
    };
    match started() {
        Ok(mut cmd) => anyhow!(cmd.exec()).context("running claude"),
        Err(e) => e,
    }
}

/// The batch an interactive coordinator asks for, with the state now.
fn next_events() -> Result<()> {
    let places = Places::from_env()?;
    let paths = briefing::paths(&places);
    let taken = coordinator::next(&paths, &places.admission)?;
    println!("## Events\n");
    for q in &taken {
        println!("{}", q.line());
    }
    print!("\n## State now\n\n{}", briefing::gather(&places));
    Ok(())
}

/// This binary's directory, first on a coordinator's `PATH`.
fn bin_dir() -> Result<PathBuf> {
    Ok(std::env::current_exe()
        .context("locating orchestrator")?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default())
}

/// The language of the user at this terminal, from the locale.
fn system_language() -> String {
    language::of_system(|name| std::env::var(name).ok())
}
