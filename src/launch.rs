//! Starting a session. `orchestrator launch` asks the systemd user manager,
//! over D-Bus, for a delegated scope in the orchestrator slice holding its own
//! process, and waits until it is in it. It then arranges the scope's cgroups,
//! points Claude Code at the shell prefix, and replaces itself with the
//! session's command. Any failure on the way leaves the session
//! unorchestrated, never prevents it: the command runs unchanged, after one
//! warning.

use anyhow::{Context, Result, anyhow, bail, ensure};
use claude_code::launch::PREFIX_VAR;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use system::cgroup;

/// The shell prefix binary, installed next to `orchestrator`.
pub const PREFIX_BIN: &str = "orchestrator-prefix";

/// The longest the scope may take, from asking for it to being in it. A
/// launcher has to replace itself with its command within about 3 s.
const SCOPE_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the request and the move are checked while waiting for them.
const POLL: Duration = Duration::from_millis(1);

/// Moves this process into a new session scope, then replaces it with
/// `command`, unchanged when the scope could not be had. Only returns when
/// `command` cannot be started.
pub fn launch(command: &[OsString]) -> anyhow::Error {
    let mut cmd = match to_command(command) {
        Ok(cmd) => cmd,
        Err(e) => return e,
    };
    match orchestrate(Path::new(cgroup::ROOT)) {
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

/// Puts this process in the `main/` leaf of a new session scope, and returns
/// the shell prefix to hand to Claude Code.
fn orchestrate(root: &Path) -> Result<PathBuf> {
    let prefix = std::env::current_exe()
        .context("locating orchestrator")?
        .with_file_name(PREFIX_BIN);
    ensure!(prefix.is_file(), "{} not found", prefix.display());
    let pid = std::process::id();
    let unit = unit_name(pid, SystemTime::now());
    let deadline = Instant::now() + SCOPE_TIMEOUT;
    let job = request_scope(&unit, pid, deadline)
        .with_context(|| format!("asking the systemd user manager for {unit}"))?;
    let scope = wait_until_in(&unit, deadline)
        .with_context(|| format!("waiting for {job} to move this process into {unit}"))?;
    cgroup::set_up_session(root, &scope, pid).with_context(|| format!("setting up {scope}"))?;
    Ok(prefix)
}

/// A name no other scope has: one process can launch twice, `exec` keeping
/// its pid, but not twice within a millisecond.
fn unit_name(pid: u32, now: SystemTime) -> String {
    let ms = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("orchestrator-{pid}-{ms}.scope")
}

/// Asks the systemd user manager to start `unit` holding `pid`, through
/// `busctl`, and returns the job it queued. The move into the scope comes
/// with that job, after the answer.
fn request_scope(unit: &str, pid: u32, deadline: Instant) -> Result<String> {
    let mut busctl = Command::new("busctl");
    busctl.args(start_args(unit, pid)).stdin(Stdio::null());
    job_of(&run_until(busctl, deadline)?)
}

/// The arguments of a `busctl` call to `StartTransientUnit`, in its own
/// notation: an array is its length then its items, a variant its signature
/// then its value. The scope is collected once its last process ends, even
/// when it failed.
fn start_args(unit: &str, pid: u32) -> Vec<String> {
    let pid = pid.to_string();
    [
        "--user",
        "call",
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
        "ssa(sv)a(sa(sv))",
        unit,
        "fail",
        "4",
        "PIDs",
        "au",
        "1",
        &pid,
        "Delegate",
        "b",
        "true",
        "Slice",
        "s",
        cgroup::SLICE,
        "CollectMode",
        "s",
        "inactive-or-failed",
        "0",
    ]
    .map(String::from)
    .to_vec()
}

/// Runs `cmd` with its output captured, or kills it at `deadline`.
fn run_until(mut cmd: Command, deadline: Instant) -> Result<Output> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {program}"))?;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} got no answer within {SCOPE_TIMEOUT:?}");
        }
        std::thread::sleep(POLL);
    }
}

/// The job `busctl call` printed, `o "<path>"`, or why it printed none.
fn job_of(output: &Output) -> Result<String> {
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        bail!("busctl failed ({}): {}", output.status, why.trim());
    }
    let reply = String::from_utf8_lossy(&output.stdout);
    let reply = reply.trim();
    reply
        .strip_prefix("o \"")
        .and_then(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
        .with_context(|| format!("unexpected reply from busctl: {reply:?}"))
}

/// Waits until this process is in the scope `unit`, and returns its path.
fn wait_until_in(unit: &str, deadline: Instant) -> Result<String> {
    loop {
        let own = std::fs::read_to_string("/proc/self/cgroup").context("reading own cgroup")?;
        if let Some(scope) = scope_named(&own, unit) {
            return Ok(scope.to_owned());
        }
        ensure!(
            Instant::now() < deadline,
            "still elsewhere after {SCOPE_TIMEOUT:?}"
        );
        std::thread::sleep(POLL);
    }
}

/// The cgroup path of a `/proc/<pid>/cgroup` file, when it is the session
/// scope `unit` itself.
fn scope_named<'a>(proc_cgroup: &'a str, unit: &str) -> Option<&'a str> {
    let path = cgroup::own_path(proc_cgroup)?;
    let is_session = cgroup::session_of(path) == Some(path);
    (is_session && path.rsplit('/').next() == Some(unit)).then_some(path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    const UNIT: &str = "orchestrator-42-1791234567890.scope";
    const SLICE: &str = "/user.slice/user-1000.slice/user@1000.service/orchestrator.slice";

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn in_secs(secs: u64) -> Instant {
        Instant::now() + Duration::from_secs(secs)
    }

    #[test]
    fn names_the_scope_after_the_pid_and_the_time() {
        let now = UNIX_EPOCH + Duration::from_millis(1_791_234_567_890);
        assert_eq!(unit_name(42, now), UNIT);
        let later = unit_name(42, now + Duration::from_millis(1));
        assert_ne!(later, UNIT);
    }

    #[test]
    fn asks_for_a_delegated_scope_holding_the_pid_in_the_slice() {
        let args = start_args(UNIT, 42);
        let expected = "--user call org.freedesktop.systemd1 /org/freedesktop/systemd1 \
             org.freedesktop.systemd1.Manager StartTransientUnit ssa(sv)a(sa(sv)) \
             orchestrator-42-1791234567890.scope fail 4 PIDs au 1 42 Delegate b true \
             Slice s orchestrator.slice CollectMode s inactive-or-failed 0";
        assert_eq!(args.join(" "), expected);
    }

    #[test]
    fn reads_the_job_from_the_reply() {
        let reply = output(0, "o \"/org/freedesktop/systemd1/job/1234\"\n", "");
        assert_eq!(
            job_of(&reply).unwrap(),
            "/org/freedesktop/systemd1/job/1234"
        );
    }

    #[test]
    fn a_refusal_carries_busctl_s_reason() {
        let refused = output(1, "", "Call failed: Unknown assignment: Bogus\n");
        let err = format!("{:#}", job_of(&refused).unwrap_err());
        assert!(err.contains("Unknown assignment: Bogus"), "{err}");
        assert!(err.contains("exit status: 1"), "{err}");
    }

    #[test]
    fn an_unexpected_reply_is_a_failure() {
        assert!(job_of(&output(0, "", "")).is_err());
        assert!(job_of(&output(0, "s \"x\"\n", "")).is_err());
    }

    #[test]
    fn knows_when_it_is_in_its_scope() {
        let inside = format!("0::{SLICE}/{UNIT}\n");
        assert_eq!(
            scope_named(&inside, UNIT),
            Some(format!("{SLICE}/{UNIT}").as_str())
        );
        let before = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/term.scope\n";
        assert_eq!(scope_named(before, UNIT), None);
        // Nested: still in the main/ leaf of the session that launched it.
        let nested = format!("0::{SLICE}/orchestrator-7-1.scope/main\n");
        assert_eq!(scope_named(&nested, UNIT), None);
        let leaf = format!("0::{SLICE}/{UNIT}/main\n");
        assert_eq!(scope_named(&leaf, UNIT), None);
        let elsewhere = format!("0::/user.slice/{UNIT}\n");
        assert_eq!(scope_named(&elsewhere, UNIT), None);
    }

    #[test]
    fn returns_what_a_quick_command_printed() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let out = run_until(cmd, in_secs(10)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn kills_a_command_past_the_deadline() {
        let mut cmd = Command::new("sleep");
        cmd.arg("10");
        let start = Instant::now();
        let err = format!("{:#}", run_until(cmd, Instant::now()).unwrap_err());
        assert!(err.contains("no answer"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_a_failure() {
        let cmd = Command::new("/nonexistent/busctl");
        let err = format!("{:#}", run_until(cmd, in_secs(10)).unwrap_err());
        assert!(err.contains("running /nonexistent/busctl"), "{err}");
    }
}
