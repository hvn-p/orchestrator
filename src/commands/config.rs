//! `orchestrator config`: print the configuration, or set a section of it
//! through a validated write.

use super::PROC;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use config::Admission;
use std::path::Path;
use system::{cgroup, machine};
use watch::report;

#[derive(Args)]
pub struct ConfigArgs {
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
    /// Whether `watch` may start a coordinator by itself for events: yes or no (also y/n, true/false, t/f, on/off, 1/0).
    #[arg(
        long,
        value_name = "yes|no",
        hide_possible_values = true,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
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

pub fn run(args: ConfigArgs) -> Result<()> {
    match args.command {
        None => print_config(),
        Some(ConfigCommand::Admission(a)) => set_admission(&a),
        Some(ConfigCommand::Coordinator(c)) => set_coordinator(c),
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
