//! orchestrator: schedules the work of the Claude Code sessions running in
//! parallel on one machine, so development keeps going. See docs/design.md.

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use orchestrator::{launch, memory, peaks, procfs, runtime, sessions, state, watch};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Characters of a command line shown by `orchestrator sessions`.
const DISPLAY_CMDLINE: usize = 70;

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
    /// Watch memory pressure, session ends and job ends, append events, and learn the memory peak of each Bash call.
    Watch(WatchArgs),
    /// Print memory per Claude session and the orphaned processes.
    Sessions(Sources),
    /// Print the memory peaks learned per repository and command.
    Peaks(StateDir),
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
    #[arg(long, default_value = "/proc")]
    proc_root: PathBuf,
    /// Claude Code's sessions directory [default: `$CLAUDE_CONFIG_DIR/sessions`, else `~/.claude/sessions`].
    #[arg(long)]
    sessions_dir: Option<PathBuf>,
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
    #[command(flatten)]
    state: StateDir,
    /// Where events and measurements are written and job records read [default: `$XDG_RUNTIME_DIR/orchestrator`, else `/run/user/<uid>/orchestrator`].
    #[arg(long)]
    runtime_dir: Option<PathBuf>,
    /// Memory stall, within a 2 s window, that makes a pressure event, in ms.
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u64).range(1..=2000))]
    stall_ms: u64,
    /// Minimum seconds between two memory events.
    #[arg(long, default_value_t = 60)]
    cooldown_secs: u64,
    /// Seconds between two orphan scans when no session ends; 0 counts as 1.
    #[arg(long, default_value_t = 300)]
    orphan_interval_secs: u64,
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
            orphan_interval: Duration::from_secs(args.orphan_interval_secs.max(1)),
            thresholds: watch::Thresholds {
                stall_ms: args.stall_ms,
                cooldown_secs: args.cooldown_secs,
            },
        }),
        Command::Sessions(sources) => print_sessions(&sources),
        Command::Peaks(dir) => print_peaks(&state_dir(dir)?),
        Command::Launch(l) => Err(launch::launch(&l.command)),
    }
}

fn sessions_dir(sources: &Sources) -> Result<PathBuf> {
    match &sources.sessions_dir {
        Some(dir) => Ok(dir.clone()),
        None => sessions::default_dir().context("neither CLAUDE_CONFIG_DIR nor HOME is set"),
    }
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

fn print_sessions(sources: &Sources) -> Result<()> {
    let available = memory::available_mb(&sources.proc_root.join("meminfo"))?;
    let att = watch::scan(&sources.proc_root, &sessions_dir(sources)?)?;
    println!("Available memory: {available} MB\n");
    println!("{:<34} {:>7}  LARGEST PROCESS", "SESSION", "RSS MB");
    for s in &att.sessions {
        println!(
            "{:<34} {:>7}  {} MB  pid {}  {}",
            s.name,
            s.rss_kb / 1024,
            s.largest.rss_kb / 1024,
            s.largest.pid,
            procfs::truncate(&s.largest.cmdline, DISPLAY_CMDLINE),
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
                procfs::truncate(&o.root.cmdline, DISPLAY_CMDLINE),
            );
        }
    }
    Ok(())
}
