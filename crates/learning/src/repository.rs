//! The repository a command runs in. Peaks are learned per repository, all
//! its worktrees together: they share one git common directory, which is the
//! key. Outside any repository, the directory itself is the key. Found by
//! walking up the file system, without starting git: the prefix will look a
//! command up before every Bash call.
//!
//! The packages of a monorepo share that key: `pnpm test` run in two of them
//! is one command, with one expected peak.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// The key of the repository holding `dir`: its git common directory, or
/// `dir` itself outside any repository. Symbolic links are resolved where the
/// path exists.
pub fn key(dir: &Path) -> PathBuf {
    let dir = normalise(dir);
    for d in dir.ancestors() {
        let dot_git = d.join(".git");
        let Ok(meta) = fs::metadata(&dot_git) else {
            continue;
        };
        if meta.is_dir() {
            return canonical(&dot_git);
        }
        // A worktree, or a malformed `.git` file: then the work tree is the key.
        return canonical(&common_dir(&dot_git).unwrap_or_else(|| d.to_path_buf()));
    }
    canonical(&dir)
}

/// The common directory a `.git` file leads to. It names the worktree's own
/// git directory (`gitdir: …`), whose `commondir` file names the common one;
/// without that file, the git directory is itself the common one (a
/// submodule, or a bare repository's main worktree).
fn common_dir(dot_git: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(dot_git).ok()?;
    let gitdir = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let gitdir = dot_git.parent()?.join(gitdir);
    match fs::read_to_string(gitdir.join("commondir")) {
        Ok(common) => Some(gitdir.join(common.trim())),
        Err(_) => Some(gitdir),
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalise(path))
}

/// `path` without `.` and `..` components, resolved by name, the way `cd`
/// resolves them.
pub fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn normalises_by_name() {
        assert_eq!(normalise(Path::new("/a/./b/../c")), Path::new("/a/c"));
        assert_eq!(normalise(Path::new("/../a")), Path::new("/a"));
    }

    #[test]
    fn a_plain_repository_is_keyed_by_its_git_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(root.join("app/.git")).unwrap();
        fs::create_dir_all(root.join("app/src/deep")).unwrap();
        assert_eq!(key(&root.join("app")), root.join("app/.git"));
        assert_eq!(key(&root.join("app/src/deep")), root.join("app/.git"));
        assert_eq!(
            key(&root.join("app/src/../src/deep")),
            root.join("app/.git")
        );
    }

    #[test]
    fn worktrees_share_their_repository_s_key() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        // `git worktree add ../wt`: an absolute gitdir, a relative commondir.
        fs::create_dir_all(root.join("main/.git/worktrees/wt")).unwrap();
        write(&root.join("main/.git/worktrees/wt/commondir"), "../..\n");
        let gitdir = root.join("main/.git/worktrees/wt");
        write(
            &root.join("wt/.git"),
            &format!("gitdir: {}\n", gitdir.display()),
        );
        fs::create_dir_all(root.join("wt/pkg")).unwrap();
        assert_eq!(key(&root.join("wt/pkg")), root.join("main/.git"));
        assert_eq!(key(&root.join("main")), root.join("main/.git"));
    }

    #[test]
    fn a_bare_repository_and_its_worktrees_share_one_key() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        // A `.bare/` directory with one worktree per branch next to it.
        fs::create_dir_all(root.join("p/.bare/worktrees/feat")).unwrap();
        write(&root.join("p/.git"), "gitdir: ./.bare\n");
        write(&root.join("p/.bare/worktrees/feat/commondir"), "../..\n");
        write(
            &root.join("p/feat/.git"),
            &format!(
                "gitdir: {}\n",
                root.join("p/.bare/worktrees/feat").display()
            ),
        );
        assert_eq!(key(&root.join("p/feat")), root.join("p/.bare"));
        assert_eq!(key(&root.join("p")), root.join("p/.bare"));
    }

    #[test]
    fn outside_a_repository_the_directory_is_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(root.join("scratch")).unwrap();
        assert_eq!(key(&root.join("scratch")), root.join("scratch"));
        // Gone meanwhile: still a key, by name.
        assert_eq!(key(&root.join("gone/../x")), root.join("x"));
    }

    #[test]
    fn a_malformed_git_file_keys_by_its_work_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        write(&root.join("odd/.git"), "not a gitfile\n");
        assert_eq!(key(&root.join("odd")), root.join("odd"));
    }
}
