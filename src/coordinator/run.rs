//! How a coordinator is started: the `claude` command line, its prompt, and
//! what is kept of a run. The role, versioned in `role.md`, is appended to
//! Claude Code's system prompt; the prompt carries what changes from one run
//! to the next.
//!
//! A coordinator sees only what its role needs. It loads no user settings,
//! hooks, plugins, MCP servers nor the user's CLAUDE.md, only the project
//! settings of its own directory, which the user may add. It may read the
//! state through `orchestrator` commands and keep its journal; at setup,
//! write the admission thresholds through one; outside setup, message
//! sessions. Runs started by `watch` and `setup` are denied anything else
//! without asking (`dontAsk`); an interactive coordinator asks its user.
//! Both modes prompt for permissions, like the sessions they message: Claude
//! Code holds a message from a session that skips permission prompts.

use super::{Paths, Pending};
use crate::config::Coordinator;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

/// The role every coordinator gets.
pub const ROLE: &str = include_str!("role.md");
/// The name a coordinator goes by: sessions see their messages come from it.
pub const NAME: &str = "orchestrator-coordinator";
/// What a coordinator may run to read the state, exactly as written.
const READ: [&str; 5] = [
    "orchestrator sessions --heads",
    "orchestrator admission",
    "orchestrator peaks",
    "orchestrator machine",
    "orchestrator config",
];
/// What writes the admission thresholds, allowed without asking at setup
/// only. An interactive coordinator asks its user first; a run for events
/// cannot, and reports thresholds that look wrong instead: one call is too
/// little to judge them by.
const WRITE_ADMISSION: &str = "orchestrator config admission *";
/// What an interactive coordinator runs in the background to receive events.
// claude-code: background-command-wake
pub const NEXT: &str = "orchestrator coordinator next";

/// Why a coordinator starts.
#[derive(Debug)]
pub enum Mode<'a> {
    /// `orchestrator setup`: examine the machine and write the thresholds.
    Setup { reconfigure: bool },
    /// `watch`: handle a batch of events, then end.
    Batch(&'a [Pending]),
    /// `orchestrator coordinator`: stay open with the user, receiving events.
    Interactive,
}

/// Writes the role where `--append-system-prompt-file` reads it, from this
/// binary, so that a coordinator always gets the role of the orchestrator
/// that starts it.
pub fn write_role(paths: &Paths) -> Result<()> {
    fs::create_dir_all(&paths.runtime)
        .with_context(|| format!("creating {}", paths.runtime.display()))?;
    fs::create_dir_all(&paths.home)
        .with_context(|| format!("creating {}", paths.home.display()))?;
    if let Some(dir) = paths.priorities.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(paths.role(), ROLE).with_context(|| format!("writing {}", paths.role().display()))
}

/// The `claude` command starting a coordinator in `mode`, in its own
/// directory, with `bin` first on its `PATH` so that its `orchestrator`
/// commands are this orchestrator's. `cfg` bounds runs; an interactive
/// coordinator has its user instead.
// claude-code: coordinator-session
pub fn command(mode: &Mode<'_>, cfg: &Coordinator, paths: &Paths, bin: &Path, now: u64) -> Command {
    command_of(OsStr::new("claude"), mode, cfg, paths, bin, now)
}

/// `command`, with `program` for `claude`.
pub fn command_of(
    program: &OsStr,
    mode: &Mode<'_>,
    cfg: &Coordinator,
    paths: &Paths,
    bin: &Path,
    now: u64,
) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args(mode, cfg, paths, now))
        .current_dir(&paths.home)
        .env("PATH", search_path(bin));
    if !matches!(mode, Mode::Interactive) {
        // claude -p waits for input on a standard input left open.
        cmd.stdin(Stdio::null());
    }
    cmd
}

/// `bin` first, then the inherited search path.
fn search_path(bin: &Path) -> OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(&inherited));
    std::env::join_paths(dirs).unwrap_or(inherited)
}

/// The arguments of `claude` for a coordinator in `mode`.
// claude-code: coordinator-session
// claude-code: coordinator-permissions
// claude-code: cross-session-message
pub fn args(mode: &Mode<'_>, cfg: &Coordinator, paths: &Paths, now: u64) -> Vec<OsString> {
    let journal = absolute_rule(&paths.journal());
    let mut tools = vec!["Bash", "Read", "Edit", "Write"];
    let mut allowed: Vec<String> = READ.iter().map(|c| format!("Bash({c})")).collect();
    if matches!(mode, Mode::Setup { .. }) {
        allowed.push(format!("Bash({WRITE_ADMISSION})"));
    }
    allowed.push(format!("Edit({journal})"));
    if !matches!(mode, Mode::Setup { .. }) {
        tools.extend(["SendMessage", "ListAgents"]);
        allowed.extend(["SendMessage".to_string(), "ListAgents".to_string()]);
    }
    // What an interactive coordinator does beyond that, its priorities file
    // and the thresholds included, it asks its user first.
    if matches!(mode, Mode::Interactive) {
        allowed.push(format!("Bash({NEXT})"));
    }
    let settings = serde_json::json!({
        "autoMemoryEnabled": false,
        "permissions": { "blockReadsOutsideWorkingDirectories": true },
    });
    let mut args: Vec<OsString> = vec![
        "--name".into(),
        NAME.into(),
        "--append-system-prompt-file".into(),
        paths.role().into(),
        "--setting-sources".into(),
        "project".into(),
        "--settings".into(),
        settings.to_string().into(),
        "--strict-mcp-config".into(),
        "--tools".into(),
        tools.join(",").into(),
        "--allowedTools".into(),
    ];
    args.extend(allowed.into_iter().map(OsString::from));
    // The priorities' directory: readable without asking.
    if let Some(dir) = paths.priorities.parent() {
        args.extend(["--add-dir".into(), dir.as_os_str().to_owned()]);
    }
    match mode {
        Mode::Interactive => args.extend(["--permission-mode".into(), "default".into()]),
        Mode::Setup { .. } | Mode::Batch(_) => {
            let output = if matches!(mode, Mode::Batch(_)) {
                "json"
            } else {
                "text"
            };
            args.extend(
                [
                    "-p",
                    "--model",
                    &cfg.model,
                    "--max-budget-usd",
                    &cfg.max_budget_usd.to_string(),
                    "--permission-mode",
                    "dontAsk",
                    "--no-session-persistence",
                    "--output-format",
                    output,
                ]
                .map(OsString::from),
            );
        }
    }
    // The prompt last, after `--`: it may start with a dash.
    args.extend(["--".into(), prompt(mode, paths, now).into()]);
    args
}

/// `path` as a permission rule names an absolute path.
fn absolute_rule(path: &Path) -> String {
    format!("/{}", path.display())
}

/// What the coordinator is asked, after its role.
pub fn prompt(mode: &Mode<'_>, paths: &Paths, now: u64) -> String {
    let state = |path: &Path| {
        if path.exists() {
            ""
        } else {
            ", which does not exist yet"
        }
    };
    let mut prompt = format!(
        "Now: {} ({now} s since the Unix epoch).\nYour journal: {}{}. The user's priorities: {}{}.\n\n",
        utc(now),
        paths.journal().display(),
        state(&paths.journal()),
        paths.priorities.display(),
        state(&paths.priorities),
    );
    match mode {
        Mode::Setup { reconfigure } => {
            prompt.push_str(if *reconfigure {
                "`orchestrator setup` started you to review the admission thresholds. "
            } else {
                "`orchestrator setup` started you: there are no admission thresholds yet. "
            });
            prompt.push_str(
                "Do the setup your role describes, then end with a short summary of the thresholds you chose and why, for the user.",
            );
        }
        Mode::Batch(batch) => {
            prompt.push_str(
                "orchestrator woke you for these events, oldest first, one JSON object per line. `at` is when the latest event of its kind came, `count` how many merged into it, `first_at` when the first came, in seconds since the Unix epoch:\n\n",
            );
            for p in *batch {
                prompt.push_str(&p.line());
                prompt.push('\n');
            }
            prompt.push_str(
                "\nHandle them following your role. This run ends with your reply: nobody reads answers to your messages, so ask for none.",
            );
        }
        Mode::Interactive => {
            let _ = write!(
                prompt,
                "The user opened you with `orchestrator coordinator` and is at this terminal. Read your journal and the priorities. Then run `{NEXT}` with the Bash tool in the background (run_in_background): it ends when events are pending, and prints them as JSON lines. Each time it ends, handle the events it printed following your role, then start it again; keep one running for as long as this session lasts. While you are open, no other coordinator starts. Between events, answer the user; change the priorities file or the thresholds only when the user asks you to. Sessions you message may answer while you are open."
            );
        }
    }
    prompt
}

/// `secs` since the Unix epoch as a UTC date and time, to the minute.
pub fn utc(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rest = secs % 86_400;
    // Days to a civil date: Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60
    )
}

/// What is kept of a run `watch` started, one line of `runs.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunRecord {
    /// When it started, in seconds since the Unix epoch.
    pub at: u64,
    pub events: usize,
    pub secs: u64,
    /// Its exit code, None when a signal ended it.
    pub exit: Option<i32>,
    /// `watch` stopped it at its time limit.
    pub stopped: bool,
    pub cost_usd: Option<f64>,
    pub turns: Option<u64>,
    pub is_error: Option<bool>,
    /// Its final reply: what it says it did.
    pub reply: Option<String>,
}

impl RunRecord {
    /// From what the run printed with `--output-format json`.
    // claude-code: print-json-result
    pub fn new(
        at: u64,
        events: usize,
        secs: u64,
        exit: Option<i32>,
        stopped: bool,
        output: &[u8],
    ) -> RunRecord {
        let result: Value = serde_json::from_slice(output).unwrap_or(Value::Null);
        RunRecord {
            at,
            events,
            secs,
            exit,
            stopped,
            cost_usd: result["total_cost_usd"].as_f64(),
            turns: result["num_turns"].as_u64(),
            is_error: result["is_error"].as_bool(),
            reply: result["result"].as_str().map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;

    fn paths() -> Paths {
        Paths::new(
            Path::new("/run/user/1/orchestrator"),
            Path::new("/home/u/.local/state/orchestrator"),
            Path::new("/home/u/.config/orchestrator/config.json"),
        )
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The values following `flag` up to the next flag.
    fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.iter()
            .skip_while(|a| *a != flag)
            .skip(1)
            .take_while(|a| !a.starts_with('-'))
            .map(String::as_str)
            .collect()
    }

    fn batch() -> Vec<Pending> {
        let event = Event::MemoryPressure {
            available_mb: 700,
            stall_ms: 200,
            largest: None,
            next: vec![],
        };
        vec![Pending::new(1_791_210_633, &event).unwrap().unwrap()]
    }

    #[test]
    fn a_run_is_bounded_and_denies_what_it_was_not_given() {
        let cfg = Coordinator::default();
        let batch = batch();
        let args = strings(&args(&Mode::Batch(&batch), &cfg, &paths(), 1_791_210_633));
        assert_eq!(values(&args, "--model"), ["haiku"]);
        assert_eq!(values(&args, "--max-budget-usd"), ["0.25"]);
        assert_eq!(values(&args, "--permission-mode"), ["dontAsk"]);
        assert_eq!(values(&args, "--output-format"), ["json"]);
        assert_eq!(values(&args, "--name"), [NAME]);
        assert_eq!(values(&args, "--setting-sources"), ["project"]);
        assert_eq!(
            values(&args, "--append-system-prompt-file"),
            ["/run/user/1/orchestrator/coordinator/role.md"]
        );
        assert_eq!(
            values(&args, "--tools"),
            ["Bash,Read,Edit,Write,SendMessage,ListAgents"]
        );
        assert_eq!(
            values(&args, "--allowedTools"),
            [
                "Bash(orchestrator sessions --heads)",
                "Bash(orchestrator admission)",
                "Bash(orchestrator peaks)",
                "Bash(orchestrator machine)",
                "Bash(orchestrator config)",
                "Edit(//home/u/.local/state/orchestrator/coordinator/journal.md)",
                "SendMessage",
                "ListAgents",
            ]
        );
        assert_eq!(values(&args, "--add-dir"), ["/home/u/.config/orchestrator"]);
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"--no-session-persistence".to_string()));
        let settings: Value = serde_json::from_str(values(&args, "--settings")[0]).unwrap();
        assert_eq!(
            settings["permissions"]["blockReadsOutsideWorkingDirectories"],
            true
        );
        let prompt = args.last().unwrap();
        assert_eq!(args[args.len() - 2], "--");
        assert!(
            prompt.starts_with("Now: 2026-10-05 14:30 UTC (1791210633 s"),
            "{prompt}"
        );
        assert!(prompt.contains(r#""kind":"memory_pressure""#), "{prompt}");
        assert!(prompt.contains(r#""count":1"#), "{prompt}");
    }

    #[test]
    fn setup_messages_no_one() {
        let args = strings(&args(
            &Mode::Setup { reconfigure: false },
            &Coordinator::default(),
            &paths(),
            0,
        ));
        assert_eq!(values(&args, "--tools"), ["Bash,Read,Edit,Write"]);
        let allowed = values(&args, "--allowedTools");
        assert!(allowed.contains(&"Bash(orchestrator config admission *)"));
        assert!(
            !allowed.iter().any(|r| r.contains("SendMessage")),
            "{allowed:?}"
        );
        assert_eq!(values(&args, "--output-format"), ["text"]);
        assert!(args.last().unwrap().contains("no admission thresholds yet"));
    }

    #[test]
    fn an_interactive_coordinator_asks_its_user() {
        let args = strings(&args(
            &Mode::Interactive,
            &Coordinator::default(),
            &paths(),
            0,
        ));
        assert_eq!(values(&args, "--permission-mode"), ["default"]);
        assert!(!args.contains(&"-p".to_string()));
        assert!(!args.contains(&"--max-budget-usd".to_string()));
        let allowed = values(&args, "--allowedTools");
        assert!(allowed.contains(&"Bash(orchestrator coordinator next)"));
        // Asked of the user, not allowed beforehand.
        assert!(!allowed.iter().any(|r| r.contains("config admission")));
        assert!(!allowed.iter().any(|r| r.contains("priorities")));
        assert!(args.last().unwrap().contains("run_in_background"));
    }

    #[test]
    fn the_role_names_every_command_it_may_run() {
        for command in READ.iter().chain([&WRITE_ADMISSION]) {
            let shown = command.trim_end_matches(" *");
            assert!(ROLE.contains(&format!("`{shown}")), "{shown}");
        }
        assert!(ROLE.contains(NAME));
    }

    #[test]
    fn dates_in_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1_791_210_633), "2026-10-05 14:30 UTC");
        assert_eq!(utc(951_782_400 + 86_399), "2000-02-29 23:59 UTC");
    }

    #[test]
    fn a_run_record_reads_the_result() {
        let output = br#"{"type":"result","subtype":"success","is_error":false,"num_turns":4,"total_cost_usd":0.012,"result":"Asked alpha to stop its dev server."}"#;
        let r = RunRecord::new(5, 2, 30, Some(0), false, output);
        assert_eq!(r.cost_usd, Some(0.012));
        assert_eq!(r.turns, Some(4));
        assert_eq!(r.is_error, Some(false));
        assert_eq!(
            r.reply.as_deref(),
            Some("Asked alpha to stop its dev server.")
        );
        let killed = RunRecord::new(5, 2, 300, None, true, b"");
        assert_eq!((killed.cost_usd, killed.reply), (None, None));
    }
}
