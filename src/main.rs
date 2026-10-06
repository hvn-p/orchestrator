//! orchestrator: schedules the work of the Claude Code sessions running in
//! parallel on one machine, so development keeps going. See docs/design.md.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use orchestrator::config::{self, Admission, Coordinator};
use orchestrator::coordinator::state::{self as briefing, Places};
use orchestrator::coordinator::{self, Holder, journal, run};
use orchestrator::{cgroup, launch, machine, report, runtime, sessions, state, watch};
use rustix::process::{Pid, Signal, kill_process};
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where this process's own and other processes' state is read.
const PROC: &str = "/proc";

#[derive(Parser)]
#[command(
    version,
    about = "Schedule the work of the parallel Claude Code sessions of this machine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Watch memory pressure, session ends and job ends, and append the events that drive scheduling.
    Watch(WatchArgs),
    /// Print memory per Claude session and the orphaned processes.
    Sessions(SessionsArgs),
    /// Print the memory peaks learned per repository and command.
    Peaks(StateDir),
    /// Print the Bash calls waiting for memory and the memory reserved by running ones.
    Admission,
    /// Print what the configuration is chosen from: memory, swap, CPUs, cgroup delegation.
    Machine,
    /// Print the configuration, or set its admission thresholds.
    Config(ConfigArgs),
    /// Enable the coordinator, then have one examine the machine and write the admission thresholds.
    Setup(SetupArgs),
    /// Open an interactive coordinator, which receives the events while it stays open.
    Coordinator(CoordinatorArgs),
    /// Start a command, normally `claude`, as an orchestrated session.
    Launch(Launched),
    /// Set up a new session scope, then run its command. Started by `launch`.
    #[command(hide = true)]
    Enter(Launched),
}

#[derive(Args)]
struct Launched {
    /// The command and its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

#[derive(Args)]
struct Sources {
    /// Where to read processes and meminfo from.
    #[arg(long, default_value = PROC)]
    proc_root: PathBuf,
    /// Claude Code's sessions directory [default: ~/.claude/sessions].
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
    /// Where learned peaks are kept [default: `$XDG_STATE_HOME/orchestrator`].
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

#[derive(Args)]
struct WatchArgs {
    #[command(flatten)]
    sources: Sources,
    #[command(flatten)]
    state: StateDir,
    /// Where the events file is written [default: `$XDG_RUNTIME_DIR/orchestrator`].
    #[arg(long)]
    runtime_dir: Option<PathBuf>,
    /// Memory stall, within a 2 s window, that makes a pressure event, in ms.
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u64).range(1..=2000))]
    stall_ms: u64,
    /// Minimum seconds between two memory events.
    #[arg(long, default_value_t = 60)]
    cooldown_secs: u64,
    /// Seconds between two orphan scans when no session ends.
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
struct SetupArgs {
    /// The model of the coordinator's runs, without asking [default: the configured one, else claude-sonnet-5-5].
    #[arg(long)]
    model: Option<String>,
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
            state_dir: state_dir(args.state)?,
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
            print!("{}", report::peaks(&state_dir(dir)?, None, None)?);
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
        },
        Command::Setup(args) => setup(args.model),
        Command::Coordinator(args) => match args.command {
            None => open_coordinator(),
            Some(CoordinatorCommand::Next) => next_events(),
            Some(CoordinatorCommand::Note { text }) => {
                let paths = coordinator::Paths::from_env()?;
                journal::note(&paths.journal(), &text.join(" "), now_secs())
            }
        },
        Command::Launch(l) => {
            let e = launch::launch(&l.command);
            eprintln!("orchestrator: {e:#}; the session runs unorchestrated");
            Err(launch::run_unchanged(&l.command))
        }
        Command::Enter(l) => Err(launch::enter(&l.command)),
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

fn state_dir(arg: StateDir) -> Result<PathBuf> {
    match arg.state_dir {
        Some(dir) => Ok(dir),
        None => state::default_dir(),
    }
}

fn print_config() -> Result<()> {
    let path = config::default_path()?;
    match config::load(&path)? {
        Some(config) => println!(
            "{}\n{}",
            path.display(),
            serde_json::to_string_pretty(&config).context("serializing the configuration")?
        ),
        None => println!("No configuration at {}.", path.display()),
    }
    let priorities = config::priorities_path(&path);
    if priorities.exists() {
        println!("The coordinator's priorities: {}", priorities.display());
    }
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

/// The model the user chooses for the coordinator: `flag` when given, else
/// asked on a terminal with `current` as the default, else `current`.
fn choose_model(flag: Option<String>, current: &str) -> Result<String> {
    if let Some(model) = flag {
        return Ok(model);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(current.to_string());
    }
    print!("Model of the coordinator's runs [{current}]: ");
    std::io::stdout().flush().context("asking for the model")?;
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .context("reading the model")?;
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        current.to_string()
    } else {
        answer.to_string()
    })
}

/// Enables the coordinator with the model the user chooses, then runs one
/// that examines the machine and writes the admission thresholds, and
/// reports what it took. The user running it is the consent.
fn setup(model: Option<String>) -> Result<()> {
    let path = config::default_path()?;
    let mut config = config::load(&path)?.unwrap_or_default();
    let enabled = config.coordinator.is_some();
    let mut cfg = config.coordinator.clone().unwrap_or_default();
    cfg.model = choose_model(model, &cfg.model)?;
    if config.coordinator.as_ref() != Some(&cfg) {
        config.coordinator = Some(cfg.clone());
        config::save(&path, &config)?;
        if enabled {
            println!("The coordinator's runs now use {}.", cfg.model);
        } else {
            println!(
                "Enabled the coordinator in {}: model {}, each run stopped past {} min, woken when admission holds a call {} s. Remove the `coordinator` section to turn it off.",
                path.display(),
                cfg.model,
                cfg.max_minutes,
                cfg.wait_secs
            );
        }
    }
    println!("Starting a coordinator to examine the machine; it ends with a summary.");
    let places = Places::from_env()?;
    let paths = briefing::paths(&places);
    let proc_root = Path::new(PROC);
    let me = Holder::of(proc_root, std::process::id()).context("reading this process's start")?;
    coordinator::acquire(&paths, proc_root, me)?;
    let ran = run_setup(&places, &paths, &cfg, config.admission.is_some());
    if let Ok(locked) = paths.lock() {
        let _ = locked.clear_holder(me);
    }
    let record = ran?;
    if let Some(reply) = &record.reply {
        println!("{reply}");
    }
    println!("\n{}", record.summary());
    if record.handled() {
        Ok(())
    } else {
        bail!(
            "the coordinator did not finish; see {}",
            paths.last_errors().display()
        )
    }
}

/// Runs the setup coordinator, within the configured time, and records it.
fn run_setup(
    places: &Places,
    paths: &coordinator::Paths,
    cfg: &Coordinator,
    reconfigure: bool,
) -> Result<run::RunRecord> {
    run::write_role(paths)?;
    let at = now_secs();
    let mode = run::Mode::Setup { reconfigure };
    let prompt = run::prompt(&mode, paths, at, &briefing::briefing(places, paths));
    let out = std::fs::File::create(paths.last_run()).context("creating the run's output")?;
    let err = std::fs::File::create(paths.last_errors()).context("creating the run's errors")?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(cfg.max_minutes.saturating_mul(60));
    let mut child = run::command(&mode, cfg, paths, &bin_dir()?, &prompt)
        .stdout(out)
        .stderr(err)
        .spawn()
        .context("starting claude")?;
    let mut stopped = false;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for claude")? {
            break status;
        }
        if Instant::now() >= deadline && !stopped {
            eprintln!("orchestrator: the coordinator ran past its time limit; stopping it");
            let _ = kill_process(Pid::from_child(&child), Signal::TERM);
            stopped = true;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let outcome = run::Outcome {
        secs: started.elapsed().as_secs(),
        exit: status.code(),
        stopped,
    };
    let output = std::fs::read(paths.last_run()).unwrap_or_default();
    let record = run::RunRecord::new("setup", at, 0, outcome, &output);
    orchestrator::events::append_line(&paths.runs(), &record)?;
    Ok(record)
}

/// Opens an interactive coordinator, once the user has enabled it: takes
/// the coordinator, waiting for a running one to end, then replaces this
/// process with `claude`. The holder's pid and start time stay this
/// process's, so the coordinator frees itself when claude exits.
fn open_coordinator() -> Result<()> {
    let path = config::default_path()?;
    let Some(cfg) = config::load(&path)?.and_then(|c| c.coordinator) else {
        bail!(
            "the coordinator is off: run `orchestrator setup`, or add a `coordinator` section to {}",
            path.display()
        );
    };
    let places = Places::from_env()?;
    let paths = briefing::paths(&places);
    let proc_root = Path::new(PROC);
    let me = Holder::of(proc_root, std::process::id()).context("reading this process's start")?;
    coordinator::acquire(&paths, proc_root, me)?;
    run::write_role(&paths)?;
    let mode = run::Mode::Interactive;
    let prompt = run::prompt(
        &mode,
        &paths,
        now_secs(),
        &briefing::briefing(&places, &paths),
    );
    let err = run::command(&mode, &cfg, &paths, &bin_dir()?, &prompt).exec();
    Err(anyhow!(err).context("running claude"))
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

fn now_secs() -> u64 {
    u64::try_from(runtime::now_ms() / 1000).unwrap_or(u64::MAX)
}
