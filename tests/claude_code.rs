//! Checks the installed Claude Code against the contracts orchestrator relies
//! on, listed by id in docs/claude-code-dependency.md. Manual only: it starts
//! a real headless session, which needs a signed-in `claude` on the `PATH`, a
//! systemd user manager with cgroup v2, and spends about a cent of tokens.
//! Run it with `cargo test --test claude_code -- --ignored`.
//!
//! The session runs through `orchestrator launch`, from a temporary git
//! repository, without the user's settings, with a hook and an MCP server of
//! its own. Haiku runs one probe script. The probe, the hook and the MCP
//! server record what they see; the checks read it once the session is over,
//! and the test fails naming every broken contract. The session's
//! `XDG_RUNTIME_DIR` is temporary too, so that a running `orchestrator watch`
//! neither learns from the probe nor removes its job record.

// Clippy exempts only `#[test]` functions; every function here is test code.
#![allow(clippy::expect_used)]

use orchestrator::prefix::{JobRecord, Kind};
use orchestrator::{cgroup, prefix, procfs, recognise, sessions};
use serde_json::Value;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The longest the session may take; it takes about ten seconds.
const TIMEOUT: Duration = Duration::from_secs(180);
/// What the probe writes to its standard error, as Claude should read it.
const STDERR_LINE: &str = "orchestrator-probe-stderr-42";

#[test]
#[ignore = "manual: starts a real Claude Code session; run with --ignored"]
fn the_installed_claude_code_keeps_every_contract() {
    let s = Session::run();
    let checks: [(&str, Checker); 10] = [
        ("shell-prefix-variable", shell_prefix_variable),
        ("shell-prefix-argument", shell_prefix_argument),
        ("shell-prefix-coverage", shell_prefix_coverage),
        ("bash-call-signature", bash_call_signature),
        ("bash-call-eval", bash_call_eval),
        ("bash-call-cwd", bash_call_cwd),
        ("bash-call-output", bash_call_output),
        ("session-file", session_file),
        ("session-file-fields", session_file_fields),
        ("session-id-variable", session_id_variable),
    ];
    let mut report = Vec::new();
    let mut broken = 0;
    for (id, check) in checks {
        match check(&s) {
            Ok(()) => report.push(format!("ok      {id}")),
            Err(why) => {
                broken += 1;
                report.push(format!("BROKEN  {id}: {why}"));
            }
        }
    }
    let report = report.join("\n");
    eprintln!("Claude Code {}\n{report}", s.version);
    assert!(
        broken == 0,
        "Claude Code {} broke {broken} contract(s) of docs/claude-code-dependency.md. They are \
         listed in dependency order: a broken one may break those after it.\n{report}",
        s.version
    );
}

type Check = Result<(), String>;
type Checker = fn(&Session) -> Check;

/// One finished session and what it left to check.
struct Session {
    version: String,
    /// Deleted when the session is dropped.
    _tmp: tempfile::TempDir,
    /// The temporary directory, resolved.
    base: PathBuf,
    repo: PathBuf,
    probe: PathBuf,
    /// The session's output, as `--output-format stream-json` prints it.
    output: String,
}

impl Session {
    fn run() -> Session {
        let version = claude_version();
        let tmp = tempfile::tempdir().expect("creating a temporary directory");
        let base = tmp
            .path()
            .canonicalize()
            .expect("resolving the temporary directory");
        let repo = base.join("repo");
        fs::create_dir(&repo).expect("creating the repository");
        let git = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status();
        assert!(git.is_ok_and(|s| s.success()), "git init failed");
        for dir in ["out", "run"] {
            fs::create_dir(base.join(dir)).expect("creating a directory");
        }
        let out = base.join("out");
        let sessions = sessions::default_dir().expect("neither CLAUDE_CONFIG_DIR nor HOME is set");
        let probe = base.join("probe.sh");
        fs::write(&probe, probe_script(&out, &sessions)).expect("writing the probe");
        let settings = base.join("settings.json");
        let mcp = base.join("mcp.json");
        write_json(&settings, &session_settings(&out));
        write_json(&mcp, &mcp_config(&out));

        let prompt = format!(
            "This is an automated compatibility test of orchestrator, a resource monitor for the \
             Claude Code sessions of this machine. Run exactly this command with the Bash tool, \
             once, then reply with the single word done: bash {}",
            probe.display()
        );
        let stdout = base.join("stdout");
        let stderr = base.join("stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
            .args(["launch", "--", "env"])
            .arg(format!("XDG_RUNTIME_DIR={}", base.join("run").display()))
            .args(["claude", "-p", &prompt, "--model", "haiku"])
            .args(["--tools", "Bash", "--allowedTools", "Bash"])
            // No user, plugin or project settings: only this hook.
            .args(["--setting-sources", "project", "--settings"])
            .arg(&settings)
            .args(["--strict-mcp-config", "--mcp-config"])
            .arg(&mcp)
            .args(["--no-session-persistence", "--max-budget-usd", "0.25"])
            .args(["--output-format", "stream-json", "--verbose"])
            .current_dir(&repo)
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).expect("creating the output file"))
            .stderr(File::create(&stderr).expect("creating the error file"))
            .spawn()
            .expect("starting orchestrator launch");
        let status = wait(child);
        let output = fs::read_to_string(&stdout).unwrap_or_default();
        let errors = fs::read_to_string(&stderr).unwrap_or_default();
        assert!(
            status.success(),
            "the session failed ({status}); this is not a broken contract yet. Its error output:\n{}",
            tail(&errors)
        );
        let s = Session {
            version,
            _tmp: tmp,
            base,
            repo,
            probe,
            output,
        };
        let commands = s.bash_commands();
        assert!(
            commands.iter().any(|c| c.contains("probe.sh")),
            "Haiku did not run the probe, so no contract was checked. Its Bash calls: {commands:?}; \
             its reply: {:?}",
            s.reply()
        );
        s
    }

    /// What the probe, the hook or the MCP server recorded under `name`.
    fn read(&self, name: &str) -> Result<String, String> {
        let path = self.base.join("out").join(name);
        fs::read_to_string(&path).map_err(|_| format!("nothing was recorded as `{name}`"))
    }

    /// The job group `who` ran in: the session's path, the job's name.
    fn job(&self, who: &str) -> Result<(String, String), String> {
        let text = self.read(&format!("{who}-cgroup"))?;
        let path = cgroup::own_path(&text).ok_or("no cgroup v2 line")?;
        let session = cgroup::session_of(path)
            .ok_or_else(|| format!("{} ran outside any orchestrated session: {path}", name(who)))?;
        let job = path
            .strip_prefix(session)
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|job| prefix::parse_job_name(job).is_some())
            .ok_or_else(|| format!("{} ran in {path}, not in a job group", name(who)))?;
        Ok((session.to_string(), job.to_string()))
    }

    /// The prefix's record of the probe's Bash call.
    fn record(&self) -> Result<JobRecord, String> {
        let (session, job) = self.job("bash")?;
        let dir = prefix::records_dir(&self.base.join("run/orchestrator"), &session);
        let text = fs::read(dir.join(format!("{job}.json")))
            .map_err(|_| format!("the prefix kept no record of {job}"))?;
        serde_json::from_slice(&text).map_err(|e| format!("unreadable record of {job}: {e}"))
    }

    /// The session file the probe found, with the pid naming it.
    fn session_file(&self) -> Result<(u32, sessions::ClaudeSession), String> {
        let dir = self.base.join("out/sessions");
        let found = sessions::read_sessions(&dir)
            .map_err(|_| "no ancestor of the probe has a session file".to_string())?;
        let [session] = found.as_slice() else {
            return Err(format!("{} readable session files, not one", found.len()));
        };
        let named = fs::read_dir(&dir)
            .ok()
            .and_then(|mut e| e.next())
            .and_then(Result::ok)
            .and_then(|e| e.path().file_stem()?.to_str()?.parse().ok())
            .ok_or("the session file is not named by a pid")?;
        Ok((named, session.clone()))
    }

    /// The commands of the session's Bash calls.
    fn bash_commands(&self) -> Vec<String> {
        self.content_blocks()
            .filter(|b| b["type"] == "tool_use" && b["name"] == "Bash")
            .filter_map(|b| b["input"]["command"].as_str().map(str::to_string))
            .collect()
    }

    fn reply(&self) -> Option<String> {
        self.events()
            .find(|e| e["type"] == "result")
            .and_then(|e| e["result"].as_str().map(str::to_string))
    }

    fn events(&self) -> impl Iterator<Item = Value> + '_ {
        self.output
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    }

    fn content_blocks(&self) -> impl Iterator<Item = Value> + '_ {
        self.events().flat_map(|e| match &e["message"]["content"] {
            Value::Array(blocks) => blocks.clone(),
            _ => Vec::new(),
        })
    }
}

/// The probe's call runs in a job group: Claude Code ran the prefix.
fn shell_prefix_variable(s: &Session) -> Check {
    s.job("bash").map(|_| ())
}

/// Each command run through the prefix ran whole: the prefix takes exactly
/// one argument, and a hook of several commands or an MCP server with
/// arguments only completes when that argument is its whole command line.
fn shell_prefix_argument(s: &Session) -> Check {
    for who in ["probe", "hook", "mcp"] {
        s.read(&format!("{who}-done"))
            .map_err(|_| format!("{} did not run to its end", name(who)))?;
    }
    Ok(())
}

fn shell_prefix_coverage(s: &Session) -> Check {
    for who in ["bash", "hook", "mcp"] {
        s.job(who)?;
    }
    Ok(())
}

fn bash_call_signature(s: &Session) -> Check {
    for (who, kind) in [
        ("bash", Kind::Bash),
        ("hook", Kind::Other),
        ("mcp", Kind::Other),
    ] {
        let (_, job) = s.job(who)?;
        if prefix::parse_job_name(&job).map(|(k, _)| k) != Some(kind) {
            return Err(format!(
                "{} ran in {job}, taken for a {kind:?} job",
                name(who)
            ));
        }
    }
    Ok(())
}

fn bash_call_eval(s: &Session) -> Check {
    let record = s.record()?;
    let script = recognise::written(&record.command)
        .ok_or("no command found in the invocation (see the record)")?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let commands = recognise::commands(&script, &s.repo, home.as_deref())
        .ok_or_else(|| format!("the command found does not parse: {script}"))?;
    let words: Vec<_> = commands.iter().map(|c| c.words.join(" ")).collect();
    let expected = format!("bash {}", s.probe.display());
    if words != [expected.clone()] {
        return Err(format!(
            "recognised {words:?} in {script:?}, expected {expected:?}"
        ));
    }
    Ok(())
}

fn bash_call_cwd(s: &Session) -> Check {
    let record = s.record()?;
    let probe_cwd = s.read("bash-cwd")?;
    let repo = s.repo.to_string_lossy();
    if record.cwd != repo || probe_cwd.trim_end() != repo {
        return Err(format!(
            "the prefix started in {:?} and the command ran in {:?}, not in {repo:?}",
            record.cwd,
            probe_cwd.trim_end()
        ));
    }
    Ok(())
}

fn bash_call_output(s: &Session) -> Check {
    let reached = s
        .content_blocks()
        .filter(|b| b["type"] == "tool_result")
        .any(|b| b["content"].to_string().contains(STDERR_LINE));
    if reached {
        Ok(())
    } else {
        Err("what the probe wrote to its standard error is not in the call's result".into())
    }
}

/// The probe found a session file named by one of its ancestors: the claude
/// process, in the session's `main/`.
fn session_file(s: &Session) -> Check {
    let (named, session) = s.session_file()?;
    if session.pid != named {
        return Err(format!("{named}.json holds pid {}", session.pid));
    }
    let (scope, _) = s.job("bash")?;
    let text = s.read(&format!("proc/{named}/cgroup"))?;
    let claude = cgroup::own_path(&text).unwrap_or_default();
    if claude != format!("{scope}/main") {
        return Err(format!(
            "{named} runs in {claude}, not as the session's claude"
        ));
    }
    Ok(())
}

fn session_file_fields(s: &Session) -> Check {
    let (pid, session) = s.session_file()?;
    let started = procfs::start_time(&s.base.join("out/proc"), pid);
    let proc_start = session.proc_start.as_deref().and_then(|p| p.parse().ok());
    let mut wrong = Vec::new();
    if session.session_id.is_empty() {
        wrong.push("sessionId is empty".to_string());
    }
    if session.name.is_empty() {
        wrong.push("name is empty".to_string());
    }
    if Path::new(&session.cwd) != s.repo {
        wrong.push(format!("cwd is {:?}", session.cwd));
    }
    if session.status.is_none() {
        wrong.push("status is missing".to_string());
    }
    if started.is_none() || proc_start != started {
        wrong.push(format!(
            "procStart is {:?}, the process started at {started:?}",
            session.proc_start
        ));
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    }
}

fn session_id_variable(s: &Session) -> Check {
    let (_, session) = s.session_file()?;
    for who in ["bash", "hook", "mcp"] {
        let id = s.read(&format!("{who}-session-id"))?;
        if id != session.session_id {
            return Err(format!(
                "{} saw CLAUDE_CODE_SESSION_ID={id:?}, the session is {:?}",
                name(who),
                session.session_id
            ));
        }
    }
    Ok(())
}

/// What recorded under `who`.
fn name(who: &str) -> &'static str {
    match who {
        "bash" => "the probe's Bash call",
        "probe" => "the probe",
        "hook" => "the hook",
        "mcp" => "the MCP server",
        _ => "something",
    }
}

fn claude_version() -> String {
    let out = Command::new("claude")
        .arg("--version")
        .output()
        .expect("running claude --version: is Claude Code on the PATH?");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("unknown")
        .to_string()
}

/// Records the probe's job group, environment and directory, writes a line to
/// its standard error, then copies the session file of the nearest ancestor
/// that has one, with that process's stat and cgroup.
fn probe_script(out: &Path, sessions: &Path) -> String {
    let (out, sessions) = (quote(out), quote(sessions));
    format!(
        r#"out={out}
sessions={sessions}
cat /proc/self/cgroup > "$out/bash-cgroup"
printf %s "${{CLAUDE_CODE_SESSION_ID-}}" > "$out/bash-session-id"
pwd -P > "$out/bash-cwd"
echo "orchestrator-probe-stderr-$((6 * 7))" >&2
pid=$$
while [ "${{pid:-0}}" -gt 1 ]; do
  if [ -f "$sessions/$pid.json" ]; then
    mkdir -p "$out/sessions" "$out/proc/$pid"
    cat "$sessions/$pid.json" > "$out/sessions/$pid.json"
    cat "/proc/$pid/stat" > "$out/proc/$pid/stat"
    cat "/proc/$pid/cgroup" > "$out/proc/$pid/cgroup"
    break
  fi
  pid=$(sed -n 's/^PPid:[[:space:]]*//p' "/proc/$pid/status")
done
: > "$out/probe-done"
"#
    )
}

/// A shell command that records, under `who`, its job group and session id.
fn recorder(out: &Path, who: &str) -> String {
    let out = quote(out);
    format!(
        "cat /proc/self/cgroup > {out}/{who}-cgroup; \
         printf %s \"${{CLAUDE_CODE_SESSION_ID-}}\" > {out}/{who}-session-id; \
         : > {out}/{who}-done"
    )
}

/// A shell-form hook before each Bash call. Without auto memory, the session
/// leaves nothing under `~/.claude/projects/`.
fn session_settings(out: &Path) -> Value {
    serde_json::json!({
        "autoMemoryEnabled": false,
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": recorder(out, "hook") }]
            }]
        }
    })
}

/// A stdio MCP server that records, then exits: Claude Code reports it
/// failed, which is fine.
fn mcp_config(out: &Path) -> Value {
    serde_json::json!({
        "mcpServers": {
            "probe": { "command": "sh", "args": ["-c", recorder(out, "mcp")] }
        }
    })
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, value.to_string()).expect("writing a configuration file");
}

/// Waits for the session, killing its whole scope past `TIMEOUT`. A scope
/// left with processes once claude is gone is killed too.
fn wait(mut child: Child) -> ExitStatus {
    let start = Instant::now();
    let scope = session_scope(child.id());
    loop {
        if let Some(status) = child.try_wait().expect("waiting for the session") {
            if let Some(scope) = &scope {
                kill_scope(scope);
            }
            return status;
        }
        if start.elapsed() > TIMEOUT {
            match session_scope(child.id()).or(scope) {
                Some(scope) => kill_scope(&scope),
                None => child.kill().expect("killing the session"),
            }
            let _ = child.wait();
            panic!("the session did not end within {TIMEOUT:?}; it was killed");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The scope of an orchestrated session, from the process it runs. Read once
/// the process has entered it.
fn session_scope(pid: u32) -> Option<PathBuf> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        if let Some(session) = cgroup::own_path(&text).and_then(cgroup::session_of) {
            return Some(Path::new(cgroup::ROOT).join(session.trim_start_matches('/')));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Kills every process left in `scope`, if it still exists.
fn kill_scope(scope: &Path) {
    let _ = fs::write(scope.join("cgroup.kill"), "1");
}

/// `path` single-quoted for sh.
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// The last lines of `text`.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}
