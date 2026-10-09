//! The user's instructions for coordinators: a CLAUDE.md of their own, at
//! `$XDG_CONFIG_HOME/orchestrator/CLAUDE.md`, read by every coordinator and
//! by no other session. The file may be a symlink, which is followed, and
//! it may import others with `@path` lines, as Claude Code's own CLAUDE.md
//! does.
//!
//! orchestrator resolves the imports itself and appends the result to the
//! coordinator's system prompt, rather than relying on Claude Code's loading
//! of CLAUDE.md: an import outside the working directory needs an approval
//! a headless run cannot give. The rules follow Claude Code's: an import is
//! `@` and a path starting a word, outside code spans and fenced blocks,
//! with `\ ` for a space; an absolute path, `~/` for the home directory, or
//! a path relative to the importing file, symlinks resolved; at most
//! `MAX_DEPTH` hops. Each file is included verbatim, once, after the file
//! importing it. A cycle, a missing file or a file too large is left out,
//! and said so where it would have been; nothing of the content is logged.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Hops of imports followed, as Claude Code does.
pub const MAX_DEPTH: usize = 4;
/// The largest file included, in bytes.
pub const MAX_FILE: u64 = 128 * 1024;
/// The most included in all, in bytes: what a coordinator reads at every
/// wake.
pub const MAX_TOTAL: usize = 256 * 1024;

/// The instructions as a coordinator reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instructions {
    /// Each file in turn, the first one being the user's file, under a line
    /// naming it.
    pub text: String,
    /// The files included, as written or imported: where an instruction
    /// lives, for a coordinator asked to change it.
    pub files: Vec<PathBuf>,
}

/// The instructions at `path`, None when there is no such file. `home`
/// resolves `~/`.
pub fn load(path: &Path, home: Option<&Path>) -> Option<Instructions> {
    if !path.exists() {
        return None;
    }
    let mut loader = Loader {
        home,
        seen: HashSet::new(),
        text: String::new(),
        files: Vec::new(),
    };
    loader.include(path, 0);
    Some(Instructions {
        text: loader.text,
        files: loader.files,
    })
}

struct Loader<'a> {
    home: Option<&'a Path>,
    /// The real paths included, to include each once and stop cycles.
    seen: HashSet<PathBuf>,
    text: String,
    files: Vec<PathBuf>,
}

impl Loader<'_> {
    fn include(&mut self, path: &Path, depth: usize) {
        let shown = path.display().to_string();
        let Ok(real) = fs::canonicalize(path) else {
            let _ = writeln!(self.text, "({shown} is not there: left out.)\n");
            return;
        };
        if !self.seen.insert(real.clone()) {
            // Included already, or a cycle: once is enough.
            return;
        }
        let size = fs::metadata(&real).map_or(u64::MAX, |m| m.len());
        let room = MAX_TOTAL.saturating_sub(self.text.len());
        if size > MAX_FILE || usize::try_from(size).map_or(true, |s| s > room) {
            let _ = writeln!(
                self.text,
                "({shown} is left out: {size} bytes, more than the {MAX_FILE} a file or the {MAX_TOTAL} in all that coordinators read.)\n"
            );
            return;
        }
        let Ok(content) = fs::read_to_string(&real) else {
            let _ = writeln!(self.text, "({shown} cannot be read as text: left out.)\n");
            return;
        };
        // The user's file goes by the name the user knows; an import by
        // where it lives.
        let named = if depth == 0 {
            path.to_path_buf()
        } else {
            real.clone()
        };
        let _ = writeln!(
            self.text,
            "Contents of {}:\n\n{}\n",
            named.display(),
            content.trim_end()
        );
        self.files.push(named);
        if depth >= MAX_DEPTH {
            return;
        }
        let dir = real.parent().map(Path::to_path_buf).unwrap_or_default();
        for import in imports(&content) {
            if let Some(target) = resolve(&import, &dir, self.home) {
                self.include(&target, depth + 1);
            }
        }
    }
}

/// Where an import points: `~/` from `home`, an absolute path as is, else
/// from `dir`, the importing file's directory.
fn resolve(import: &str, dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = import.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    let path = Path::new(import);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.join(path)
    })
}

/// The paths a text imports, in order: `@` and a path starting a word,
/// outside fenced blocks and code spans, `\ ` standing for a space.
pub fn imports(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // Even pieces between backticks are prose, odd ones code.
        for (i, prose) in line.split('`').enumerate() {
            if i % 2 == 0 {
                found.extend(words_imported(prose));
            }
        }
    }
    found
}

fn words_imported(prose: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut chars = prose.chars().peekable();
    let mut at_word_start = true;
    while let Some(c) = chars.next() {
        if c == '@' && at_word_start {
            let mut path = String::new();
            while let Some(&next) = chars.peek() {
                if next == '\\' {
                    chars.next();
                    if chars.peek() == Some(&' ') {
                        path.push(' ');
                        chars.next();
                    } else {
                        path.push('\\');
                    }
                } else if next.is_whitespace() {
                    break;
                } else {
                    path.push(next);
                    chars.next();
                }
            }
            if !path.is_empty() {
                found.push(path);
            }
            at_word_start = false;
        } else {
            at_word_start = c.is_whitespace();
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn finds_imports_as_claude_code_does() {
        let text = "See @README and @docs/a.md.\n@/abs/b.md\n@~/c.md\nmail me@example.com\n`@not/this` but @this\n```\n@fenced\n```\n@with\\ space.md";
        assert_eq!(
            imports(text),
            [
                "README",
                "docs/a.md.",
                "/abs/b.md",
                "~/c.md",
                "this",
                "with space.md"
            ]
        );
    }

    struct Tree {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for dir in ["config", "dot1", "dot2", "home"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        Tree { _tmp: tmp, root }
    }

    #[test]
    fn follows_a_symlink_and_imports_relative_to_the_real_file() {
        let t = tree();
        let r = &t.root;
        fs::write(
            r.join("dot1/coordinator.md"),
            "Note in haiku.\n@../dot2/priorities.md\n@~/extra.md\n",
        )
        .unwrap();
        fs::write(r.join("dot2/priorities.md"), "Experiments can wait.\n").unwrap();
        fs::write(r.join("home/extra.md"), "Be brief.\n").unwrap();
        let link = r.join("config/CLAUDE.md");
        symlink(r.join("dot1/coordinator.md"), &link).unwrap();
        let got = load(&link, Some(&r.join("home"))).unwrap();
        assert_eq!(
            got.files,
            [
                link.clone(),
                r.join("dot2/priorities.md"),
                r.join("home/extra.md")
            ]
        );
        let first = got.text.find("Note in haiku.").unwrap();
        let second = got.text.find("Experiments can wait.").unwrap();
        let third = got.text.find("Be brief.").unwrap();
        assert!(first < second && second < third, "{}", got.text);
        assert!(got.text.starts_with(&format!(
            "Contents of {}:\n\nNote in haiku.\n@../dot2",
            link.display()
        )));
    }

    #[test]
    fn a_cycle_or_a_repeat_is_included_once() {
        let t = tree();
        let r = &t.root;
        fs::write(r.join("dot1/a.md"), "A\n@b.md\n@b.md\n").unwrap();
        fs::write(r.join("dot1/b.md"), "B\n@a.md\n").unwrap();
        let got = load(&r.join("dot1/a.md"), None).unwrap();
        assert_eq!(got.files.len(), 2);
        assert_eq!(got.text.matches("Contents of").count(), 2);
    }

    #[test]
    fn imports_stop_after_four_hops() {
        let t = tree();
        let r = &t.root;
        for i in 0..7 {
            fs::write(
                r.join(format!("dot1/{i}.md")),
                format!("level {i}\n@{}.md\n", i + 1),
            )
            .unwrap();
        }
        let got = load(&r.join("dot1/0.md"), None).unwrap();
        assert_eq!(got.files.len(), MAX_DEPTH + 1);
        assert!(got.text.contains("level 4") && !got.text.contains("level 5"));
    }

    #[test]
    fn missing_and_oversized_files_are_left_out_and_said_so() {
        let t = tree();
        let r = &t.root;
        let big = "x".repeat(usize::try_from(MAX_FILE).unwrap() + 1);
        fs::write(r.join("dot1/big.md"), &big).unwrap();
        fs::write(r.join("dot1/main.md"), "Main\n@gone.md\n@big.md\n").unwrap();
        let got = load(&r.join("dot1/main.md"), None).unwrap();
        assert_eq!(got.files, [r.join("dot1/main.md")]);
        assert!(
            got.text.contains("gone.md is not there: left out."),
            "{}",
            got.text
        );
        assert!(got.text.contains("big.md is left out"), "{}", got.text);
        assert!(!got.text.contains("xxxx"));
        assert_eq!(load(&r.join("config/CLAUDE.md"), None), None);
    }
}
