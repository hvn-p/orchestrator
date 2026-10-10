//! What the shell prefix receives from Claude Code, and how it runs it.
//! Claude Code runs every Bash call, hook, status line refresh and stdio
//! MCP server start through the prefix, the whole command line as one
//! argument; a Bash call is a script that sources the session's shell
//! snapshot, runs the command Claude wrote inside an `eval`, then records
//! its working directory.
//!
//! Processes Claude Code starts without the prefix, such as its clipboard
//! helpers, stay in `main/` and count with claude. Claude Code's own memory
//! cap on tool commands (`CLAUDE_CODE_TOOL_MEMORY_LIMIT`) must stay off: it
//! puts commands in a memory cgroup of its own, out of their job groups,
//! and kills them past its limit.

use brush_parser::ast::Command as ShellCommand;
use brush_parser::ast::{CommandPrefixOrSuffixItem, Pipeline, Program, SimpleCommand};
use brush_parser::word::{self, TildeExpr, WordPiece, WordPieceWithSource};
use brush_parser::{Parser, ParserOptions};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Command;

// claude-code: shell-prefix-coverage
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A Bash tool call, or a `!` command typed in Claude Code.
    Bash,
    /// A hook, the status line or an MCP server.
    Other,
}

/// A Bash call sources the session's shell snapshot and records its working
/// directory afterwards; hooks, the status line and MCP servers do neither.
// claude-code: bash-call-signature
pub fn kind(invocation: &str) -> Kind {
    if invocation.contains("/shell-snapshots/snapshot-") || invocation.contains("pwd -P >|") {
        Kind::Bash
    } else {
        Kind::Other
    }
}

/// The command line among the prefix's arguments: Claude Code passes it as
/// one argument. None when it passed anything else.
// claude-code: shell-prefix-argument
pub fn command(args: &[OsString]) -> Option<&OsStr> {
    match args {
        [command] => Some(command),
        _ => None,
    }
}

/// `invocation` run as Claude Code would have run it without the prefix.
// claude-code: shell-prefix-argument
pub fn shell(invocation: &OsStr) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(invocation);
    cmd
}

/// The directory a Bash call runs in: Claude Code starts the prefix there.
// claude-code: bash-call-cwd
pub fn working_dir() -> std::io::Result<PathBuf> {
    std::env::current_dir()
}

/// The command Claude wrote, from the invocation Claude Code hands the shell
/// prefix: the arguments of the invocation's last top-level `eval`, unquoted
/// and joined with spaces, as `eval` joins them.
// claude-code: bash-call-eval
pub fn written(invocation: &str) -> Option<String> {
    let program = parse(invocation)?;
    let eval = top_level(&program)
        .filter_map(|p| match p.seq.as_slice() {
            [ShellCommand::Simple(s)] if is_eval(s) => Some(s),
            _ => None,
        })
        .last()?;
    Some(eval_args(eval)?.join(" "))
}

// claude-code: bash-call-eval
pub fn parser_options() -> ParserOptions {
    // The invocation turns extglob off before its eval.
    ParserOptions {
        enable_extended_globbing: false,
        ..ParserOptions::default()
    }
}

/// `script` parsed as the shell Claude Code runs it parses it.
pub fn parse(script: &str) -> Option<Program> {
    Parser::new(script.as_bytes(), &parser_options())
        .parse_program()
        .ok()
}

/// The pipelines at the top level of `program`.
fn top_level(program: &Program) -> impl Iterator<Item = &Pipeline> {
    program
        .complete_commands
        .iter()
        .flat_map(|list| &list.0)
        .flat_map(|item| &item.0)
        .map(|(_, pipeline)| pipeline)
}

fn is_eval(s: &SimpleCommand) -> bool {
    s.prefix.is_none() && s.word_or_name.as_ref().is_some_and(|w| w.value == "eval")
}

/// The unquoted arguments of an `eval` command. None when only running it
/// would tell them.
fn eval_args(s: &SimpleCommand) -> Option<Vec<String>> {
    let mut args = Vec::new();
    for item in s.suffix.iter().flat_map(|suffix| &suffix.0) {
        match item {
            CommandPrefixOrSuffixItem::Word(w)
            | CommandPrefixOrSuffixItem::AssignmentWord(_, w) => {
                let arg = unquote(&w.value);
                if !arg.literal || arg.home {
                    return None;
                }
                args.push(arg.text);
            }
            CommandPrefixOrSuffixItem::IoRedirect(_) => {}
            CommandPrefixOrSuffixItem::ProcessSubstitution(..) => return None,
        }
    }
    Some(args)
}

/// A word without its quotes. Expansions stay as written: nothing is run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unquoted {
    pub text: String,
    /// The text is the word's value: it holds no expansion.
    pub literal: bool,
    /// The word starts with a `~` standing for the home directory.
    pub home: bool,
}

/// `word` without its quotes, as the shell Claude Code runs it reads it.
pub fn unquote(word: &str) -> Unquoted {
    let Ok(pieces) = word::parse(word, &parser_options()) else {
        return Unquoted {
            text: word.to_string(),
            literal: false,
            home: false,
        };
    };
    let mut out = Unquoted {
        text: String::new(),
        literal: true,
        home: matches!(
            pieces.first().map(|p| &p.piece),
            Some(WordPiece::TildeExpansion(TildeExpr::Home))
        ),
    };
    push_pieces(word, &pieces, &mut out);
    out
}

fn push_pieces(word: &str, pieces: &[WordPieceWithSource], out: &mut Unquoted) {
    for p in pieces {
        match &p.piece {
            WordPiece::Text(s) | WordPiece::SingleQuotedText(s) => out.text.push_str(s),
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => push_pieces(word, inner, out),
            WordPiece::EscapeSequence(s) => out.text.push_str(s.strip_prefix('\\').unwrap_or(s)),
            // `$'…'` keeps its escapes unprocessed.
            WordPiece::AnsiCQuotedText(s) => {
                out.text.push_str(s);
                out.literal &= !s.contains('\\');
            }
            WordPiece::TildeExpansion(_)
            | WordPiece::ParameterExpansion(_)
            | WordPiece::CommandSubstitution(_)
            | WordPiece::BackquotedCommandSubstitution(_)
            | WordPiece::ArithmeticExpression(_) => {
                out.text
                    .push_str(word.get(p.start_index..p.end_index).unwrap_or_default());
                out.literal = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Bash call as Claude Code 2.1.289 hands it to the prefix.
    const BASH_CALL: &str = "source /home/u/.claude/shell-snapshots/snapshot-bash-1-x.sh 2>/dev/null || true && { shopt -u extglob || setopt NO_EXTENDED_GLOB NO_BARE_GLOB; } >/dev/null 2>&1 || true && eval 'echo hi' && pwd -P >| /tmp/claude-ab12-cwd";

    #[test]
    fn tells_bash_calls_from_the_rest() {
        assert_eq!(kind(BASH_CALL), Kind::Bash);
        assert_eq!(kind("bash ~/.claude/hooks/gate.sh"), Kind::Other);
        assert_eq!(kind("npx -y @scope/mcp-server"), Kind::Other);
    }

    fn invocation(eval: &str) -> String {
        format!(
            "source /home/u/.claude/shell-snapshots/snapshot-bash-1-x.sh 2>/dev/null || true && {{ shopt -u extglob || setopt NO_EXTENDED_GLOB NO_BARE_GLOB_QUAL; }} >/dev/null 2>&1 || true && {{ \\builtin unalias -- 'unsetenv'; \\builtin unset -f -- 'unsetenv'; }} >/dev/null 2>&1 || true && eval {eval} && pwd -P >| /tmp/claude-ab12-cwd"
        )
    }

    #[test]
    fn extracts_a_single_quoted_command() {
        let call = invocation(r#"'python3 -c '"'"'x=1'"'"''"#);
        assert_eq!(written(&call).as_deref(), Some("python3 -c 'x=1'"));
    }
    #[test]
    fn extracts_a_bare_word_followed_by_a_redirection() {
        let call = invocation("true < /dev/null");
        assert_eq!(written(&call).as_deref(), Some("true"));
    }
    #[test]
    fn extracts_a_double_quoted_command_and_joins_arguments() {
        let call = invocation(r#""echo \"\$HOME\"" < /dev/null"#);
        assert_eq!(written(&call).as_deref(), Some(r#"echo "$HOME""#));
        assert_eq!(written(&invocation("ls -la")).as_deref(), Some("ls -la"));
    }
    #[test]
    fn extracts_across_a_multi_line_preamble() {
        let call = format!("export A=1\n: && {}", invocation("'make test'"));
        assert_eq!(written(&call).as_deref(), Some("make test"));
    }
    #[test]
    fn no_eval_or_an_unknown_argument_yields_nothing() {
        assert_eq!(written("bash ~/.claude/hooks/gate.sh"), None);
        assert_eq!(written(&invocation("\"$CMD\"")), None);
        assert_eq!(written("eval 'unterminated"), None);
    }
}
