//! How a coordinator is started: the `claude` command line, its prompt, and
//! what is kept of a run. The role, versioned in `role.md`, is appended to
//! Claude Code's system prompt; the prompt carries what changes from one run
//! to the next.
//!
//! A coordinator sees only what its role needs. It loads no user settings,
//! hooks, plugins, MCP servers nor the user's CLAUDE.md, only the project
//! settings of its own directory, which the user may add. It starts from a
//! briefing code gathered (see `state`); it may read more through
//! `orchestrator` commands and note in its journal through one. In the setup
//! conversation, a session with the user at the terminal, it writes the
//! configuration through the validated `orchestrator config` commands and
//! the priorities; outside setup, it messages sessions. A run `watch` starts
//! is denied anything else without asking (`dontAsk`); a coordinator with
//! the user at the terminal asks them. Both prompt for permissions, like the
//! sessions they message: Claude Code holds a message from a session that
//! skips permission prompts.

use super::queue::Queued;
use super::state::{Briefing, missing};
use super::{Paths, instructions};
use anyhow::{Context, Result};
use config::Coordinator;
use serde::Serialize;
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

/// The role every coordinator gets.
pub const ROLE: &str = include_str!("role.md");
/// What a coordinator holding the setup conversation gets on top of its
/// role.
pub const SETUP: &str = include_str!("setup.md");
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
/// What writes the configuration, through validated commands: allowed
/// without asking in the setup conversation, where the user agrees to each
/// value first. An interactive coordinator asks its user; a run for events
/// cannot, and reports thresholds that look wrong instead: one call is too
/// little to judge them by.
const WRITE_CONFIG: [&str; 2] = [
    "orchestrator config admission *",
    "orchestrator config coordinator *",
];
/// What an interactive coordinator runs in the background to receive events.
// claude-code: background-command-wake
pub const NEXT: &str = "orchestrator coordinator next";

/// Why a coordinator starts.
#[derive(Debug)]
pub enum Mode<'a> {
    /// `orchestrator setup`, or `orchestrator coordinator` without a
    /// configuration: a conversation with the user that ends with the
    /// configuration written. `configured` when one exists already.
    Setup { configured: bool },
    /// `watch`: handle a batch of events, then end.
    Batch(&'a [Queued]),
    /// `orchestrator coordinator`: stay open with the user, receiving events.
    Interactive,
}

/// Writes the role where `--append-system-prompt-file` reads it, from this
/// binary, so that a coordinator always gets the role of the orchestrator
/// that starts it; in `mode` setup, with the setup instructions; then the
/// user's instructions for coordinators, imports resolved. The file is
/// rewritten at each start, so an edit of the instructions applies to the
/// next coordinator.
pub fn write_role(paths: &Paths, mode: &Mode<'_>) -> Result<()> {
    fs::create_dir_all(&paths.runtime)
        .with_context(|| format!("creating {}", paths.runtime.display()))?;
    fs::create_dir_all(&paths.home)
        .with_context(|| format!("creating {}", paths.home.display()))?;
    if let Some(dir) = paths.instructions.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut role = match mode {
        Mode::Setup { .. } => format!("{ROLE}\n{SETUP}"),
        Mode::Batch(_) | Mode::Interactive => ROLE.to_string(),
    };
    if let Some(user) = instructions::load(&paths.instructions, home().as_deref()) {
        let _ = write!(
            role,
            "\n# The user's instructions for coordinators\n\nThe user wrote these for you, in {} and the files it imports. They take precedence over this role's defaults; the tools you have stay the same.\n\n{}",
            paths.instructions.display(),
            user.text
        );
    }
    fs::write(paths.role(), role).with_context(|| format!("writing {}", paths.role().display()))
}

fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// The directories holding the user's instruction files, its own directory
/// aside: a coordinator with the user at the terminal may read them, to
/// change an instruction where it lives.
fn instruction_dirs(paths: &Paths) -> Vec<std::path::PathBuf> {
    let own = paths.instructions.parent().map(Path::to_path_buf);
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let files = instructions::load(&paths.instructions, home().as_deref())
        .map(|i| i.files)
        .unwrap_or_default();
    for file in files {
        let dir = fs::canonicalize(&file)
            .ok()
            .and_then(|real| real.parent().map(Path::to_path_buf));
        if let Some(dir) = dir.filter(|d| Some(d) != own.as_ref() && !dirs.contains(d)) {
            dirs.push(dir);
        }
    }
    dirs
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
    let dirs = match mode {
        Mode::Setup { .. } | Mode::Interactive => instruction_dirs(paths),
        Mode::Batch(_) => Vec::new(),
    };
    let mut cmd = Command::new(program);
    cmd.args(args(mode, cfg, paths, &dirs, prompt))
        .current_dir(&paths.home)
        .env("PATH", search_path(bin))
        // The user's instructions come through the role, imports resolved:
        // Claude Code must not load them a second time from `--add-dir`.
        .env_remove("CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD");
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

/// The arguments of `claude` for a coordinator in `mode`, asked `prompt`,
/// with `dirs` readable besides its own.
// claude-code: coordinator-session
// claude-code: coordinator-permissions
// claude-code: cross-session-message
pub fn args(
    mode: &Mode<'_>,
    cfg: &Coordinator,
    paths: &Paths,
    dirs: &[std::path::PathBuf],
    prompt: &str,
) -> Vec<OsString> {
    let mut tools = vec!["Bash", "Read"];
    let mut allowed: Vec<String> = ALLOWED.iter().map(|c| format!("Bash({c})")).collect();
    match mode {
        // The file tools create the user's instructions file when it is
        // missing; its imports, elsewhere, are asked for.
        Mode::Setup { .. } => {
            tools.extend(["Edit", "Write"]);
            allowed.extend(WRITE_CONFIG.iter().map(|c| format!("Bash({c})")));
            allowed.push(format!("Edit({})", absolute_rule(&paths.instructions)));
        }
        Mode::Batch(_) => {
            tools.extend(["SendMessage", "ListAgents"]);
            allowed.extend(["SendMessage".to_string(), "ListAgents".to_string()]);
        }
        // The file tools edit the user's instructions, which it asks its
        // user for, like the thresholds.
        Mode::Interactive => {
            tools.extend(["Edit", "Write", "SendMessage", "ListAgents"]);
            allowed.extend(["SendMessage".to_string(), "ListAgents".to_string()]);
            allowed.push(format!("Bash({NEXT})"));
        }
    }
    // No CLAUDE.md, rules or AGENTS.md from its directory or those above
    // it, the home directory's `.claude/CLAUDE.md` among them: the user's
    // instructions for coordinators come through the role. No agent view:
    // a coordinator moved to the background would outlive the process that
    // holds the coordinator, and run beside the next one.
    let settings = serde_json::json!({
        "autoMemoryEnabled": false,
        "claudeMdExcludes": ["/**"],
        "disableAgentView": true,
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
    // The instructions' directories: readable without asking.
    for dir in paths
        .instructions
        .parent()
        .into_iter()
        .chain(dirs.iter().map(std::path::PathBuf::as_path))
    {
        args.extend(["--add-dir".into(), dir.as_os_str().to_owned()]);
    }
    match mode {
        Mode::Interactive | Mode::Setup { .. } => {
            args.extend(["--permission-mode".into(), "default".into()]);
        }
        Mode::Batch(_) => {
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

/// `path` as a permission rule names an absolute path.
fn absolute_rule(path: &Path) -> String {
    format!("/{}", path.display())
}

/// What the coordinator is asked, after its role, at `now` in seconds since
/// the Unix epoch.
pub fn prompt(mode: &Mode<'_>, paths: &Paths, now: u64, briefing: &Briefing) -> String {
    let mut prompt = format!("Now: {} ({now} s since the Unix epoch).\n\n", utc(now));
    match mode {
        Mode::Setup { configured } => {
            prompt.push_str(if *configured {
                "The user ran `orchestrator setup` to review the configuration shown below. "
            } else {
                "There is no configuration yet: this is orchestrator's first setup on this machine. "
            });
            prompt.push_str(
                "The user is at this terminal. Hold the setup conversation your instructions describe, from the state below, gathered just now; start it now with your first message.\n",
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
                "The user opened you with `orchestrator coordinator` and is at this terminal. The state below was gathered just now. Run `{NEXT}` with the Bash tool in the background (run_in_background): it ends when events are pending and prints them, with the state at that moment. Each time it ends, handle the events following your role, then start it again: asking for the next batch tells orchestrator you handled the last one. Keep one running for as long as this session lasts; while you are open, no other coordinator starts. Between events, answer the user. When the user tells you a priority or a lasting instruction, propose the change and the file it belongs in, the instructions file or the one it imports that holds such things, and write it once they agree; change the thresholds only when the user asks you to. Sessions you message may answer while you are open."
            );
        }
    }
    let tag = &briefing.language.tag;
    let language = match (mode, briefing.language.configured) {
        (Mode::Setup { .. }, false) => format!(
            "The system's language, from its locale, is {tag}. Start the conversation in it, and in your first message offer to switch, as in \"I'll continue in <its name>; tell me if you prefer another.\" Write the language you settle on with `orchestrator config coordinator --language <tag>`."
        ),
        (Mode::Setup { .. }, true) => format!(
            "Language: {tag}, as configured. Hold the conversation in it unless the user asks for another; then write the new one with `orchestrator config coordinator --language <tag>`."
        ),
        (Mode::Batch(_) | Mode::Interactive, _) => format!(
            "Language: {tag}. Write your replies, your journal notes and your messages to sessions in it, whatever other instructions say."
        ),
    };
    let _ = writeln!(prompt, "\n{language}");
    let _ = write!(
        prompt,
        "\n## State when you were woken\n\n{}\n## Your journal, latest lines ({}{})\n\n{}\n\n## The user's instructions\n\n{}{}: {}.\n",
        briefing.state,
        paths.journal().display(),
        missing(&paths.journal()),
        briefing.journal.as_deref().unwrap_or("Nothing yet."),
        paths.instructions.display(),
        missing(&paths.instructions),
        if paths.instructions.exists() {
            "at the end of your system prompt, with the files it imports"
        } else {
            "none written yet"
        },
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
    pub fn new(at: u64, events: usize, outcome: Outcome, output: &[u8]) -> RunRecord {
        let result: Value = serde_json::from_slice(output).unwrap_or(Value::Null);
        let usage = &result["usage"];
        let input = [
            usage["input_tokens"].as_u64(),
            usage["cache_creation_input_tokens"].as_u64(),
        ];
        RunRecord {
            at,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use watch::events::Event;

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
            language: crate::state::Language {
                tag: "fr".into(),
                configured: true,
            },
        }
    }

    #[test]
    fn a_run_reads_messages_and_notes_nothing_else() {
        let cfg = Coordinator::default();
        let batch = batch();
        let args = strings(&args(
            &Mode::Batch(&batch),
            &cfg,
            &paths(),
            &[],
            "the prompt",
        ));
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
        assert_eq!(settings["claudeMdExcludes"], serde_json::json!(["/**"]));
        assert_eq!(settings["disableAgentView"], true);
        assert_eq!(args[args.len() - 2..], ["--", "the prompt"]);
        let capped = Coordinator {
            max_budget_usd: Some(0.5),
            ..cfg
        };
        let capped = strings(&super::args(
            &Mode::Batch(&batch),
            &capped,
            &paths(),
            &[],
            "p",
        ));
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
        assert!(prompt.contains("none written yet"), "{prompt}");
        assert!(
            prompt.contains("Language: fr. Write your replies"),
            "{prompt}"
        );
        assert!(prompt.contains("(does not exist yet)"), "{prompt}");
    }

    #[test]
    fn setup_is_a_conversation_that_writes_through_commands() {
        let setup = Mode::Setup { configured: false };
        let args = strings(&args(&setup, &Coordinator::default(), &paths(), &[], "p"));
        assert_eq!(values(&args, "--permission-mode"), ["default"]);
        assert!(!args.contains(&"-p".to_string()), "interactive");
        assert!(!args.contains(&"--model".to_string()), "the user's model");
        assert_eq!(values(&args, "--tools"), ["Bash,Read,Edit,Write"]);
        let allowed = values(&args, "--allowedTools");
        for rule in [
            "Bash(orchestrator config admission *)",
            "Bash(orchestrator config coordinator *)",
            "Edit(//home/u/.config/orchestrator/CLAUDE.md)",
        ] {
            assert!(allowed.contains(&rule), "{rule} in {allowed:?}");
        }
        assert!(
            !allowed
                .iter()
                .any(|r| r.contains("SendMessage") || r.contains("config.json")),
            "{allowed:?}"
        );
        let prompt = prompt(&setup, &paths(), 0, &briefing());
        assert!(prompt.contains("first setup"), "{prompt}");
        assert!(prompt.contains("### Machine"), "{prompt}");
        let review = prompt_of(&Mode::Setup { configured: true });
        assert!(review.contains("to review the configuration"), "{review}");
        assert!(review.contains("Language: fr, as configured"), "{review}");
        let mut first = briefing();
        first.language.configured = false;
        let prompt = super::prompt(&Mode::Setup { configured: false }, &paths(), 0, &first);
        assert!(
            prompt.contains("The system's language, from its locale, is fr. Start the conversation in it, and in your first message offer to switch"),
            "{prompt}"
        );
        assert!(prompt.contains("--language <tag>"), "{prompt}");
    }

    fn prompt_of(mode: &Mode<'_>) -> String {
        prompt(mode, &paths(), 0, &briefing())
    }

    #[test]
    fn the_role_ends_with_the_user_s_instructions_imports_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let paths = Paths::new(
            &base.join("run"),
            &base.join("state"),
            &base.join("config/config.json"),
        );
        fs::create_dir_all(base.join("elsewhere")).unwrap();
        fs::create_dir_all(base.join("config")).unwrap();
        fs::write(
            base.join("elsewhere/coordinator.md"),
            "Begin each note with KESTREL.\n@priorities.md\n",
        )
        .unwrap();
        fs::write(
            base.join("elsewhere/priorities.md"),
            "Experiments can wait.\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere/coordinator.md"), &paths.instructions)
            .unwrap();
        write_role(&paths, &Mode::Batch(&[])).unwrap();
        let role = fs::read_to_string(paths.role()).unwrap();
        assert!(role.starts_with(ROLE));
        assert!(role.contains("# The user's instructions for coordinators"));
        assert!(role.contains("Begin each note with KESTREL."));
        assert!(role.contains("Experiments can wait."));
        assert_eq!(instruction_dirs(&paths), [base.join("elsewhere")]);
    }

    #[test]
    fn setup_gets_its_instructions_on_top_of_the_role() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(
            &dir.path().join("run"),
            &dir.path().join("state"),
            &dir.path().join("config/config.json"),
        );
        write_role(&paths, &Mode::Setup { configured: false }).unwrap();
        let role = fs::read_to_string(paths.role()).unwrap();
        assert!(role.starts_with(ROLE) && role.ends_with(SETUP));
        write_role(&paths, &Mode::Interactive).unwrap();
        assert_eq!(fs::read_to_string(paths.role()).unwrap(), ROLE);
    }

    #[test]
    fn an_interactive_coordinator_asks_its_user() {
        let args = strings(&args(
            &Mode::Interactive,
            &Coordinator::default(),
            &paths(),
            &[PathBuf::from("/home/u/elsewhere/coordinator")],
            "p",
        ));
        assert_eq!(values(&args, "--permission-mode"), ["default"]);
        let added: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "--add-dir")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(
            added,
            [
                "/home/u/.config/orchestrator",
                "/home/u/elsewhere/coordinator"
            ]
        );
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
        let texts = format!("{ROLE}{SETUP}");
        for command in ALLOWED.iter().chain(&WRITE_CONFIG) {
            let shown = command.trim_end_matches(" *");
            assert!(texts.contains(&format!("`{shown}")), "{shown}");
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
        let r = RunRecord::new(5, 2, DONE, output);
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
        let line = serde_json::to_string(&r).unwrap();
        assert!(
            line.starts_with(r#"{"at":5,"events":2,"secs":30,"turns":4,"input_tokens":4010,"#),
            "{line}"
        );
    }

    #[test]
    fn a_stopped_failed_or_erring_run_did_not_handle_its_events() {
        let killed = RunRecord::new(
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
        assert!(!RunRecord::new(5, 2, failed, b"{}").handled());
        let erring = br#"{"is_error":true,"result":"Not logged in"}"#;
        assert!(!RunRecord::new(5, 2, DONE, erring).handled());
    }
}
