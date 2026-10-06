//! Starting a session. `orchestrator launch` asks systemd for a delegated user
//! scope in the orchestrator slice and runs `orchestrator enter` inside it.
//! `enter` arranges the scope's cgroups, points Claude Code at the shell
//! prefix, then replaces itself with the session's command.

use crate::cgroup;
use anyhow::{Context, Result, anyhow, ensure};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The shell prefix binary, installed next to `orchestrator`.
// claude-code: shell-prefix-variable
pub const PREFIX_BIN: &str = "orchestrator-prefix";
// claude-code: shell-prefix-variable
const PREFIX_VAR: &str = "CLAUDE_CODE_SHELL_PREFIX";

/// The systemd user manager's own socket, under `XDG_RUNTIME_DIR`: the one
/// `systemd-run --user` talks to when that variable is set.
const MANAGER_SOCKET: &str = "systemd/private";
/// How long the user manager has to answer. The launcher must replace itself
/// within about 3 s, `systemd-run` included.
const MANAGER_TIMEOUT: Duration = Duration::from_secs(1);

/// Replaces this process with `systemd-run`, which runs
/// `orchestrator enter -- <command>` in a new scope. Only returns when the
/// systemd user manager does not answer or `systemd-run` cannot be started:
/// past `exec`, a failure of `systemd-run` would end the session instead of
/// leaving it unorchestrated.
pub fn launch(command: &[OsString]) -> anyhow::Error {
    if let Err(e) = reach_user_manager(std::env::var_os("XDG_RUNTIME_DIR").as_deref()) {
        return e;
    }
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

/// Checks that the systemd user manager answers on the socket `systemd-run
/// --user` uses. Without `XDG_RUNTIME_DIR`, `systemd-run` would try the
/// session bus instead; the session then runs unorchestrated rather than risk
/// not starting.
fn reach_user_manager(runtime_dir: Option<&OsStr>) -> Result<()> {
    let dir = runtime_dir
        .filter(|d| !d.is_empty())
        .context("XDG_RUNTIME_DIR is not set")?;
    let socket = Path::new(dir).join(MANAGER_SOCKET);
    authenticate(&socket)
        .with_context(|| format!("reaching the systemd user manager at {}", socket.display()))
}

/// Opens a D-Bus authentication on `socket` and waits for it to succeed. A
/// bare connection would not do: the manager logs a failure for each one
/// closed before it accepted it.
fn authenticate(socket: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(MANAGER_TIMEOUT))?;
    stream.write_all(auth_request(rustix::process::getuid().as_raw()).as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream.take(256))
        .read_line(&mut answer)
        .context("waiting for an answer")?;
    ensure!(
        answer.starts_with("OK "),
        "unexpected answer {:?}",
        answer.trim_end()
    );
    Ok(())
}

/// A D-Bus client's opening: a nul byte, then the EXTERNAL mechanism with the
/// uid as hex-encoded decimal digits.
fn auth_request(uid: u32) -> String {
    let hex = uid.to_string().bytes().fold(String::new(), |mut hex, b| {
        // Writing to a String cannot fail.
        let _ = write!(hex, "{b:02x}");
        hex
    });
    format!("\0AUTH EXTERNAL {hex}\r\n")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread::{self, JoinHandle};

    /// A runtime directory whose manager socket answers one authentication
    /// with `answer`, or never answers it.
    fn runtime_dir(answer: Option<&'static str>) -> (tempfile::TempDir, JoinHandle<()>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("systemd")).unwrap();
        let listener = UnixListener::bind(dir.path().join(MANAGER_SOCKET)).unwrap();
        let manager = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&stream).read_line(&mut request).unwrap();
            assert!(request.starts_with("\0AUTH EXTERNAL "), "{request:?}");
            match answer {
                Some(answer) => stream.write_all(answer.as_bytes()).unwrap(),
                // Until the client gives up.
                None => _ = stream.read(&mut [0; 1]),
            }
        });
        (dir, manager)
    }

    fn reach(dir: &Path) -> Result<()> {
        reach_user_manager(Some(dir.as_os_str()))
    }

    #[test]
    fn opens_with_the_uid_in_hex() {
        assert_eq!(auth_request(1000), "\0AUTH EXTERNAL 31303030\r\n");
        assert_eq!(auth_request(0), "\0AUTH EXTERNAL 30\r\n");
    }

    #[test]
    fn reaches_a_manager_that_accepts() {
        let (dir, manager) = runtime_dir(Some("OK 0123456789abcdef\r\n"));
        reach(dir.path()).unwrap();
        manager.join().unwrap();
    }

    #[test]
    fn fails_on_a_manager_that_rejects() {
        let (dir, manager) = runtime_dir(Some("REJECTED EXTERNAL\r\n"));
        let err = reach(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("REJECTED"), "{err:#}");
        manager.join().unwrap();
    }

    #[test]
    fn fails_on_a_manager_that_does_not_answer() {
        let (dir, manager) = runtime_dir(None);
        let err = reach(dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("waiting for an answer"),
            "{err:#}"
        );
        manager.join().unwrap();
    }

    #[test]
    fn fails_without_a_listening_manager() {
        let dir = tempfile::tempdir().unwrap();
        assert!(reach(dir.path()).is_err());
        // A socket left behind by a manager that is gone.
        std::fs::create_dir(dir.path().join("systemd")).unwrap();
        drop(UnixListener::bind(dir.path().join(MANAGER_SOCKET)).unwrap());
        assert!(dir.path().join(MANAGER_SOCKET).exists());
        assert!(reach(dir.path()).is_err());
    }

    #[test]
    fn fails_without_a_runtime_dir() {
        assert!(reach_user_manager(None).is_err());
        assert!(reach_user_manager(Some(OsStr::new(""))).is_err());
    }
}
