//! orchestrator: schedules the work of the Claude Code sessions running in
//! parallel on one machine, so development keeps going. See docs/design.md.

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use orchestrator::{launch, memory, procfs, runtime, sessions, watch};
use std::ffi::OsString;
use std::path::PathBuf;
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
    /// Watch memory pressure, session ends and job ends, and append the events that drive scheduling.
    Watch(WatchArgs),
    /// Print memory per Claude session and the orphaned processes.
    Sessions(Sources),
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
    #[arg(long, default_value = "/proc")]
    proc_root: PathBuf,
    /// Claude Code's sessions directory [default: ~/.claude/sessions].
    #[arg(long)]
    sessions_dir: Option<PathBuf>,
}

#[derive(Args)]
struct WatchArgs {
    #[command(flatten)]
    sources: Sources,
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

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Watch(args) => watch::run(&watch::Config {
            sessions_dir: sessions_dir(&args.sources)?,
            proc_root: args.sources.proc_root,
            runtime_dir: match args.runtime_dir {
                Some(dir) => dir,
                None => runtime::default_dir()?,
            },
            orphan_interval: Duration::from_secs(args.orphan_interval_secs.max(1)),
            thresholds: watch::Thresholds {
                stall_ms: args.stall_ms,
                cooldown_secs: args.cooldown_secs,
            },
        }),
        Command::Sessions(sources) => print_sessions(&sources),
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
        None => sessions::default_dir().context("neither CLAUDE_CONFIG_DIR nor HOME is set"),
    }
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
