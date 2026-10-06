//! How a coordinator is started: the `claude` command line, its prompt, and
//! what is kept of a run. The role, versioned in `role.md`, is appended to
//! Claude Code's system prompt; the prompt carries what changes from one run
//! to the next.
//!
//! A coordinator sees only what its role needs. It loads no user settings,
//! hooks, plugins, MCP servers nor the user's CLAUDE.md, only the project
//! settings of its own directory, which the user may add. It starts from a
//! briefing code gathered (see `state`); it may read more through
//! `orchestrator` commands and note in its journal through one; at setup,
//! write the admission thresholds; outside setup, message sessions. Runs
//! started by `watch` and `setup` are denied anything else without asking
//! (`dontAsk`); an interactive coordinator asks its user.
//! Both modes prompt for permissions, like the sessions they message: Claude
//! Code holds a message from a session that skips permission prompts.

use super::Paths;
use super::queue::Queued;
use super::state::{Briefing, missing};
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
/// What a coordinator may run without asking, exactly as written: reading
/// the state, and noting in its journal.
const ALLOWED: [&str; 6] = [
    "orchestrator sessions --heads",
    "orchestrator admission",
    "orchestrator peaks",
    "orchestrator machine",
    "orchestrator config",
    "orchestrator coordinator note *",
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
    Batch(&'a [Queued]),
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

/// The `claude` command starting a coordinator in `mode` with `prompt`, in
/// its own directory, with `bin` first on its `PATH` so that its
/// `orchestrator` commands are this orchestrator's. `cfg` gives a run its
/// model; an interactive coordinator has its user instead.
// claude-code: coordinator-session
pub fn command(
    mode: &Mode<'_>,
    cfg: &Coordinator,
    paths: &Paths,
    bin: &Path,
    prompt: &str,
) -> Command {
    command_of(OsStr::new("claude"), mode, cfg, paths, bin, prompt)
}

/// `command`, with `program` for `claude`.
pub fn command_of(
    program: &OsStr,
    mode: &Mode<'_>,
    cfg: &Coordinator,
    paths: &Paths,
    bin: &Path,
    prompt: &str,
) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args(mode, cfg, paths, prompt))
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

/// The arguments of `claude` for a coordinator in `mode`, asked `prompt`.
// claude-code: coordinator-session
// claude-code: coordinator-permissions
// claude-code: cross-session-message
pub fn args(mode: &Mode<'_>, cfg: &Coordinator, paths: &Paths, prompt: &str) -> Vec<OsString> {
    let mut tools = vec!["Bash", "Read"];
    let mut allowed: Vec<String> = ALLOWED.iter().map(|c| format!("Bash({c})")).collect();
    match mode {
        Mode::Setup { .. } => allowed.push(format!("Bash({WRITE_ADMISSION})")),
        Mode::Batch(_) => {
            tools.extend(["SendMessage", "ListAgents"]);
            allowed.extend(["SendMessage".to_string(), "ListAgents".to_string()]);
        }
        // The file tools edit the priorities, which it asks its user for,
        // like the thresholds.
        Mode::Interactive => {
            tools.extend(["Edit", "Write", "SendMessage", "ListAgents"]);
            allowed.extend(["SendMessage".to_string(), "ListAgents".to_string()]);
            allowed.push(format!("Bash({NEXT})"));
        }
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
            args.extend(
                [
                    "-p",
                    "--model",
                    &cfg.model,
                    "--permission-mode",
                    "dontAsk",
                    "--no-session-persistence",
                    "--output-format",
                    "json",
                ]
                .map(OsString::from),
            );
            if let Some(usd) = cfg.max_budget_usd {
                args.extend(["--max-budget-usd".into(), usd.to_string().into()]);
            }
        }
    }
    // The prompt last, after `--`: it may start with a dash.
    args.extend(["--".into(), prompt.into()]);
    args
}

/// What the coordinator is asked, after its role, at `now` in seconds since
/// the Unix epoch.
pub fn prompt(mode: &Mode<'_>, paths: &Paths, now: u64, briefing: &Briefing) -> String {
    let mut prompt = format!("Now: {} ({now} s since the Unix epoch).\n\n", utc(now));
    match mode {
        Mode::Setup { reconfigure } => {
            prompt.push_str(if *reconfigure {
                "`orchestrator setup` started you to review the admission thresholds. "
            } else {
                "`orchestrator setup` started you: there are no admission thresholds yet. "
            });
            prompt.push_str(
                "Do the setup your role describes from the state below, gathered just now, then end with a short summary of the thresholds you chose and why, for the user.\n",
            );
        }
        Mode::Batch(batch) => {
            prompt.push_str(
                "orchestrator woke you for the events below. Decide from them and from the state below, gathered when you were woken; run a read command only for something it lacks. Then send your messages and note in your journal, in the same turn when you can. This run ends with your reply: nobody reads answers to your messages, so ask for none. End with one or two sentences saying what you did.\n\n## Events\n\nOldest first, one JSON object per line. `at` is when the latest event of its kind came, `count` how many merged into it, `first_at` when the first came, in seconds since the Unix epoch.\n\n",
            );
            for q in *batch {
                prompt.push_str(&q.line());
                prompt.push('\n');
            }
        }
        Mode::Interactive => {
            let _ = writeln!(
                prompt,
                "The user opened you with `orchestrator coordinator` and is at this terminal. The state below was gathered just now. Run `{NEXT}` with the Bash tool in the background (run_in_background): it ends when events are pending and prints them, with the state at that moment. Each time it ends, handle the events following your role, then start it again: asking for the next batch tells orchestrator you handled the last one. Keep one running for as long as this session lasts; while you are open, no other coordinator starts. Between events, answer the user; change the priorities file or the thresholds only when the user asks you to. Sessions you message may answer while you are open."
            );
        }
    }
    let _ = write!(
        prompt,
        "\n## State when you were woken\n\n{}\n## Your journal, latest lines ({}{})\n\n{}\n\n## The user's priorities ({}{})\n\n{}\n",
        briefing.state,
        paths.journal().display(),
        missing(&paths.journal()),
        briefing.journal.as_deref().unwrap_or("Nothing yet."),
        paths.priorities.display(),
        missing(&paths.priorities),
        briefing.priorities.as_deref().unwrap_or("None written."),
    );
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

/// How a run ended, seen from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub secs: u64,
    /// Its exit code, None when a signal ended it.
    pub exit: Option<i32>,
    /// Stopped at its time limit.
    pub stopped: bool,
}

/// What is kept of a run, one line of `runs.jsonl`: what it used first,
/// then how it ended and what it says it did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunRecord {
    /// When it started, in seconds since the Unix epoch.
    pub at: u64,
    /// `events`, for a run `watch` started, or `setup`.
    pub kind: String,
    pub events: usize,
    pub secs: u64,
    pub turns: Option<u64>,
    /// Input tokens processed, cache writes included, cache reads not.
    pub input_tokens: Option<u64>,
    /// Input tokens read from the prompt cache.
    pub cache_read_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub exit: Option<i32>,
    pub stopped: bool,
    pub is_error: Option<bool>,
    /// Claude Code's estimate at API list price; a subscription counts
    /// tokens against its quota instead.
    pub list_price_estimate_usd: Option<f64>,
    /// Its final reply: what it says it did.
    pub reply: Option<String>,
}

impl RunRecord {
    /// From what the run printed with `--output-format json`.
    // claude-code: print-json-result
    pub fn new(kind: &str, at: u64, events: usize, outcome: Outcome, output: &[u8]) -> RunRecord {
        let result: Value = serde_json::from_slice(output).unwrap_or(Value::Null);
        let usage = &result["usage"];
        let input = [
            usage["input_tokens"].as_u64(),
            usage["cache_creation_input_tokens"].as_u64(),
        ];
        RunRecord {
            at,
            kind: kind.to_string(),
            events,
            secs: outcome.secs,
            turns: result["num_turns"].as_u64(),
            input_tokens: input
                .iter()
                .any(Option::is_some)
                .then(|| input.iter().flatten().sum()),
            cache_read_tokens: usage["cache_read_input_tokens"].as_u64(),
            output_tokens: usage["output_tokens"].as_u64(),
            exit: outcome.exit,
            stopped: outcome.stopped,
            is_error: result["is_error"].as_bool(),
            list_price_estimate_usd: result["total_cost_usd"].as_f64(),
            reply: result["result"].as_str().map(str::to_string),
        }
    }

    /// The run handled its events: it ended by itself, successfully.
    pub fn handled(&self) -> bool {
        self.exit == Some(0) && !self.stopped && self.is_error != Some(true)
    }

    /// How much it took, for a human: tokens and time first.
    pub fn summary(&self) -> String {
        let n = |v: Option<u64>| v.map_or("?".to_string(), |v| v.to_string());
        format!(
            "{} turns, {} s, {} input tokens, {} read from cache, {} output tokens",
            n(self.turns),
            self.secs,
            n(self.input_tokens),
            n(self.cache_read_tokens),
            n(self.output_tokens),
        )
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

    fn batch() -> Vec<Queued> {
        let event = Event::MemoryPressure {
            available_mb: 700,
            stall_ms: 200,
            largest: None,
            next: vec![],
        };
        vec![Queued::new(1_791_210_633, &event).unwrap().unwrap()]
    }

    fn briefing() -> Briefing {
        Briefing {
            state: "### Machine\n\nMemory: 31250 MB total\n\n".into(),
            journal: Some("2026-10-05 14:00 UTC  asked alpha".into()),
            priorities: None,
        }
    }

    #[test]
    fn a_run_reads_messages_and_notes_nothing_else() {
        let cfg = Coordinator::default();
        let batch = batch();
        let args = strings(&args(&Mode::Batch(&batch), &cfg, &paths(), "the prompt"));
        assert_eq!(values(&args, "--model"), ["claude-sonnet-5-5"]);
        assert!(
            !args.contains(&"--max-budget-usd".to_string()),
            "no cap by default"
        );
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
            ["Bash,Read,SendMessage,ListAgents"]
        );
        assert_eq!(
            values(&args, "--allowedTools"),
            [
                "Bash(orchestrator sessions --heads)",
                "Bash(orchestrator admission)",
                "Bash(orchestrator peaks)",
                "Bash(orchestrator machine)",
                "Bash(orchestrator config)",
                "Bash(orchestrator coordinator note *)",
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
        assert_eq!(args[args.len() - 2..], ["--", "the prompt"]);
        let capped = Coordinator {
            max_budget_usd: Some(0.5),
            ..cfg
        };
        let capped = strings(&super::args(&Mode::Batch(&batch), &capped, &paths(), "p"));
        assert_eq!(values(&capped, "--max-budget-usd"), ["0.5"]);
    }

    #[test]
    fn a_run_is_briefed_with_its_events_and_the_state() {
        let batch = batch();
        let prompt = prompt(&Mode::Batch(&batch), &paths(), 1_791_210_633, &briefing());
        assert!(
            prompt.starts_with("Now: 2026-10-05 14:30 UTC (1791210633 s"),
            "{prompt}"
        );
        assert!(prompt.contains(r#""kind":"memory_pressure""#), "{prompt}");
        assert!(prompt.contains(r#""count":1"#), "{prompt}");
        assert!(
            prompt.contains("## State when you were woken\n\n### Machine"),
            "{prompt}"
        );
        assert!(prompt.contains("asked alpha"), "{prompt}");
        assert!(prompt.contains("None written."), "{prompt}");
        assert!(prompt.contains("(does not exist yet)"), "{prompt}");
    }

    #[test]
    fn setup_writes_thresholds_and_messages_no_one() {
        let args = strings(&args(
            &Mode::Setup { reconfigure: false },
            &Coordinator::default(),
            &paths(),
            "p",
        ));
        assert_eq!(values(&args, "--tools"), ["Bash,Read"]);
        let allowed = values(&args, "--allowedTools");
        assert!(allowed.contains(&"Bash(orchestrator config admission *)"));
        assert!(
            !allowed.iter().any(|r| r.contains("SendMessage")),
            "{allowed:?}"
        );
        let prompt = prompt(
            &Mode::Setup { reconfigure: false },
            &paths(),
            0,
            &briefing(),
        );
        assert!(prompt.contains("no admission thresholds yet"), "{prompt}");
    }

    #[test]
    fn an_interactive_coordinator_asks_its_user() {
        let args = strings(&args(
            &Mode::Interactive,
            &Coordinator::default(),
            &paths(),
            "p",
        ));
        assert_eq!(values(&args, "--permission-mode"), ["default"]);
        assert!(!args.contains(&"-p".to_string()));
        assert!(!args.contains(&"--model".to_string()));
        let allowed = values(&args, "--allowedTools");
        assert!(allowed.contains(&"Bash(orchestrator coordinator next)"));
        // Asked of the user, not allowed beforehand.
        assert!(!allowed.iter().any(|r| r.contains("config admission")));
        assert!(
            !allowed
                .iter()
                .any(|r| r.starts_with("Edit") || r.starts_with("Write"))
        );
        assert_eq!(
            values(&args, "--tools"),
            ["Bash,Read,Edit,Write,SendMessage,ListAgents"]
        );
        let prompt = prompt(&Mode::Interactive, &paths(), 0, &briefing());
        assert!(prompt.contains("run_in_background"), "{prompt}");
    }

    #[test]
    fn the_role_names_every_command_it_may_run() {
        for command in ALLOWED.iter().chain([&WRITE_ADMISSION]) {
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

    const DONE: Outcome = Outcome {
        secs: 30,
        exit: Some(0),
        stopped: false,
    };

    #[test]
    fn a_run_record_reports_tokens_and_time_first() {
        let output = br#"{"type":"result","subtype":"success","is_error":false,"num_turns":4,"total_cost_usd":0.012,"usage":{"input_tokens":10,"cache_creation_input_tokens":4000,"cache_read_input_tokens":9000,"output_tokens":300},"result":"Asked alpha to stop its dev server."}"#;
        let r = RunRecord::new("events", 5, 2, DONE, output);
        assert_eq!(r.turns, Some(4));
        assert_eq!(
            (r.input_tokens, r.cache_read_tokens, r.output_tokens),
            (Some(4010), Some(9000), Some(300))
        );
        assert_eq!(r.list_price_estimate_usd, Some(0.012));
        assert_eq!(
            r.reply.as_deref(),
            Some("Asked alpha to stop its dev server.")
        );
        assert!(r.handled());
        assert_eq!(
            r.summary(),
            "4 turns, 30 s, 4010 input tokens, 9000 read from cache, 300 output tokens"
        );
        let line = serde_json::to_string(&r).unwrap();
        assert!(
            line.starts_with(
                r#"{"at":5,"kind":"events","events":2,"secs":30,"turns":4,"input_tokens":4010,"#
            ),
            "{line}"
        );
    }

    #[test]
    fn a_stopped_failed_or_erring_run_did_not_handle_its_events() {
        let killed = RunRecord::new(
            "events",
            5,
            2,
            Outcome {
                exit: None,
                stopped: true,
                ..DONE
            },
            b"",
        );
        assert_eq!((killed.input_tokens, killed.reply.as_deref()), (None, None));
        assert!(!killed.handled());
        let failed = Outcome {
            exit: Some(1),
            ..DONE
        };
        assert!(!RunRecord::new("events", 5, 2, failed, b"{}").handled());
        let erring = br#"{"is_error":true,"result":"Not logged in"}"#;
        assert!(!RunRecord::new("events", 5, 2, DONE, erring).handled());
    }
}
