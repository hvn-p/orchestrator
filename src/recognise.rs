//! Recognising a command. Claude writes the same command in many forms; the
//! peaks are learned, and looked up, per command in one form, the way Claude
//! Code matches its Bash permission rules.
//!
//! A call is split into simple commands. Wrappers, leading environment
//! assignments, redirections and `cd` are removed, `cd` still deciding the
//! directory of the commands after it. What remains is kept word for word,
//! unquoted, expansions as written: `cd app && timeout 300 pnpm exec vitest
//! run X 2>&1 | grep FAIL` yields `pnpm exec vitest run X` and `grep FAIL`.
//! Unlike a permission rule, no wildcard widens the match: `vitest run` (the
//! whole suite) and `vitest run one.test.ts` stay distinct, since they do
//! not weigh the same. The bodies of compound commands (brace
//! groups, subshells, loops, conditionals) are commands of the call too; a
//! subshell's `cd` stays inside it. A command substitution stays part of the
//! word that holds it. A here-document or here-string is the command's input:
//! like the script of `python3 -c`, it decides the work, so it stays with the
//! command.
//!
//! Shell is parsed with brush-parser. Anything it cannot parse yields nothing:
//! the call teaches nothing, and an unknown command starts at once.
//! brush-parser was chosen over tree-sitter-bash, which needs a C compiler,
//! ran at half the speed and leaves unquoting to the caller; yash-syntax,
//! which parses POSIX shell only (it rejects `[[ ]]`) through an asynchronous
//! API; conch-parser, with no release since 2019; and a parser of our own, a
//! shell grammar to maintain.

use crate::repository;
use brush_parser::ast::{
    AndOr, AndOrList, Command as ShellCommand, CommandPrefixOrSuffixItem, CompoundCommand,
    CompoundList, IoRedirect, Pipeline, Program, SeparatorOperator, SimpleCommand,
};
use brush_parser::word::{self, TildeExpr, WordPiece, WordPieceWithSource};
use brush_parser::{Parser, ParserOptions};
use std::path::{Path, PathBuf};

/// One simple command of a call, as it is recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Its words, unquoted, expansions as written.
    pub words: Vec<String>,
    /// What its here-documents and here-strings feed it.
    pub input: Option<String>,
    /// The directory it runs in. None when a `cd` before it goes where only
    /// running the call would tell (`cd "$dir"`, `cd -`).
    pub dir: Option<PathBuf>,
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

/// The simple commands of `script` run from `cwd`, in order. `home` resolves
/// `cd` alone and `cd ~`.
pub fn commands(script: &str, cwd: &Path, home: Option<&Path>) -> Option<Vec<Command>> {
    let program = parse(script)?;
    let mut walk = Walk {
        home,
        found: Vec::new(),
    };
    let mut dir = Some(cwd.to_path_buf());
    for list in &program.complete_commands {
        walk.list(list, &mut dir);
    }
    Some(walk.found)
}

// claude-code: bash-call-eval
fn options() -> ParserOptions {
    // The invocation turns extglob off before its eval.
    ParserOptions {
        enable_extended_globbing: false,
        ..ParserOptions::default()
    }
}

fn parse(script: &str) -> Option<Program> {
    Parser::new(script.as_bytes(), &options())
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

/// Walks a parsed call, collecting its simple commands.
struct Walk<'a> {
    home: Option<&'a Path>,
    found: Vec<Command>,
}

impl Walk<'_> {
    fn list(&mut self, list: &CompoundList, dir: &mut Option<PathBuf>) {
        for item in &list.0 {
            // A command started in the background runs in a subshell.
            if matches!(item.1, SeparatorOperator::Async) {
                self.and_or(&item.0, &mut dir.clone());
            } else {
                self.and_or(&item.0, dir);
            }
        }
    }

    fn and_or(&mut self, list: &AndOrList, dir: &mut Option<PathBuf>) {
        self.pipeline(&list.first, dir);
        for next in &list.additional {
            let (AndOr::And(p) | AndOr::Or(p)) = next;
            self.pipeline(p, dir);
        }
    }

    fn pipeline(&mut self, pipeline: &Pipeline, dir: &mut Option<PathBuf>) {
        match pipeline.seq.as_slice() {
            [single] => self.command(single, dir),
            // Each command of a longer pipeline runs in a subshell.
            several => {
                for command in several {
                    self.command(command, &mut dir.clone());
                }
            }
        }
    }

    fn command(&mut self, command: &ShellCommand, dir: &mut Option<PathBuf>) {
        match command {
            ShellCommand::Simple(s) => self.simple(s, dir),
            ShellCommand::Compound(c, _) => self.compound(c, dir),
            // A definition runs nothing; a test is the shell's own.
            ShellCommand::Function(_) | ShellCommand::ExtendedTest(..) => {}
        }
    }

    fn compound(&mut self, command: &CompoundCommand, dir: &mut Option<PathBuf>) {
        match command {
            CompoundCommand::BraceGroup(g) => self.list(&g.list, dir),
            CompoundCommand::Subshell(s) => self.list(&s.list, &mut dir.clone()),
            CompoundCommand::ForClause(f) => self.list(&f.body.list, dir),
            CompoundCommand::ArithmeticForClause(f) => self.list(&f.body.list, dir),
            CompoundCommand::WhileClause(w) | CompoundCommand::UntilClause(w) => {
                self.list(&w.0, dir);
                self.list(&w.1.list, dir);
            }
            CompoundCommand::IfClause(i) => {
                self.list(&i.condition, dir);
                self.list(&i.then, dir);
                for e in i.elses.iter().flatten() {
                    if let Some(condition) = &e.condition {
                        self.list(condition, dir);
                    }
                    self.list(&e.body, dir);
                }
            }
            CompoundCommand::CaseClause(c) => {
                for case in &c.cases {
                    if let Some(list) = &case.cmd {
                        self.list(list, dir);
                    }
                }
            }
            CompoundCommand::Coprocess(c) => self.command(&c.body, &mut dir.clone()),
            CompoundCommand::Arithmetic(_) => {}
        }
    }

    fn simple(&mut self, s: &SimpleCommand, dir: &mut Option<PathBuf>) {
        let mut words = Vec::new();
        let mut input: Option<String> = None;
        let prefix = s.prefix.iter().flat_map(|p| &p.0);
        let suffix = s.suffix.iter().flat_map(|p| &p.0);
        for item in prefix {
            match item {
                // Leading environment assignments.
                CommandPrefixOrSuffixItem::AssignmentWord(..) => {}
                other => add_item(other, &mut words, &mut input),
            }
        }
        if let Some(name) = &s.word_or_name {
            words.push(unquote(&name.value));
        }
        for item in suffix {
            add_item(item, &mut words, &mut input);
        }
        let words = &words[after_wrappers(&words)..];
        match words.first().map(|w| w.text.as_str()) {
            None => {}
            Some("cd") => *dir = cd(dir.take(), &words[1..], self.home),
            Some(_) => self.found.push(Command {
                words: words.iter().map(|w| w.text.clone()).collect(),
                input,
                dir: dir.clone(),
            }),
        }
    }
}

/// Adds an argument to `words`, or the content of a here-document or
/// here-string to `input`. Any other redirection is dropped.
fn add_item(
    item: &CommandPrefixOrSuffixItem,
    words: &mut Vec<Unquoted>,
    input: &mut Option<String>,
) {
    match item {
        CommandPrefixOrSuffixItem::Word(w) | CommandPrefixOrSuffixItem::AssignmentWord(_, w) => {
            words.push(unquote(&w.value));
        }
        CommandPrefixOrSuffixItem::ProcessSubstitution(kind, subshell) => words.push(Unquoted {
            text: format!("{kind}{subshell}"),
            literal: false,
            home: false,
        }),
        CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
            let fed = match redirect {
                IoRedirect::HereDocument(_, doc) => doc.doc.value.clone(),
                IoRedirect::HereString(_, w) => unquote(&w.value).text,
                IoRedirect::File(..) | IoRedirect::OutputAndError(..) => return,
            };
            input.get_or_insert_with(String::new).push_str(&fed);
        }
    }
}

/// A word without its quotes. Expansions stay as written: nothing is run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Unquoted {
    text: String,
    /// The text is the word's value: it holds no expansion.
    literal: bool,
    /// The word starts with a `~` standing for the home directory.
    home: bool,
}

fn unquote(word: &str) -> Unquoted {
    let Ok(pieces) = word::parse(word, &options()) else {
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

/// How many leading words are wrappers that do not change the work:
/// `timeout`, `time`, `nice`, `nohup`, `stdbuf`, `command` and `builtin`, with
/// their options (and `timeout`'s duration).
fn after_wrappers(words: &[Unquoted]) -> usize {
    let mut start = 0;
    while let Some(first) = words.get(start) {
        let rest = &words[start + 1..];
        let skipped = match first.text.as_str() {
            "timeout" => {
                let options = after_options(rest, "ks", &["kill-after", "signal"]);
                (options < rest.len()).then_some(options + 1)
            }
            "time" => Some(after_options(rest, "fo", &["format", "output"])),
            "nice" => Some(after_options(rest, "n", &["adjustment"])),
            "nohup" => Some(after_options(rest, "", &[])),
            "stdbuf" => Some(after_options(rest, "ioe", &["input", "output", "error"])),
            // `command -v` looks a command up rather than running it.
            "command"
                if rest.first().is_none_or(|w| {
                    !w.text.starts_with('-') || w.text == "-p" || w.text == "--"
                }) =>
            {
                Some(after_options(rest, "", &[]))
            }
            "builtin" => Some(0),
            _ => None,
        };
        let Some(skipped) = skipped else { break };
        start += 1 + skipped;
    }
    start.min(words.len())
}

/// How many leading words of `args` are options. `short` lists the short
/// options that take a value, `long` the long ones; a long option may be
/// abbreviated, as getopt allows.
fn after_options(args: &[Unquoted], short: &str, long: &[&str]) -> usize {
    let mut i = 0;
    while let Some(arg) = args.get(i).map(|a| a.text.as_str()) {
        i += 1;
        if arg == "--" {
            break;
        }
        if let Some(name) = arg.strip_prefix("--") {
            if !name.contains('=') && long.iter().any(|l| l.starts_with(name)) {
                i += 1;
            }
        } else if let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.is_empty()) {
            // The first option taking a value takes the rest of the cluster,
            // else the next word.
            if cluster.find(|c| short.contains(c)) == Some(cluster.len() - 1) {
                i += 1;
            }
        } else {
            i -= 1;
            break;
        }
    }
    i.min(args.len())
}

/// The directory after `cd args` run from `dir`.
fn cd(dir: Option<PathBuf>, args: &[Unquoted], home: Option<&Path>) -> Option<PathBuf> {
    let Some(target) = args.get(after_options(args, "", &[])) else {
        return home.map(Path::to_path_buf);
    };
    let path = if target.home {
        let rest = target.text.trim_start_matches('~').trim_start_matches('/');
        home?.join(rest)
    } else if target.literal && target.text != "-" {
        PathBuf::from(&target.text)
    } else {
        return None;
    };
    let joined = if path.is_absolute() {
        path
    } else {
        dir?.join(path)
    };
    Some(repository::normalise(&joined))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Bash call as Claude Code 2.1.289 hands it to the prefix.
    fn invocation(eval: &str) -> String {
        format!(
            "source /home/u/.claude/shell-snapshots/snapshot-bash-1-x.sh 2>/dev/null || true && {{ shopt -u extglob || setopt NO_EXTENDED_GLOB NO_BARE_GLOB_QUAL; }} >/dev/null 2>&1 || true && {{ \\builtin unalias -- 'unsetenv'; \\builtin unset -f -- 'unsetenv'; }} >/dev/null 2>&1 || true && eval {eval} && pwd -P >| /tmp/claude-ab12-cwd"
        )
    }

    fn words(script: &str) -> Vec<Vec<String>> {
        commands(script, Path::new("/repo"), Some(Path::new("/home/u")))
            .unwrap()
            .into_iter()
            .map(|c| c.words)
            .collect()
    }

    fn w(command: &str) -> Vec<String> {
        command.split(' ').map(str::to_string).collect()
    }

    fn ws(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
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

    #[test]
    fn splits_at_every_operator() {
        assert_eq!(
            words("a 1 && b 2 || c 3; d 4 | e 5 |& f 6 & g 7\nh 8"),
            ["a 1", "b 2", "c 3", "d 4", "e 5", "f 6", "g 7", "h 8"].map(w)
        );
    }

    #[test]
    fn the_documented_example() {
        let found = commands(
            "cd app && timeout 300 pnpm exec vitest run X 2>&1 | grep FAIL",
            Path::new("/repo"),
            None,
        )
        .unwrap();
        assert_eq!(
            found,
            vec![
                Command {
                    words: w("pnpm exec vitest run X"),
                    input: None,
                    dir: Some("/repo/app".into()),
                },
                Command {
                    words: w("grep FAIL"),
                    input: None,
                    dir: Some("/repo/app".into()),
                },
            ]
        );
    }

    #[test]
    fn removes_redirections_and_leading_assignments() {
        assert_eq!(
            words(r#"nr lint 2>&1 | tail -1; nr typecheck >/dev/null 2>&1; echo "exit $?""#),
            [
                w("nr lint"),
                w("tail -1"),
                w("nr typecheck"),
                ws(&["echo", "exit $?"])
            ]
        );
        assert_eq!(words("NODE_ENV=test npm test"), [w("npm test")]);
        assert_eq!(
            words("A=1 B='x y' >out.log make all &>/dev/null"),
            [w("make all")]
        );
        assert_eq!(words("FOO=1"), Vec::<Vec<String>>::new());
        // An assignment after the program is one of its arguments.
        assert_eq!(words("make CC=clang"), [w("make CC=clang")]);
    }

    #[test]
    fn removes_wrappers_with_their_options() {
        let same = w("pnpm test");
        for form in [
            "timeout 300 pnpm test",
            "timeout -k 5 -s KILL 60 pnpm test",
            "timeout --signal=TERM --kill-after 5 60 pnpm test",
            "time pnpm test",
            "time -p pnpm test",
            "nice -n 10 pnpm test",
            "nice -10 pnpm test",
            "nohup pnpm test",
            "stdbuf -oL -e 0 pnpm test",
            "command pnpm test",
            "builtin pnpm test",
            "timeout 60 nice nohup pnpm test",
        ] {
            assert_eq!(words(form), vec![same.clone()], "{form}");
        }
        assert_eq!(words("command -v git"), [w("command -v git")]);
        assert_eq!(words("timeout 60"), Vec::<Vec<String>>::new());
    }

    #[test]
    fn same_command_in_other_quotes() {
        assert_eq!(words("grep 'FAIL' \"src\""), [w("grep FAIL src")]);
        assert_eq!(words(r"echo a\ b"), [ws(&["echo", "a b"])]);
    }

    #[test]
    fn keeps_the_command_word_for_word() {
        assert_ne!(words("vitest run"), words("vitest run one.test.ts"));
        assert_eq!(
            words("git log --format=%h $(git merge-base HEAD main)..HEAD"),
            [ws(&[
                "git",
                "log",
                "--format=%h",
                "$(git merge-base HEAD main)..HEAD"
            ])]
        );
    }

    #[test]
    fn cd_decides_the_directory_of_what_follows() {
        let dirs = |script: &str| -> Vec<Option<PathBuf>> {
            commands(script, Path::new("/repo"), Some(Path::new("/home/u")))
                .unwrap()
                .into_iter()
                .map(|c| c.dir)
                .collect()
        };
        assert_eq!(
            dirs("cd app && make; cd .. && make; cd /tmp/x && make"),
            [
                Some("/repo/app".into()),
                Some("/repo".into()),
                Some("/tmp/x".into())
            ]
        );
        assert_eq!(
            dirs("cd && a; cd ~/p && b; cd -P -- sub && c"),
            [
                Some("/home/u".into()),
                Some("/home/u/p".into()),
                Some("/home/u/p/sub".into())
            ]
        );
        assert_eq!(
            dirs("cd \"$DIR\" && a; cd /x && b"),
            [None, Some("/x".into())]
        );
        assert_eq!(dirs("cd - && a"), [None]);
        // A subshell, a pipeline or the background keeps its cd to itself.
        assert_eq!(
            dirs("(cd sub && a); b"),
            [Some("/repo/sub".into()), Some("/repo".into())]
        );
        assert_eq!(
            dirs("cd sub | a; b"),
            [Some("/repo".into()), Some("/repo".into())]
        );
        assert_eq!(dirs("cd sub & a"), [Some("/repo".into())]);
        assert_eq!(
            dirs("{ cd sub; a; }; b"),
            [Some("/repo/sub".into()), Some("/repo/sub".into())]
        );
    }

    #[test]
    fn walks_into_compound_commands() {
        assert_eq!(
            words(
                "for f in a b; do pytest \"$f\"; done; while ! curl -s localhost; do sleep 1; done"
            ),
            [w("pytest $f"), w("curl -s localhost"), w("sleep 1")]
        );
        assert_eq!(
            words("if [[ -f x ]]; then make; elif test -d y; then a; else b; fi"),
            [w("make"), w("test -d y"), w("a"), w("b")]
        );
        assert_eq!(
            words("case $x in a) one;; *) two;; esac"),
            [w("one"), w("two")]
        );
        assert_eq!(
            words("f() { heavy; }; (( i++ ))"),
            Vec::<Vec<String>>::new()
        );
    }

    #[test]
    fn a_heredoc_is_the_command_s_input() {
        let found = commands(
            "python3 - <<'PY'\nimport os\nprint(os.getcwd())\nPY\npython3 - <<'PY' 2>&1 | tail -2\nprint(1)\nPY",
            Path::new("/repo"),
            None,
        )
        .unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].words, w("python3 -"));
        assert_eq!(found[1].words, w("python3 -"));
        assert_ne!(found[0].input, found[1].input);
        assert!(
            found[0]
                .input
                .as_deref()
                .is_some_and(|i| i.contains("getcwd"))
        );
        assert_eq!(found[2].words, w("tail -2"));
        assert_eq!(found[2].input, None);
        let here = commands("wc -w <<< \"a b\"", Path::new("/repo"), None).unwrap();
        assert_eq!(here[0].input.as_deref(), Some("a b"));
    }

    #[test]
    fn real_calls_end_to_end() {
        let call = invocation(
            r"'cd app && timeout 300 pnpm exec vitest run X 2>&1 | grep FAIL' < /dev/null",
        );
        let script = written(&call).unwrap();
        assert_eq!(
            words(&script),
            [w("pnpm exec vitest run X"), w("grep FAIL")]
        );
        // Two forms of one command.
        let alone = written(&invocation(
            r#"'python3 -c '"'"'b = bytearray(300 << 20)'"'"''"#,
        ))
        .unwrap();
        let piped = written(&invocation(
            r#"'timeout 60 python3 -c '"'"'b = bytearray(300 << 20)'"'"' 2>&1 | tail -1'"#,
        ))
        .unwrap();
        assert_eq!(words(&alone)[0], words(&piped)[0]);
    }

    #[test]
    fn unparsable_scripts_yield_nothing() {
        assert_eq!(commands("echo 'open", Path::new("/r"), None), None);
        assert_eq!(commands("if then fi", Path::new("/r"), None), None);
    }
}
