//! orchestrator: schedules the work of the Claude Code sessions running in
//! parallel on one machine, so development keeps going. See docs/design.md.

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use orchestrator::config::{self, Admission, Coordinator};
use orchestrator::coordinator::state::{self as briefing, Places};
use orchestrator::coordinator::{self, Holder, journal, run};
use orchestrator::{cgroup, language, launch, machine, report, runtime, sessions, state, watch};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where this process's own and other processes' state is read.
const PROC: &str = "/proc";

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
    Watch(WatchArgs),
    /// Print memory per Claude session and the orphaned processes.
    Sessions(SessionsArgs),
    /// Print the memory peaks learned per repository and command.
    Peaks(StateDir),
    /// Print the Bash calls waiting for memory and the memory reserved by running ones.
    Admission,
    /// Print what the configuration is chosen from: memory, swap, CPUs, cgroup delegation.
    Machine,
    /// Print the configuration, or set a section of it.
    Config(ConfigArgs),
    /// Set orchestrator up in a conversation with the coordinator.
    Setup,
    /// Open an interactive coordinator, which receives the events while it stays open; without a configuration, set orchestrator up first.
    Coordinator(CoordinatorArgs),
    /// Start a command, normally `claude`, as an orchestrated session.
    Launch(Launched),
}

#[derive(Args)]
struct Launched {
    /// The command and its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

#[derive(Args)]
struct Sources {
    /// The proc file system to read: processes and meminfo, and for `watch` also pressure/memory and its own cgroup.
    #[arg(long, default_value = PROC)]
    proc_root: PathBuf,
    /// Claude Code's sessions directory [default: `$CLAUDE_CONFIG_DIR/sessions`, else `~/.claude/sessions`].
    #[arg(long)]
    sessions_dir: Option<PathBuf>,
}

#[derive(Args)]
struct SessionsArgs {
    #[command(flatten)]
    sources: Sources,
    /// Show each process by the head of its command line, as events do, never its arguments.
    #[arg(long)]
    heads: bool,
}

#[derive(Args)]
struct StateDir {
    /// Where learned peaks are kept [default: `$XDG_STATE_HOME/orchestrator`, else `~/.local/state/orchestrator`].
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

#[derive(Args)]
struct WatchArgs {
    #[command(flatten)]
    sources: Sources,
    /// Where learned peaks are kept [default: `$XDG_STATE_HOME/orchestrator`, else `~/.local/state/orchestrator`]. The prefix always reads the default: with another directory, admission never sees what `watch` learns.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Where events and measurements are written and job records read [default: `$XDG_RUNTIME_DIR/orchestrator`, else `/run/user/<uid>/orchestrator`]. The prefix always writes its job records to the default: with another directory, no Bash call is measured.
    #[arg(long)]
    runtime_dir: Option<PathBuf>,
    /// Memory stall, within a 2 s window, that makes a pressure event, in ms.
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u64).range(1..=2000))]
    stall_ms: u64,
    /// Minimum seconds between two memory pressure events.
    #[arg(long, default_value_t = 60)]
    cooldown_secs: u64,
    /// Seconds between two orphan scans when no session ends; 0 counts as 1.
    #[arg(long, default_value_t = 300)]
    orphan_interval_secs: u64,
}

#[derive(Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: Option<ConfigCommand>,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Set the admission thresholds, once they make sense on this machine, keeping the rest.
    Admission(AdmissionArgs),
    /// Set the coordinator: whether `watch` may wake it, its model and bounds, keeping the rest.
    Coordinator(CoordinatorConfigArgs),
}

#[derive(Args)]
#[group(required = true, multiple = true)]
struct CoordinatorConfigArgs {
    /// Whether `watch` may start a coordinator by itself for events (yes or no).
    #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
    wake: Option<bool>,
    /// The model of the runs `watch` starts.
    #[arg(long)]
    model: Option<String>,
    /// The longest a run may last, in minutes.
    #[arg(long)]
    max_minutes: Option<u64>,
    /// How long admission holds a call before it wakes the coordinator, in seconds.
    #[arg(long)]
    wait_secs: Option<u64>,
    /// The language the coordinator writes in: a tag such as fr, en or pt-BR.
    #[arg(long)]
    language: Option<String>,
}

#[derive(Args)]
struct AdmissionArgs {
    /// A Bash call whose expected peak reaches this, in MB, waits for memory.
    #[arg(long)]
    heavy_mb: u64,
    /// Free memory kept on top of a heavy call's expected peak, in MB.
    #[arg(long)]
    margin_mb: u64,
    /// The longest a call waits before it runs anyway, in seconds.
    #[arg(long)]
    max_wait_secs: u64,
}

#[derive(Args)]
struct CoordinatorArgs {
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

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Watch(args) => watch::run(&watch::Config {
            sessions_dir: sessions_dir(&args.sources)?,
            proc_root: args.sources.proc_root,
            runtime_dir: match args.runtime_dir {
                Some(dir) => dir,
                None => runtime::default_dir()?,
            },
            state_dir: state_dir(args.state_dir)?,
            config_path: config::default_path()?,
            orphan_interval: Duration::from_secs(args.orphan_interval_secs.max(1)),
            thresholds: watch::Thresholds {
                stall_ms: args.stall_ms,
                cooldown_secs: args.cooldown_secs,
            },
        }),
        Command::Sessions(args) => {
            let dir = sessions_dir(&args.sources)?;
            print!(
                "{}",
                report::sessions(&args.sources.proc_root, &dir, args.heads)?
            );
            Ok(())
        }
        Command::Peaks(dir) => {
            print!("{}", report::peaks(&state_dir(dir.state_dir)?, None, None)?);
            Ok(())
        }
        Command::Admission => {
            let places = Places::from_env()?;
            print!(
                "{}",
                report::admission(&places.admission, &places.config, &places.sessions_dir)?
            );
            Ok(())
        }
        Command::Machine => {
            let m = machine::read(Path::new(PROC), Path::new(cgroup::ROOT))?;
            print!("{}", machine::describe(&m));
            Ok(())
        }
        Command::Config(args) => match args.command {
            None => print_config(),
            Some(ConfigCommand::Admission(a)) => set_admission(&a),
            Some(ConfigCommand::Coordinator(c)) => set_coordinator(c),
        },
        Command::Setup => {
            let configured = config::load(&config::default_path()?)?.is_some();
            Err(become_coordinator(&run::Mode::Setup { configured }))
        }
        Command::Coordinator(args) => match args.command {
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
        },
        Command::Launch(l) => Err(launch::launch(&l.command)),
    }
}

fn sessions_dir(sources: &Sources) -> Result<PathBuf> {
    match &sources.sessions_dir {
        Some(dir) => Ok(dir.clone()),
        None => default_sessions_dir(),
    }
}

fn default_sessions_dir() -> Result<PathBuf> {
    sessions::default_dir().context("neither CLAUDE_CONFIG_DIR nor HOME is set")
}

fn state_dir(arg: Option<PathBuf>) -> Result<PathBuf> {
    match arg {
        Some(dir) => Ok(dir),
        None => state::default_dir(),
    }
}

fn print_config() -> Result<()> {
    print!("{}", report::config(&config::default_path()?)?);
    Ok(())
}

fn set_admission(a: &AdmissionArgs) -> Result<()> {
    let total_mb = machine::read(Path::new(PROC), Path::new(cgroup::ROOT))?.mem_total_mb;
    let path = config::default_path()?;
    let admission = Admission {
        heavy_mb: a.heavy_mb,
        margin_mb: a.margin_mb,
        max_wait_secs: a.max_wait_secs,
    };
    let config = config::set_admission(&path, admission, total_mb)?;
    println!(
        "Wrote {}\n{}",
        path.display(),
        serde_json::to_string_pretty(&config).context("serializing the configuration")?
    );
    Ok(())
}

fn set_coordinator(c: CoordinatorConfigArgs) -> Result<()> {
    let path = config::default_path()?;
    let config = config::set_coordinator(&path, |section| {
        if let Some(wake) = c.wake {
            section.wake = wake;
        }
        if let Some(model) = c.model {
            section.model = model;
        }
        if let Some(minutes) = c.max_minutes {
            section.max_minutes = minutes;
        }
        if let Some(secs) = c.wait_secs {
            section.wait_secs = secs;
        }
        if let Some(tag) = c.language {
            section.language = Some(tag);
        }
    })?;
    println!(
        "Wrote {}\n{}",
        path.display(),
        serde_json::to_string_pretty(&config).context("serializing the configuration")?
    );
    Ok(())
}

/// Opens a coordinator in `mode` for the user at this terminal: takes the
/// coordinator, waiting for a running one to end, then replaces this
/// process with `claude`. The holder's pid and start time stay this
/// process's, so the coordinator frees itself when claude exits. Only
/// returns on an error.
fn become_coordinator(mode: &run::Mode<'_>) -> anyhow::Error {
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

fn now_secs() -> u64 {
    u64::try_from(runtime::now_ms() / 1000).unwrap_or(u64::MAX)
}
