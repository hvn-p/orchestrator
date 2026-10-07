//! `scripts/guide-guard.sh`, the merge guard of the guide: a pull request that
//! changes a file outside `plugins/orchestrator/` updates the guide too, or
//! its description holds a line `Guide: unchanged, <reason>`. Each case runs
//! the script on a scratch repository.

// Clippy exempts only `#[test]` functions; every function here is test code.
#![allow(clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

const GUARD: &str = "scripts/guide-guard.sh";
const SKILL: &str = "plugins/orchestrator/skills/guide/SKILL.md";

/// A scratch repository with one commit, `base`, holding some code and the
/// guide.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Repo {
        let repo = Repo {
            dir: tempfile::tempdir().expect("creating a scratch directory"),
        };
        repo.git(&["init", "--quiet", "--initial-branch=main"]);
        repo.commit(&[("src/main.rs", "fn main() {}\n"), (SKILL, "guide\n")]);
        repo.git(&["tag", "base"]);
        repo
    }

    /// Runs git there, isolated from the user's and the system's
    /// configuration, and returns its output.
    fn git(&self, args: &[&str]) -> String {
        let out = isolated(Command::new("git"))
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .expect("running git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Writes `files` and commits them.
    fn commit(&self, files: &[(&str, &str)]) {
        for (path, text) in files {
            let path = self.dir.path().join(path);
            fs::create_dir_all(path.parent().expect("a file has a directory"))
                .expect("creating a directory");
            fs::write(path, text).expect("writing a file");
        }
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "--message", "change"]);
    }

    /// The guard's exit code and output for `base..HEAD` with `description`.
    fn guard(&self, description: &str) -> (i32, String) {
        let body = self.dir.path().join(".git/description.md");
        fs::write(&body, description).expect("writing the description");
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join(GUARD);
        let out = isolated(Command::new("bash"))
            .arg(script)
            .args(["base", "HEAD"])
            .arg(&body)
            .current_dir(self.dir.path())
            .output()
            .expect("running the guard");
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        (out.status.code().expect("the guard exited"), text)
    }
}

fn isolated(mut cmd: Command) -> Command {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    cmd
}

#[test]
fn a_change_with_the_guide_passes() {
    let repo = Repo::new();
    repo.commit(&[("src/main.rs", "fn main() { run() }\n"), (SKILL, "new\n")]);
    let (code, out) = repo.guard("");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("The guide is updated."), "{out}");
}

#[test]
fn a_change_to_the_guide_alone_passes() {
    let repo = Repo::new();
    repo.commit(&[("plugins/orchestrator/skills/guide/reference/new.md", "x\n")]);
    let (code, out) = repo.guard("");
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_statement_with_a_reason_passes() {
    let repo = Repo::new();
    repo.commit(&[("src/main.rs", "fn main() { run() }\n")]);
    let description = "Refactor.\r\n\r\nGuide: unchanged, nothing a user sees changes.\r\n";
    let (code, out) = repo.guard(description);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("nothing a user sees changes."), "{out}");
}

#[test]
fn a_statement_without_a_reason_fails() {
    let repo = Repo::new();
    repo.commit(&[("src/main.rs", "fn main() { run() }\n")]);
    for description in [
        "Guide: unchanged",
        "Guide: unchanged,",
        "Guide: unchanged,   \n",
    ] {
        let (code, out) = repo.guard(description);
        assert_eq!(code, 1, "{description:?}: {out}");
        assert!(out.contains("without giving a reason"), "{out}");
        assert!(
            out.contains("Guide: unchanged, <why the guide stays true>"),
            "{out}"
        );
    }
}

#[test]
fn a_change_without_guide_nor_statement_fails_saying_what_to_do() {
    let repo = Repo::new();
    repo.commit(&[
        ("src/main.rs", "fn main() { run() }\n"),
        ("README.md", "x\n"),
    ]);
    let (code, out) = repo.guard("Refactor.\n\nThe guide is fine.\n");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("  README.md\n  src/main.rs\n"), "{out}");
    assert!(
        out.contains("Guide: unchanged, <why the guide stays true>"),
        "{out}"
    );
    assert!(out.contains("Editing the description runs this check again."));
}

#[test]
fn no_change_passes() {
    let repo = Repo::new();
    let (code, out) = repo.guard("");
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_missing_description_file_is_an_error() {
    let repo = Repo::new();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join(GUARD);
    let status = isolated(Command::new("bash"))
        .arg(script)
        .args(["base", "HEAD", "/nonexistent/description.md"])
        .current_dir(repo.dir.path())
        .output()
        .expect("running the guard")
        .status;
    assert_eq!(status.code(), Some(2));
}
