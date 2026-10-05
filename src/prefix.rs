//! The shell prefix. Claude Code runs every Bash call, hook, status line
//! refresh and stdio MCP server start as `orchestrator-prefix '<command>'`.
//! Inside an orchestrated session, the prefix moves itself into a job leaf of
//! its own, then replaces itself with a shell running the command, so output,
//! exit code and signals stay the command's. Any failure leaves the command
//! running unchanged.

use crate::{cgroup, runtime};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a Bash call ran, kept for the service until it has measured the job.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    /// The whole invocation Claude Code assembled, environment setup included.
    pub command: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A Bash tool call, or a `!` command typed in Claude Code.
    Bash,
    /// A hook, the status line or an MCP server.
    Other,
}

impl Kind {
    /// A Bash call sources the session's shell snapshot and records its
    /// working directory afterwards; hooks, the status line and MCP servers do
    /// neither.
    pub fn of(command: &str) -> Kind {
        if command.contains("/shell-snapshots/snapshot-") || command.contains("pwd -P >|") {
            Kind::Bash
        } else {
            Kind::Other
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Bash => "bash",
            Kind::Other => "other",
        }
    }
}

/// Places this process, then replaces it with `bash -c <command>`. Only
/// returns when bash cannot be started.
pub fn run(command: &OsStr) -> anyhow::Error {
    if let Err(e) = place(Path::new(cgroup::ROOT), command) {
        log_failure(&e);
    }
    let err = Command::new("bash").arg("-c").arg(command).exec();
    anyhow::Error::new(err).context("running bash")
}

fn place(root: &Path, command: &OsStr) -> Result<()> {
    let own = fs::read_to_string("/proc/self/cgroup").context("reading own cgroup")?;
    let Some(session) = cgroup::own_path(&own).and_then(cgroup::session_of) else {
        return Ok(());
    };
    let text = command.to_string_lossy();
    let kind = Kind::of(&text);
    let pid = std::process::id();
    let name = job_name(kind, pid, runtime::now_ms());
    cgroup::enter_job(root, session, &name, pid).with_context(|| format!("creating {name}"))?;
    if kind == Kind::Bash {
        let record = JobRecord {
            command: text.into_owned(),
            cwd: std::env::current_dir()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        write_record(
            &records_dir(&runtime::default_dir()?, session),
            &name,
            &record,
        )?;
    }
    Ok(())
}

/// The pid names the job for a human; the time keeps the name unique when a
/// pid comes back before the service has removed the old group.
pub fn job_name(kind: Kind, pid: u32, ms: u128) -> String {
    format!("job-{}-{pid}-{ms}", kind.name())
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

/// Nothing goes to the terminal: the prefix's stderr is the command's, and a
/// hook or the status line must not show orchestrator's troubles.
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
    fn tells_bash_calls_from_the_rest() {
        assert_eq!(Kind::of(BASH_CALL), Kind::Bash);
        assert_eq!(Kind::of("bash ~/.claude/hooks/gate.sh"), Kind::Other);
        assert_eq!(Kind::of("npx -y @scope/mcp-server"), Kind::Other);
    }

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
        let session = "/u/orchestrator.slice/run-p9-i9.scope";
        let dir = records_dir(runtime.path(), session);
        assert_eq!(dir, runtime.path().join("jobs/run-p9-i9.scope"));
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
