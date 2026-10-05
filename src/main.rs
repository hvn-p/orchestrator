//! orchestrator: schedules the work of the Claude Code sessions running in
//! parallel on one machine, so development keeps going. See docs/design.md.

mod attribution;
mod events;
mod memory;
mod procfs;
mod sessions;
mod watch;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
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
    /// Watch memory pressure and orphaned processes, and append the events that drive scheduling.
    Watch(WatchArgs),
    /// Print memory per Claude session and the orphaned processes.
    Sessions(Sources),
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
    /// Seconds between two memory checks.
    #[arg(long, default_value_t = 2)]
    interval_secs: u64,
    /// Seconds between two orphan scans.
    #[arg(long, default_value_t = 30)]
    orphan_interval_secs: u64,
    /// Available memory under which a memory event fires, in MB.
    #[arg(long, default_value_t = 3000)]
    mem_min_mb: u64,
    /// Minimum seconds between two memory events.
    #[arg(long, default_value_t = 60)]
    cooldown_secs: u64,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Watch(args) => watch::run(&watch::Config {
            sessions_dir: sessions_dir(&args.sources)?,
            proc_root: args.sources.proc_root,
            runtime_dir: match args.runtime_dir {
                Some(dir) => dir,
                None => default_runtime_dir()?,
            },
            interval: Duration::from_secs(args.interval_secs.max(1)),
            orphan_interval: Duration::from_secs(args.orphan_interval_secs.max(1)),
            thresholds: watch::Thresholds {
                mem_min_mb: args.mem_min_mb,
                cooldown_secs: args.cooldown_secs,
            },
        }),
        Command::Sessions(sources) => print_sessions(&sources),
    }
}

fn sessions_dir(sources: &Sources) -> Result<PathBuf> {
    match &sources.sessions_dir {
        Some(dir) => Ok(dir.clone()),
        None => sessions::default_dir().context("neither CLAUDE_CONFIG_DIR nor HOME is set"),
    }
}

/// `$XDG_RUNTIME_DIR/orchestrator`, else `/run/user/<uid>/orchestrator`: in memory, cleared
/// at reboot, never versioned.
fn default_runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir).join("orchestrator"));
    }
    let status = std::fs::read_to_string("/proc/self/status").context("reading own uid")?;
    let uid = status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|v| v.split_whitespace().next())
        .context("no Uid line in /proc/self/status")?;
    Ok(Path::new("/run/user").join(uid).join("orchestrator"))
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
