//! orchestrator: schedules the work of the Claude Code sessions running in
//! parallel on one machine, so development keeps going. See docs/design.md.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use orchestrator::config::{self, Admission, Coordinator};
use orchestrator::coordinator::{self, Holder, run};
use orchestrator::{
    admission, cgroup, launch, machine, memory, peaks, procfs, runtime, sessions, state, watch,
};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Characters of a command line shown by `orchestrator sessions`.
const DISPLAY_CMDLINE: usize = 70;
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
    Setup,
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
struct CoordinatorArgs {
    #[command(subcommand)]
    command: Option<CoordinatorCommand>,
}

#[derive(Subcommand)]
enum CoordinatorCommand {
    /// Wait until events are pending for the coordinator, then print them and take them off the queue.
    Next,
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
        Command::Sessions(args) => print_sessions(&args.sources, args.heads),
        Command::Peaks(dir) => print_peaks(&state_dir(dir)?),
        Command::Admission => print_admission(),
        Command::Machine => {
            let m = machine::read(Path::new(PROC), Path::new(cgroup::ROOT))?;
            print!("{}", machine::describe(&m));
            Ok(())
        }
        Command::Config(args) => match args.command {
            None => print_config(),
            Some(ConfigCommand::Admission(a)) => set_admission(&a),
        },
        Command::Setup => setup(),
        Command::Coordinator(args) => match args.command {
            None => open_coordinator(),
            Some(CoordinatorCommand::Next) => next_events(),
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

/// Heaviest first. A command shows as its label; its id's start tells apart
/// two commands with the same label.
fn print_peaks(state: &Path) -> Result<()> {
    let learned = peaks::list(&peaks::dir(state))?;
    if learned.is_empty() {
        println!("No peak learned yet.");
    }
    for repo in learned {
        let mut commands = repo.commands;
        commands.sort_by(|a, b| b.peak_mb.cmp(&a.peak_mb).then_with(|| a.id.cmp(&b.id)));
        let width = commands
            .iter()
            .map(|c| c.label.chars().count().min(peaks::LABEL_MAX))
            .fold("COMMAND".len(), usize::max);
        println!("{}", repo.repository);
        println!(
            "  {:>7}  {:<8}  {:<width$}  LATEST CALLS, MB (* ALONE)",
            "PEAK MB", "ID", "COMMAND",
        );
        for c in commands {
            let calls: Vec<String> = c
                .recent
                .iter()
                .rev()
                .map(|peaks::Call(mb, alone)| format!("{mb}{}", if *alone { "*" } else { "" }))
                .collect();
            println!(
                "  {:>7}  {:<8}  {:<width$}  {}",
                c.peak_mb,
                c.id.get(..8).unwrap_or(&c.id),
                procfs::truncate(&c.label, peaks::LABEL_MAX),
                calls.join(" "),
            );
        }
        println!();
    }
    Ok(())
}

/// With `heads`, processes show as events name them: no argument that could
/// hold a credential, for a reader that hands the output to a model. A
/// session shows by its name, which messages address.
// claude-code: cross-session-message
fn print_sessions(sources: &Sources, heads: bool) -> Result<()> {
    let available = memory::available_mb(&sources.proc_root.join("meminfo"))?;
    let att = watch::scan(&sources.proc_root, &sessions_dir(sources)?)?;
    let shown = |p: &orchestrator::attribution::ProcRef| {
        if heads {
            p.command_head.clone()
        } else {
            procfs::truncate(&p.cmdline, DISPLAY_CMDLINE)
        }
    };
    println!("Available memory: {available} MB\n");
    println!("{:<34} {:>7}  LARGEST PROCESS", "SESSION", "RSS MB");
    for s in &att.sessions {
        println!(
            "{:<34} {:>7}  {} MB  pid {}  {}",
            s.name,
            s.rss_kb / 1024,
            s.largest.rss_kb / 1024,
            s.largest.pid,
            shown(&s.largest),
        );
    }
    if att.orphans.is_empty() {
        println!("\nNo orphaned process.");
    } else {
        println!("\nORPHANS (session gone)");
        for o in &att.orphans {
            println!(
                "{:>7} MB  {} process(es)  pid {}  session {}  {}",
                o.rss_kb / 1024,
                o.processes,
                o.root.pid,
                o.session_id.chars().take(8).collect::<String>(),
                shown(&o.root),
            );
        }
    }
    Ok(())
}

fn admission_paths() -> Result<admission::Paths> {
    Ok(admission::Paths {
        cgroup_root: PathBuf::from(cgroup::ROOT),
        meminfo: Path::new(PROC).join("meminfo"),
        runtime: runtime::default_dir()?,
    })
}

/// The session a job group belongs to, by name, which messages address,
/// and the start of its id, else by its scope.
// claude-code: cross-session-message
fn session_label(group: &str, cgroup_root: &Path, known: &[sessions::ClaudeSession]) -> String {
    let scope = cgroup::session_of(group);
    let session = scope
        .and_then(|s| cgroup::main_pid(cgroup_root, s))
        .and_then(|pid| known.iter().find(|s| s.pid == pid));
    match (session, scope) {
        (Some(s), _) => format!(
            "{} ({})",
            s.name,
            s.session_id.chars().take(8).collect::<String>()
        ),
        (None, Some(scope)) => scope.rsplit('/').next().unwrap_or(scope).to_string(),
        (None, None) => group.to_string(),
    }
}

fn print_admission() -> Result<()> {
    let paths = admission_paths()?;
    let snap = admission::snapshot(&paths)?;
    let config_path = config::default_path()?;
    match config::load(&config_path) {
        Ok(Some(config::Config {
            admission: Some(a), ..
        })) => println!(
            "Admission: a call expected to peak at {} MB or more waits until free memory covers its peak plus {} MB, {} s at most.",
            a.heavy_mb, a.margin_mb, a.max_wait_secs
        ),
        Ok(_) => println!(
            "Admission: off, no admission section in {}.",
            config_path.display()
        ),
        Err(e) => println!("Admission: off, {e:#}."),
    }
    println!(
        "Available memory: {} MB; running heavy calls still hold {} MB of it: {} MB free for admission.\n",
        snap.available_mb,
        snap.held_mb,
        snap.free_mb()
    );
    let known = sessions::read_sessions(&default_sessions_dir()?).unwrap_or_default();
    let label = |group: &str| session_label(group, &paths.cgroup_root, &known);
    let now = u64::try_from(runtime::now_ms()).unwrap_or(u64::MAX);
    if snap.waiting.is_empty() {
        println!("No call waits for memory.");
    } else {
        println!("WAITING FOR MEMORY");
        println!(
            "  {:<40} {:>7} {:>8} {:>9}  COMMAND",
            "SESSION", "WAITED", "PEAK MB", "NEEDS MB"
        );
        for w in &snap.waiting {
            println!(
                "  {:<40} {:>5} s {:>8} {:>9}  {}",
                label(&w.group),
                now.saturating_sub(w.since_ms) / 1000,
                w.peak_mb,
                w.need_mb,
                w.label
            );
        }
    }
    println!();
    if snap.reserved.is_empty() {
        println!("No heavy call runs with a reservation.");
    } else {
        println!("RESERVED BY RUNNING HEAVY CALLS");
        println!(
            "  {:<40} {:>8} {:>8} {:>8}  COMMAND",
            "SESSION", "HOLDS MB", "PEAK MB", "USES MB"
        );
        for r in &snap.reserved {
            println!(
                "  {:<40} {:>8} {:>8} {:>8}  {}",
                label(&r.group),
                r.held_mb,
                r.peak_mb,
                r.current_mb.map_or("?".to_string(), |mb| mb.to_string()),
                if r.label.is_empty() { "?" } else { &r.label }
            );
        }
    }
    Ok(())
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

/// Enables the coordinator if it is not, then replaces this process with a
/// coordinator run that writes the admission thresholds. The user running it
/// is the consent.
fn setup() -> Result<()> {
    let path = config::default_path()?;
    let mut config = config::load(&path)?.unwrap_or_default();
    let cfg = if let Some(cfg) = &config.coordinator {
        cfg.clone()
    } else {
        let cfg = Coordinator::default();
        config.coordinator = Some(cfg.clone());
        config::save(&path, &config)?;
        println!(
            "Enabled the coordinator in {}: model {}, at most {} USD and {} min per run, woken when admission holds a call {} s. Remove the `coordinator` section to turn it off.",
            path.display(),
            cfg.model,
            cfg.max_budget_usd,
            cfg.max_minutes,
            cfg.wait_secs
        );
        cfg
    };
    println!("Starting a coordinator to examine the machine; it ends with a summary.");
    let mode = run::Mode::Setup {
        reconfigure: config.admission.is_some(),
    };
    Err(become_coordinator(&mode, &cfg))
}

/// Opens an interactive coordinator, once the user has enabled it.
fn open_coordinator() -> Result<()> {
    let path = config::default_path()?;
    let Some(cfg) = config::load(&path)?.and_then(|c| c.coordinator) else {
        bail!(
            "the coordinator is off: run `orchestrator setup`, or add a `coordinator` section to {}",
            path.display()
        );
    };
    Err(become_coordinator(&run::Mode::Interactive, &cfg))
}

/// Takes the coordinator, waiting for a running one to end, then replaces
/// this process with `claude`: the holder's pid and start time stay this
/// process's, so the coordinator frees itself when claude exits. Only
/// returns on an error.
fn become_coordinator(mode: &run::Mode<'_>, cfg: &Coordinator) -> anyhow::Error {
    let started = || -> Result<std::process::Command> {
        let paths = coordinator::Paths::from_env()?;
        let proc_root = Path::new(PROC);
        let me =
            Holder::of(proc_root, std::process::id()).context("reading this process's start")?;
        coordinator::acquire(&paths, proc_root, me)?;
        run::write_role(&paths)?;
        let bin = std::env::current_exe()
            .context("locating orchestrator")?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let now = u64::try_from(runtime::now_ms() / 1000).unwrap_or(u64::MAX);
        Ok(run::command(mode, cfg, &paths, &bin, now))
    };
    match started() {
        Ok(mut cmd) => anyhow!(cmd.exec()).context("running claude"),
        Err(e) => e,
    }
}

fn next_events() -> Result<()> {
    let paths = coordinator::Paths::from_env()?;
    for pending in coordinator::next(&paths, &admission_paths()?)? {
        println!("{}", pending.line());
    }
    Ok(())
}
