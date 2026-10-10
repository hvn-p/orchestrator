//! The shell prefix. Claude Code runs every Bash call, hook, status line
//! refresh and stdio MCP server start as `orchestrator-prefix '<command>'`.
//! Inside an orchestrated session, the prefix moves itself into a job leaf of
//! its own; a Bash call then goes through admission, which may hold a heavy
//! call back until memory covers it. The prefix then replaces itself with a
//! shell running the command, so output, exit code and signals stay the
//! command's. Any failure leaves the command running at once, unchanged.
//!
//! The prefix is where a command can wait: inside the call, already in its
//! job group. A `PreToolUse` hook could not: it cannot wait past its own
//! timeout, after which the call proceeds, and it runs before the call has
//! a job group. Nor could interception at system level: `LD_PRELOAD` is
//! fragile and misses static binaries, seccomp breaks `sudo` and stalls the
//! session if its supervisor dies, ptrace breaks strace and gdb, and
//! `eBPF`, fanotify and audit need root.

pub mod admission;

use anyhow::{Context, Result};
use claude_code::call;
use claude_code::invocation::{self, Kind};
use learning::peaks;
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use system::cgroup;
use system::runtime;
use system::state;

/// What a Bash call ran, kept for the service until it has measured the job.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    /// The whole invocation Claude Code assembled, environment setup included.
    pub command: String,
    pub cwd: String,
}

/// A job's kind, in its group's name.
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Bash => "bash",
        Kind::Other => "other",
    }
}

/// Places this process, admits a Bash call, then replaces this process with
/// a shell running `command`. Only returns when the shell cannot be started.
pub fn run(command: &OsStr) -> anyhow::Error {
    let root = Path::new(cgroup::ROOT);
    match place(root, command) {
        Ok(Some(job)) => {
            if let Err(e) = admit(root, &job, command) {
                log_failure(&e);
            }
        }
        Ok(None) => {}
        Err(e) => log_failure(&e),
    }
    let err = invocation::shell(command).exec();
    anyhow::Error::new(err).context("running bash")
}

/// Moves this process into a job leaf of its own. Returns the leaf of a Bash
/// call, which admission may hold back.
fn place(root: &Path, command: &OsStr) -> Result<Option<admission::Job>> {
    let own = fs::read_to_string("/proc/self/cgroup").context("reading own cgroup")?;
    let Some(session) = cgroup::own_path(&own).and_then(cgroup::session_of) else {
        return Ok(None);
    };
    let text = command.to_string_lossy();
    let kind = invocation::kind(&text);
    let pid = std::process::id();
    let name = job_name(kind, pid, runtime::now_ms());
    cgroup::enter_job(root, session, &name, pid).with_context(|| format!("creating {name}"))?;
    if kind != Kind::Bash {
        return Ok(None);
    }
    let record = JobRecord {
        command: text.into_owned(),
        cwd: invocation::working_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    write_record(
        &records_dir(&runtime::default_dir()?, session),
        &name,
        &record,
    )?;
    Ok(Some(admission::Job {
        group: format!("{session}/{name}"),
        name,
    }))
}

/// Holds a heavy Bash call back until memory covers it. Returns at once
/// without a configuration, and for a call none of whose commands is known
/// to be heavy: the configuration is read first, so that without one nothing
/// is parsed.
fn admit(root: &Path, job: &admission::Job, command: &OsStr) -> Result<()> {
    let Some(cfg) = config::load(&config::default_path()?)?.and_then(|c| c.admission) else {
        return Ok(());
    };
    let cwd = invocation::working_dir().context("reading the working directory")?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let peaks = peaks::dir(&state::default_dir()?);
    let invocation = command.to_string_lossy();
    let known = admission::known(&invocation, &cwd, home.as_deref(), &peaks);
    let Some(call) = admission::heavy(&known, &cfg) else {
        return Ok(());
    };
    let paths = admission::Paths {
        cgroup_root: root.to_path_buf(),
        meminfo: PathBuf::from("/proc/meminfo"),
        runtime: runtime::default_dir()?,
    };
    admission::admit(&paths, &cfg, job, &call, &mut call::notices())
}

/// The pid names the job for a human; the time keeps the name unique when a
/// pid comes back before the service has removed the old group.
pub fn job_name(kind: Kind, pid: u32, ms: u128) -> String {
    format!("job-{}-{pid}-{ms}", kind_name(kind))
}

/// Kind and start time of a job, from a name made by `job_name`.
pub fn parse_job_name(name: &str) -> Option<(Kind, u128)> {
    let (rest, ms) = name.strip_prefix("job-")?.rsplit_once('-')?;
    let (kind, pid) = rest.rsplit_once('-')?;
    pid.parse::<u32>().ok()?;
    let kind = match kind {
        "bash" => Kind::Bash,
        "other" => Kind::Other,
        _ => return None,
    };
    Some((kind, ms.parse().ok()?))
}

/// Where job records live: `<runtime>/jobs/`, one directory per session scope.
pub fn records_root(runtime: &Path) -> PathBuf {
    runtime.join("jobs")
}

/// Records of one session's jobs: `<runtime>/jobs/<session scope>/`.
pub fn records_dir(runtime: &Path, session: &str) -> PathBuf {
    let scope = session.rsplit('/').next().unwrap_or(session);
    records_root(runtime).join(scope)
}

fn write_record(dir: &Path, job: &str, record: &JobRecord) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{job}.json"));
    let json = serde_json::to_vec(record).context("serializing the job record")?;
    fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

/// Runs what Claude Code passed when it is not the one command line the
/// prefix expects: as a command, unplaced and unadmitted, and logged. A
/// change on Claude Code's side then costs orchestration, never the command.
/// Returns None when there is nothing to run, else only when the command
/// cannot be started.
pub fn run_unexpected(args: &[OsString]) -> Option<anyhow::Error> {
    log_failure(&anyhow::anyhow!(
        "expected one argument, got {}: running them unorchestrated",
        args.len()
    ));
    let (program, rest) = args.split_first()?;
    let err = Command::new(program).args(rest).exec();
    Some(anyhow::Error::new(err).context("running the command"))
}

/// Nothing goes to the terminal: the prefix's stderr is the command's, and a
/// hook or the status line must not show orchestrator's troubles. Only
/// admission's notices go there.
fn log_failure(e: &anyhow::Error) {
    let Ok(dir) = runtime::default_dir() else {
        return;
    };
    if let Ok(mut log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("prefix.log"))
    {
        let _ = writeln!(log, "{} {e:#}", runtime::now_ms());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Bash call as Claude Code 2.1.289 hands it to the prefix.
    const BASH_CALL: &str = "source /home/u/.claude/shell-snapshots/snapshot-bash-1-x.sh 2>/dev/null || true && { shopt -u extglob || setopt NO_EXTENDED_GLOB NO_BARE_GLOB; } >/dev/null 2>&1 || true && eval 'echo hi' && pwd -P >| /tmp/claude-ab12-cwd";

    #[test]
    fn job_names_carry_kind_pid_and_time() {
        assert_eq!(job_name(Kind::Bash, 7, 1234), "job-bash-7-1234");
        assert_eq!(job_name(Kind::Other, 8, 5), "job-other-8-5");
    }

    #[test]
    fn job_names_parse_back() {
        assert_eq!(parse_job_name("job-bash-7-1234"), Some((Kind::Bash, 1234)));
        assert_eq!(parse_job_name("job-other-8-5"), Some((Kind::Other, 5)));
        assert_eq!(parse_job_name("job-bash-7"), None);
        assert_eq!(parse_job_name("job-cron-7-5"), None);
        assert_eq!(parse_job_name("main"), None);
    }

    #[test]
    fn records_land_under_the_session_scope() {
        let runtime = tempfile::tempdir().unwrap();
        let session = "/u/orchestrator.slice/orchestrator-9-9.scope";
        let dir = records_dir(runtime.path(), session);
        assert_eq!(dir, runtime.path().join("jobs/orchestrator-9-9.scope"));
        let record = JobRecord {
            command: BASH_CALL.into(),
            cwd: "/repo".into(),
        };
        write_record(&dir, "job-bash-7-1", &record).unwrap();
        let back: JobRecord =
            serde_json::from_slice(&fs::read(dir.join("job-bash-7-1.json")).unwrap()).unwrap();
        assert_eq!(back, record);
    }
}
