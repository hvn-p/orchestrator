//! Starting a session. `orchestrator launch` asks systemd for a delegated user
//! scope in the orchestrator slice and runs `orchestrator enter` inside it.
//! `enter` arranges the scope's cgroups, points Claude Code at the shell
//! prefix, then replaces itself with the session's command.

use crate::cgroup;
use anyhow::{Context, Result, anyhow, ensure};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The shell prefix binary, installed next to `orchestrator`.
// claude-code: shell-prefix-variable
pub const PREFIX_BIN: &str = "orchestrator-prefix";
// claude-code: shell-prefix-variable
const PREFIX_VAR: &str = "CLAUDE_CODE_SHELL_PREFIX";

/// Replaces this process with `systemd-run`, which runs
/// `orchestrator enter -- <command>` in a new scope. Only returns when
/// `systemd-run` cannot be started.
pub fn launch(command: &[OsString]) -> anyhow::Error {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return anyhow::Error::new(e).context("locating orchestrator"),
    };
    let err = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect"])
        .arg(format!("--slice={}", cgroup::SLICE))
        .args(["-p", "Delegate=yes", "--"])
        .arg(exe)
        .args(["enter", "--"])
        .args(command)
        .exec();
    anyhow::Error::new(err).context("starting systemd-run")
}

/// Runs inside the new scope, as its only process: sets the scope up, then
/// replaces itself with `command`. A failed setup leaves the session
/// unorchestrated, it never prevents it. Only returns when `command` cannot be
/// started.
pub fn enter(command: &[OsString]) -> anyhow::Error {
    let mut cmd = match to_command(command) {
        Ok(cmd) => cmd,
        Err(e) => return e,
    };
    match set_up(Path::new(cgroup::ROOT)) {
        Ok(prefix) => {
            if let Some(theirs) = std::env::var_os(PREFIX_VAR).filter(|v| *v != prefix) {
                eprintln!(
                    "orchestrator: replacing {PREFIX_VAR}={}",
                    theirs.to_string_lossy()
                );
            }
            cmd.env(PREFIX_VAR, prefix);
        }
        Err(e) => eprintln!("orchestrator: {e:#}; the session runs unorchestrated"),
    }
    exec(cmd)
}

/// Replaces this process with `command`, outside any orchestration. Only
/// returns when `command` cannot be started.
pub fn run_unchanged(command: &[OsString]) -> anyhow::Error {
    match to_command(command) {
        Ok(cmd) => exec(cmd),
        Err(e) => e,
    }
}

/// Returns the shell prefix to hand to Claude Code.
fn set_up(root: &Path) -> Result<PathBuf> {
    let own = std::fs::read_to_string("/proc/self/cgroup").context("reading own cgroup")?;
    let scope = cgroup::own_path(&own).context("no cgroup v2 line in /proc/self/cgroup")?;
    // Started by hand elsewhere, it would rearrange someone else's cgroup.
    ensure!(
        cgroup::session_of(scope) == Some(scope),
        "not the scope of a new session: {scope}"
    );
    cgroup::set_up_session(root, scope, std::process::id())
        .with_context(|| format!("setting up {scope}"))?;
    let prefix = std::env::current_exe()
        .context("locating orchestrator")?
        .with_file_name(PREFIX_BIN);
    ensure!(prefix.is_file(), "{} not found", prefix.display());
    Ok(prefix)
}

fn to_command(command: &[OsString]) -> Result<Command> {
    let (program, args) = command.split_first().context("no command given")?;
    let mut cmd = Command::new(program);
    cmd.args(args);
    Ok(cmd)
}

fn exec(mut cmd: Command) -> anyhow::Error {
    let program = cmd.get_program().to_string_lossy().into_owned();
    anyhow!(cmd.exec()).context(format!("running {program}"))
}
