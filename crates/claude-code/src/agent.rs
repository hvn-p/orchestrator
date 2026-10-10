//! Running an agent: a Claude Code session orchestrator starts with a role
//! of its own, allowed only what it is given.

use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What a prompt calls the agent's tools, by what they do.
pub const SHELL_TOOL: &str = "Bash";
pub const READ_TOOL: &str = "Read";
pub const EDIT_TOOL: &str = "Edit";
pub const WRITE_TOOL: &str = "Write";
pub const MESSAGE_TOOL: &str = "SendMessage";
pub const LIST_TOOL: &str = "ListAgents";
/// The shell tool's input that runs a command in the background.
pub const IN_BACKGROUND: &str = "run_in_background";

/// `text`, a prompt written for any agent, with each `{{shell_tool}}`,
/// `{{message_tool}}`, `{{list_tool}}` and `{{in_background}}` replaced by
/// what this agent calls it.
pub fn render(text: &str) -> String {
    text.replace("{{shell_tool}}", SHELL_TOOL)
        .replace("{{message_tool}}", MESSAGE_TOOL)
        .replace("{{list_tool}}", LIST_TOOL)
        .replace("{{in_background}}", IN_BACKGROUND)
}

/// An agent to start, in orchestrator's terms.
#[derive(Debug)]
pub struct Agent<'a> {
    /// The name it goes by: sessions see its messages come from it.
    pub name: &'a str,
    /// A file appended to its system prompt.
    pub role: &'a Path,
    /// Its working directory, whose project settings are the only ones it
    /// loads.
    pub dir: &'a Path,
    /// A directory put first on its `PATH`.
    pub bin: &'a Path,
    /// Directories it may read besides its own.
    pub readable: Vec<PathBuf>,
    /// Commands it may run without asking, exactly as written; a trailing
    /// ` *` stands for any arguments.
    pub commands: Vec<String>,
    /// Files it may edit without asking.
    pub editable: Vec<PathBuf>,
    /// It may create and edit files, asking for those not `editable`.
    pub edits_files: bool,
    /// It may message other sessions and list them, without asking.
    pub messages: bool,
    /// A command it runs in the background to wait for work, allowed
    /// without asking: when the command ends, the agent wakes.
    pub waits_on: Option<&'a str>,
    pub run: Run<'a>,
    /// What it is asked first.
    pub prompt: &'a str,
}

/// How an agent runs.
#[derive(Debug, PartialEq)]
pub enum Run<'a> {
    /// With the user at the terminal, asking them for anything not allowed.
    Interactive,
    /// Without a terminal: handles its prompt, then ends, denied anything
    /// not allowed. It prints its result as JSON (see `RunResult`).
    Headless {
        model: &'a str,
        /// A spending cap, against Claude Code's estimate at API list price.
        max_budget_usd: Option<f64>,
    },
}

/// `program`, which is `claude`, starting `agent`.
// claude-code: coordinator-session
pub fn command(program: &OsStr, agent: &Agent<'_>) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args(agent))
        .current_dir(agent.dir)
        .env("PATH", search_path(agent.bin))
        // The role carries the instructions it should have: Claude Code must
        // not load more from `--add-dir`.
        .env_remove("CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD");
    if matches!(agent.run, Run::Headless { .. }) {
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

/// The arguments of `claude` starting `agent`.
// claude-code: coordinator-session
// claude-code: coordinator-permissions
// claude-code: cross-session-message
// claude-code: background-command-wake
pub fn args(agent: &Agent<'_>) -> Vec<OsString> {
    let mut tools = vec![SHELL_TOOL, READ_TOOL];
    let mut allowed: Vec<String> = agent
        .commands
        .iter()
        .map(|c| format!("{SHELL_TOOL}({c})"))
        .collect();
    if agent.edits_files {
        tools.extend([EDIT_TOOL, WRITE_TOOL]);
    }
    allowed.extend(
        agent
            .editable
            .iter()
            .map(|path| format!("{EDIT_TOOL}(/{})", path.display())),
    );
    if agent.messages {
        tools.extend([MESSAGE_TOOL, LIST_TOOL]);
        allowed.extend([MESSAGE_TOOL.to_string(), LIST_TOOL.to_string()]);
    }
    if let Some(command) = agent.waits_on {
        allowed.push(format!("{SHELL_TOOL}({command})"));
    }
    // No CLAUDE.md, rules or AGENTS.md from its directory or those above
    // it, the home directory's `.claude/CLAUDE.md` among them: its role
    // carries its instructions. No agent view: an agent moved to the
    // background would outlive the process that started it.
    let settings = serde_json::json!({
        "autoMemoryEnabled": false,
        "claudeMdExcludes": ["/**"],
        "disableAgentView": true,
        "permissions": { "blockReadsOutsideWorkingDirectories": true },
    });
    let mut args: Vec<OsString> = vec![
        "--name".into(),
        agent.name.into(),
        "--append-system-prompt-file".into(),
        agent.role.into(),
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
    for dir in &agent.readable {
        args.extend(["--add-dir".into(), dir.as_os_str().to_owned()]);
    }
    match agent.run {
        Run::Interactive => {
            args.extend(["--permission-mode".into(), "default".into()]);
        }
        Run::Headless {
            model,
            max_budget_usd,
        } => {
            args.extend(
                [
                    "-p",
                    "--model",
                    model,
                    "--permission-mode",
                    "dontAsk",
                    "--no-session-persistence",
                    "--output-format",
                    "json",
                ]
                .map(OsString::from),
            );
            if let Some(usd) = max_budget_usd {
                args.extend(["--max-budget-usd".into(), usd.to_string().into()]);
            }
        }
    }
    // The prompt last, after `--`: it may start with a dash.
    args.extend(["--".into(), agent.prompt.into()]);
    args
}

/// What a headless run says of itself when it ends.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunResult {
    pub turns: Option<u64>,
    /// Input tokens processed, cache writes included, cache reads not.
    pub input_tokens: Option<u64>,
    /// Input tokens read from the prompt cache.
    pub cache_read_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub is_error: Option<bool>,
    /// Claude Code's estimate at API list price; a subscription counts
    /// tokens against its quota instead.
    pub list_price_estimate_usd: Option<f64>,
    /// Its final reply.
    pub reply: Option<String>,
}

impl RunResult {
    /// From what a headless run printed. Anything missing stays unknown.
    // claude-code: print-json-result
    pub fn parse(output: &[u8]) -> RunResult {
        let result: Value = serde_json::from_slice(output).unwrap_or(Value::Null);
        let usage = &result["usage"];
        let input = [
            usage["input_tokens"].as_u64(),
            usage["cache_creation_input_tokens"].as_u64(),
        ];
        RunResult {
            turns: result["num_turns"].as_u64(),
            input_tokens: input
                .iter()
                .any(Option::is_some)
                .then(|| input.iter().flatten().sum()),
            cache_read_tokens: usage["cache_read_input_tokens"].as_u64(),
            output_tokens: usage["output_tokens"].as_u64(),
            is_error: result["is_error"].as_bool(),
            list_price_estimate_usd: result["total_cost_usd"].as_f64(),
            reply: result["result"].as_str().map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn headless(max_budget_usd: Option<f64>) -> Agent<'static> {
        Agent {
            name: "an-agent",
            role: Path::new("/run/role.md"),
            dir: Path::new("/state/agent"),
            bin: Path::new("/opt/bin"),
            readable: vec![PathBuf::from("/config")],
            commands: vec!["tool read".into(), "tool note *".into()],
            editable: Vec::new(),
            edits_files: false,
            messages: true,
            waits_on: None,
            run: Run::Headless {
                model: "a-model",
                max_budget_usd,
            },
            prompt: "the prompt",
        }
    }

    #[test]
    fn a_headless_agent_runs_its_commands_and_messages_nothing_else() {
        let args = strings(&args(&headless(None)));
        assert_eq!(values(&args, "--model"), ["a-model"]);
        assert!(
            !args.contains(&"--max-budget-usd".to_string()),
            "no cap unless given"
        );
        assert_eq!(values(&args, "--permission-mode"), ["dontAsk"]);
        assert_eq!(values(&args, "--output-format"), ["json"]);
        assert_eq!(values(&args, "--name"), ["an-agent"]);
        assert_eq!(values(&args, "--setting-sources"), ["project"]);
        assert_eq!(
            values(&args, "--append-system-prompt-file"),
            ["/run/role.md"]
        );
        assert_eq!(
            values(&args, "--tools"),
            ["Bash,Read,SendMessage,ListAgents"]
        );
        assert_eq!(
            values(&args, "--allowedTools"),
            [
                "Bash(tool read)",
                "Bash(tool note *)",
                "SendMessage",
                "ListAgents"
            ]
        );
        assert_eq!(values(&args, "--add-dir"), ["/config"]);
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
        let capped = strings(&super::args(&headless(Some(0.5))));
        assert_eq!(values(&capped, "--max-budget-usd"), ["0.5"]);
        let cmd = command(OsStr::new("claude"), &headless(None));
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/state/agent")));
        let path = cmd
            .get_envs()
            .find(|(k, _)| *k == "PATH")
            .and_then(|(_, v)| v)
            .unwrap();
        assert!(path.to_string_lossy().starts_with("/opt/bin"));
    }

    #[test]
    fn an_interactive_agent_asks_its_user_for_the_rest() {
        let agent = Agent {
            readable: vec![PathBuf::from("/config"), PathBuf::from("/elsewhere")],
            editable: vec![PathBuf::from("/config/CLAUDE.md")],
            edits_files: true,
            messages: false,
            waits_on: Some("tool next"),
            run: Run::Interactive,
            ..headless(None)
        };
        let args = strings(&args(&agent));
        assert_eq!(values(&args, "--permission-mode"), ["default"]);
        assert!(!args.contains(&"-p".to_string()), "interactive");
        assert!(!args.contains(&"--model".to_string()), "the user's model");
        assert_eq!(values(&args, "--tools"), ["Bash,Read,Edit,Write"]);
        let allowed = values(&args, "--allowedTools");
        assert!(allowed.contains(&"Edit(//config/CLAUDE.md)"), "{allowed:?}");
        assert!(allowed.contains(&"Bash(tool next)"), "{allowed:?}");
        assert!(
            !allowed.iter().any(|r| r.contains("SendMessage")),
            "{allowed:?}"
        );
        let added: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "--add-dir")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(added, ["/config", "/elsewhere"]);
    }

    #[test]
    fn a_prompt_names_tools_as_the_agent_calls_them() {
        assert_eq!(
            render("{{message_tool}} or {{list_tool}}; {{shell_tool}} with {{in_background}}"),
            "SendMessage or ListAgents; Bash with run_in_background"
        );
    }

    #[test]
    fn a_result_reports_tokens_and_reply() {
        let output = br#"{"type":"result","is_error":false,"num_turns":4,"total_cost_usd":0.012,"usage":{"input_tokens":10,"cache_creation_input_tokens":4000,"cache_read_input_tokens":9000,"output_tokens":300},"result":"Done."}"#;
        let r = RunResult::parse(output);
        assert_eq!(r.turns, Some(4));
        assert_eq!(
            (r.input_tokens, r.cache_read_tokens, r.output_tokens),
            (Some(4010), Some(9000), Some(300))
        );
        assert_eq!(r.list_price_estimate_usd, Some(0.012));
        assert_eq!(r.reply.as_deref(), Some("Done."));
        assert_eq!(RunResult::parse(b""), RunResult::default());
    }
}
