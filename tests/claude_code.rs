//! Checks the installed Claude Code against the contracts orchestrator relies
//! on, listed by id in crates/claude-code/claude-code-dependency.md. Manual only: it starts
//! real headless sessions, which need a signed-in `claude` on the `PATH`, a
//! systemd user manager with cgroup v2, and spend a few tens of thousands of
//! tokens, most read from the prompt cache.
//! Run it with `cargo test --test claude_code -- --ignored`.
//!
//! The session runs through `orchestrator launch`, from a temporary git
//! repository, without the user's settings, with a hook and an MCP server of
//! its own. Haiku runs one probe script. The probe, the hook and the MCP
//! server record what they see; the checks read it once the session is over,
//! and the test fails naming every broken contract. The session's
//! `XDG_RUNTIME_DIR` is temporary too, so that a running `orchestrator watch`
//! neither learns from the probe nor removes its job record.
//!
//! A second test starts a coordinator as `watch` would, with temporary
//! runtime, state and configuration directories, and a test prompt in place
//! of events. It may list the user's sessions but never message them: its
//! `SendMessage` tool is removed.

// Clippy exempts only `#[test]` functions; every function here is test code.
#![allow(clippy::expect_used)]

use claude_code::agent;
use claude_code::invocation::Kind;
use claude_code::sessions;
use config::Coordinator;
use coordinator::{self, run};
use learning::recognise;
use prefix::JobRecord;
use prefix::admission::{self, Waiting};
use serde_json::Value;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use system::cgroup;
use system::procfs;

/// The longest the session may take; it takes about ten seconds.
const TIMEOUT: Duration = Duration::from_secs(180);
/// What the probe writes to its standard error, as Claude should read it.
const STDERR_LINE: &str = "orchestrator-probe-stderr-42";
/// What the probe prints after a process it ran was killed with `SIGKILL`.
const KILLED_LINE: &str = "orchestrator-probe-killed-137";
/// The marker word of a CLAUDE.md above the coordinator's directory, which
/// must not reach it.
const ANCESTOR_MARKER: &str = "ANCESTOR-MARKER-7";
/// A job the coordinator gives priority to: none waits, so the command
/// refuses it, but runs.
const PRIORITY_JOB: &str = "job-bash-1-1";

#[test]
#[ignore = "manual: starts a real Claude Code session; run with --ignored"]
fn the_installed_claude_code_keeps_every_contract() {
    let s = Session::run();
    let checks: [(&str, Checker); 11] = [
        ("shell-prefix-variable", shell_prefix_variable),
        ("shell-prefix-argument", shell_prefix_argument),
        ("shell-prefix-coverage", shell_prefix_coverage),
        ("bash-call-signature", bash_call_signature),
        ("bash-call-eval", bash_call_eval),
        ("bash-call-cwd", bash_call_cwd),
        ("bash-call-shell", bash_call_shell),
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
        "Claude Code {} broke {broken} contract(s) of crates/claude-code/claude-code-dependency.md. They are \
         listed in dependency order: a broken one may break those after it.\n{report}",
        s.version
    );
}

#[test]
#[ignore = "manual: starts a real Claude Code session; run with --ignored"]
fn the_installed_claude_code_runs_a_coordinator() {
    let c = CoordinatorRun::run();
    let checks: [(&str, CoordinatorChecker); 4] = [
        ("coordinator-session", coordinator_session),
        ("coordinator-permissions", coordinator_permissions),
        ("cross-session-message", cross_session_message),
        ("print-json-result", print_json_result),
    ];
    let mut report = Vec::new();
    let mut broken = 0;
    for (id, check) in checks {
        match check(&c) {
            Ok(()) => report.push(format!("ok      {id}")),
            Err(why) => {
                broken += 1;
                report.push(format!("BROKEN  {id}: {why}"));
            }
        }
    }
    let report = report.join("\n");
    let used = run::RunRecord::new(
        0,
        0,
        run::Outcome {
            secs: 0,
            exit: Some(0),
            stopped: false,
        },
        c.result.to_string().as_bytes(),
    );
    eprintln!(
        "Claude Code {} (coordinator: {} turns, {} input tokens, {} from cache, {} output)\n{report}",
        c.version,
        used.turns.unwrap_or(0),
        used.input_tokens.unwrap_or(0),
        used.cache_read_tokens.unwrap_or(0),
        used.output_tokens.unwrap_or(0),
    );
    assert!(
        broken == 0,
        "Claude Code {} broke {broken} contract(s) of crates/claude-code/claude-code-dependency.md while \
         running a coordinator.\n{report}\nIts reply: {:?}",
        c.version,
        c.reply()
    );
}

type Check = Result<(), String>;
type Checker = fn(&Session) -> Check;
type CoordinatorChecker = fn(&CoordinatorRun) -> Check;

/// A finished coordinator run and what it left.
struct CoordinatorRun {
    version: String,
    /// Deleted when the run is dropped.
    _tmp: tempfile::TempDir,
    paths: coordinator::Paths,
    /// Admission's runtime directory, where giving priority writes.
    admission: PathBuf,
    outside: PathBuf,
    /// What it printed: one JSON object.
    result: Value,
}

impl CoordinatorRun {
    fn run() -> CoordinatorRun {
        let version = claude_version();
        let tmp = tempfile::tempdir().expect("creating a temporary directory");
        let base = tmp
            .path()
            .canonicalize()
            .expect("resolving the temporary directory");
        let paths = coordinator::Paths::new(
            &base.join("run/orchestrator"),
            &base.join("state/orchestrator"),
            &base.join("config/orchestrator/config.json"),
        );
        run::write_role(&paths, &run::Mode::Batch(&[])).expect("writing the role");
        let outside = base.join("outside.txt");
        fs::write(&outside, "outside").expect("writing a file outside");
        record_waiting(&base.join("run/orchestrator"));
        fs::write(
            base.join("state/CLAUDE.md"),
            format!("The marker word is {ANCESTOR_MARKER}.\n"),
        )
        .expect("writing a CLAUDE.md above the coordinator's directory");
        let prompt = coordinator_prompt(&paths.home.join("other.md"), &outside);
        let bin = Path::new(env!("CARGO_BIN_EXE_orchestrator"))
            .parent()
            .expect("the binary's directory");
        // A small model and a spending cap, for a test.
        let cfg = Coordinator {
            model: "haiku".into(),
            max_budget_usd: Some(0.25),
            ..Coordinator::default()
        };
        let built = run::command(&run::Mode::Batch(&[]), &cfg, &paths, bin, &prompt);
        let mut args: Vec<OsString> = built.get_args().map(ToOwned::to_owned).collect();
        // Never message the user's sessions from a test.
        let message_tool = format!("{},", agent::MESSAGE_TOOL);
        for a in &mut args {
            if a.to_string_lossy().contains(&message_tool) {
                *a = a.to_string_lossy().replace(&message_tool, "").into();
            }
        }
        let stdout = base.join("stdout");
        let stderr = base.join("stderr");
        let mut cmd = Command::new(built.get_program());
        cmd.args(&args)
            .current_dir(built.get_current_dir().expect("a directory"))
            .env("XDG_RUNTIME_DIR", base.join("run"))
            .env("XDG_STATE_HOME", base.join("state"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).expect("creating the output file"))
            .stderr(File::create(&stderr).expect("creating the error file"));
        for (key, value) in built.get_envs() {
            if let Some(value) = value {
                cmd.env(key, value);
            }
        }
        // The read commands ask `watch`: one serves this test's runtime
        // directory while the coordinator runs.
        let _watch = Watch::start(&base);
        let mut child = cmd.spawn().expect("starting claude");
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("waiting for the coordinator") {
                break status;
            }
            if start.elapsed() > TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the coordinator did not end within {TIMEOUT:?}; it was killed");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let errors = fs::read_to_string(&stderr).unwrap_or_default();
        assert!(
            status.success(),
            "the coordinator failed ({status}); this is not a broken contract yet. Its error \
             output:\n{}",
            tail(&errors)
        );
        let result = fs::read(&stdout)
            .ok()
            .and_then(|out| serde_json::from_slice(&out).ok())
            .unwrap_or(Value::Null);
        CoordinatorRun {
            version,
            _tmp: tmp,
            paths,
            admission: base.join("run/orchestrator"),
            outside,
            result,
        }
    }

    fn reply(&self) -> &str {
        self.result["result"].as_str().unwrap_or_default()
    }

    /// The commands or files of the calls denied to `tool`.
    fn denied(&self, tool: &str, field: &str) -> Vec<String> {
        self.result["permission_denials"]
            .as_array()
            .map(|denials| {
                denials
                    .iter()
                    .filter(|d| d["tool_name"] == tool)
                    .filter_map(|d| d["tool_input"][field].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The test prompt: commands it may run, `other` to create and `outside` to
/// read, which it may not.
fn coordinator_prompt(other: &Path, outside: &Path) -> String {
    format!(
        "This is an automated compatibility test of orchestrator, not an event: message no \
         one. Do these steps in order, one tool call each, and go on after a denial or an \
         error; make every call, even one you expect to be denied. 1) Run `orchestrator \
         machine` with the Bash tool. 2) Run `orchestrator coordinator note \
         checked` with the Bash tool. 3) Run `orchestrator admission priority {PRIORITY_JOB}` \
         with the Bash tool. 4) Run `orchestrator admission priority` with the Bash tool. \
         5) Run `touch {}` with the Bash tool. 6) Run `cat {}` \
         with the Bash tool. 7) Call {list} once. Then reply with exactly five lines: \
         the first line `orchestrator machine` printed, the first line step 3 printed, the \
         line of the {list} result \
         that starts with `This session is`, the first line of the role appended to your system prompt, and the \
         marker word a CLAUDE.md gives you, or NONE.",
        other.display(),
        outside.display(),
        list = agent::LIST_TOOL,
    )
}

/// Records `PRIORITY_JOB` as waiting for memory in `runtime`, in this test's
/// own cgroup, which stays populated while the coordinator runs: a call to
/// give priority to.
fn record_waiting(runtime: &Path) {
    let own = fs::read_to_string("/proc/self/cgroup").expect("reading the test's cgroup");
    let waiting = Waiting {
        job: PRIORITY_JOB.into(),
        group: cgroup::own_path(&own)
            .expect("the test's cgroup v2 path")
            .into(),
        label: "make".into(),
        peak_mb: 1,
        need_mb: 1,
        since_ms: 1,
    };
    let dir = admission::waiting_dir(runtime);
    fs::create_dir_all(&dir).expect("creating the waiting calls' directory");
    fs::write(
        dir.join(format!("{PRIORITY_JOB}.json")),
        serde_json::to_vec(&waiting).expect("serializing a waiting call"),
    )
    .expect("recording a waiting call");
}

/// An `orchestrator watch` serving a test's directories, stopped when
/// dropped.
struct Watch(Child);

impl Watch {
    fn start(base: &Path) -> Watch {
        let child = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
            .arg("watch")
            .env("XDG_RUNTIME_DIR", base.join("run"))
            .env("XDG_STATE_HOME", base.join("state"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting orchestrator watch");
        let socket = base.join("run/orchestrator/api.sock");
        let start = Instant::now();
        while !socket.exists() && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(socket.exists(), "orchestrator watch did not start serving");
        Watch(child)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Its role reached the system prompt, it ran under its name, and no
/// CLAUDE.md above its directory reached it.
fn coordinator_session(c: &CoordinatorRun) -> Check {
    let reply = c.reply();
    if reply.contains(ANCESTOR_MARKER) {
        return Err("a CLAUDE.md above the coordinator's directory reached it".into());
    }
    // Its heading or its first line of text: the model reads "first line"
    // either way.
    let first: Vec<&str> = run::ROLE
        .lines()
        .map(|l| l.trim_start_matches("# ").trim())
        .filter(|l| !l.is_empty())
        .take(2)
        .collect();
    if !first.iter().any(|line| reply.contains(line)) {
        return Err(format!(
            "the reply quotes neither of the role's first lines {first:?}"
        ));
    }
    if !reply.contains(run::NAME) {
        return Err(format!(
            "the reply does not show the session's name {:?}",
            run::NAME
        ));
    }
    Ok(())
}

/// Its allowed commands ran, the journal note and giving priority included,
/// with and without a job; a command it was not given and a read outside
/// its directories were denied without asking.
fn coordinator_permissions(c: &CoordinatorRun) -> Check {
    if !c.reply().contains("Memory:") {
        return Err("`orchestrator machine` did not run, or not this orchestrator".into());
    }
    let journal = fs::read_to_string(c.paths.journal()).unwrap_or_default();
    if !journal.trim_end().ends_with("  checked") {
        return Err(format!("the journal holds {journal:?}, not the note"));
    }
    // Given priority, the waiting call is listed; taken back from all after,
    // the list is empty.
    if !c.reply().contains("Priority, in this order") {
        return Err(format!(
            "`orchestrator admission priority {PRIORITY_JOB}` did not run, or Haiku skipped it (run again); denials: {}",
            c.result["permission_denials"]
        ));
    }
    let priority = fs::read_to_string(c.admission.join("priority.json")).unwrap_or_default();
    if priority != "[]" {
        return Err(format!(
            "`orchestrator admission priority` did not run, or Haiku skipped it (run again): the list holds {priority:?}; denials: {}",
            c.result["permission_denials"]
        ));
    }
    let other = c.paths.home.join("other.md");
    if other.exists() {
        return Err(format!("{} was created", other.display()));
    }
    let denied = c.denied("Bash", "command");
    let touch = format!("touch {}", other.display());
    if !denied.contains(&touch) {
        return Err(format!(
            "`{touch}` was not denied, or Haiku skipped it (run again); denials: {}",
            c.result["permission_denials"]
        ));
    }
    // Asked to `cat` it, Haiku sometimes reads it with Read instead: either
    // call must be denied.
    let outside = c.outside.display().to_string();
    let read = denied.contains(&format!("cat {outside}"))
        || c.denied("Read", "file_path").contains(&outside);
    if !read {
        return Err(format!(
            "reading {outside} was not denied, or Haiku skipped it (run again); denials: {}",
            c.result["permission_denials"]
        ));
    }
    Ok(())
}

/// `ListAgents` runs and names the session itself; delivery is measured, not
/// tested, since it would take a second session.
fn cross_session_message(c: &CoordinatorRun) -> Check {
    if c.reply()
        .contains(&format!("This session is {}", run::NAME))
    {
        Ok(())
    } else {
        Err("the reply does not quote ListAgents naming this session".into())
    }
}

fn print_json_result(c: &CoordinatorRun) -> Check {
    let outcome = run::Outcome {
        secs: 0,
        exit: Some(0),
        stopped: false,
    };
    let record = run::RunRecord::new(0, 0, outcome, c.result.to_string().as_bytes());
    let mut missing = Vec::new();
    if record.reply.is_none() {
        missing.push("result");
    }
    if record.input_tokens.is_none() || record.output_tokens.is_none() {
        missing.push("usage");
    }
    if record.list_price_estimate_usd.is_none() {
        missing.push("total_cost_usd");
    }
    if record.turns.is_none() {
        missing.push("num_turns");
    }
    if record.is_error != Some(false) {
        missing.push("is_error false");
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("the output lacks {}", missing.join(", ")))
    }
}

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
        if prefix::parse_job_name(&job).map(|(k, _, _)| k) != Some(kind) {
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
    let script = claude_code::invocation::written(&record.command)
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

/// The probe's parent, the call's shell, is the pid in the job's name, and
/// a child of the session's claude process; a process the call kills with
/// `SIGKILL`, as the OOM killer does, shows in the call's result.
fn bash_call_shell(s: &Session) -> Check {
    let killed = s
        .content_blocks()
        .filter(|b| b["type"] == "tool_result")
        .any(|b| {
            let result = b["content"].to_string();
            result.contains(KILLED_LINE) && result.contains("Killed")
        });
    if !killed {
        return Err("a process killed during the call does not show in its result".into());
    }
    let (_, job) = s.job("bash")?;
    let (_, named, _) = prefix::parse_job_name(&job).ok_or("unreadable job name")?;
    let number = |what: &str| -> Result<u32, String> {
        s.read(what)?
            .trim()
            .parse()
            .map_err(|_| format!("`{what}` holds no pid"))
    };
    let shell = number("bash-shell")?;
    if shell != named {
        return Err(format!(
            "the call's shell is {shell}, not {named}, the pid {job} is named by"
        ));
    }
    let (claude, _) = s.session_file()?;
    let parent = number("bash-shell-parent")?;
    if parent != claude {
        return Err(format!(
            "the call's shell is a child of {parent}, not of claude ({claude})"
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
echo "$PPID" > "$out/bash-shell"
sed -n 's/^PPid:[[:space:]]*//p' "/proc/$PPID/status" > "$out/bash-shell-parent"
echo "orchestrator-probe-stderr-$((6 * 7))" >&2
echo "orchestrator-probe: killing a test process on purpose"
sh -c 'kill -KILL $$'
echo "orchestrator-probe-killed-$?"
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
