//! The cgroup v2 tree of an orchestrated session. Each session is a delegated
//! systemd user scope in the orchestrator slice: claude runs in its `main/`
//! leaf, and every command Claude Code starts runs in a `job-*` leaf of its
//! own. Paths are relative to the cgroup root, which tests point at a
//! temporary directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where the kernel mounts the cgroup v2 hierarchy.
pub const ROOT: &str = "/sys/fs/cgroup";
/// The systemd slice every orchestrated session is started in.
pub const SLICE: &str = "orchestrator.slice";
/// Controllers handed to the leaves of a session, those the session has.
const CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];

/// The cgroup v2 path of a `/proc/<pid>/cgroup` file: its `0::` line.
pub fn own_path(proc_cgroup: &str) -> Option<&str> {
    proc_cgroup.lines().find_map(|l| l.strip_prefix("0::"))
}

/// The session a cgroup path belongs to: the path down to the scope right
/// under the orchestrator slice. None outside an orchestrated session.
pub fn session_of(path: &str) -> Option<&str> {
    let marker = format!("/{SLICE}/");
    let start = path.find(&marker)? + marker.len();
    let end = path
        .get(start..)?
        .find('/')
        .map_or(path.len(), |i| start + i);
    path.get(..end).filter(|_| end > start)
}

/// The orchestrator slice's path, from the path of any cgroup of the same user
/// manager: the slice sits right under `user@<uid>.service`.
pub fn slice_path(path: &str) -> Option<String> {
    let mut prefix = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        prefix.push('/');
        prefix.push_str(part);
        if part.starts_with("user@") && part.ends_with(".service") {
            return Some(format!("{prefix}/{SLICE}"));
        }
    }
    None
}

/// Moves `pid`, the only process of a new session scope, into `main/`, then
/// hands the controllers to the scope's leaves. A cgroup that hands controllers
/// to its children may not hold processes itself, hence the order.
pub fn set_up_session(root: &Path, session: &str, pid: u32) -> io::Result<()> {
    let dir = dir(root, session);
    let main = dir.join("main");
    fs::create_dir(&main)?;
    fs::write(main.join("cgroup.procs"), pid.to_string())?;
    let available = fs::read_to_string(dir.join("cgroup.controllers"))?;
    let enable: Vec<String> = CONTROLLERS
        .iter()
        .filter(|c| available.split_whitespace().any(|a| a == **c))
        .map(|c| format!("+{c}"))
        .collect();
    if enable.is_empty() {
        return Ok(());
    }
    fs::write(dir.join("cgroup.subtree_control"), enable.join(" "))
}

/// Creates the job leaf `name` in `session` and moves `pid` into it.
pub fn enter_job(root: &Path, session: &str, name: &str, pid: u32) -> io::Result<PathBuf> {
    let job = dir(root, session).join(name);
    fs::create_dir(&job)?;
    fs::write(job.join("cgroup.procs"), pid.to_string())?;
    Ok(job)
}

fn dir(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str =
        "/user.slice/user-1000.slice/user@1000.service/orchestrator.slice/orchestrator-9-9.scope";

    #[test]
    fn reads_the_v2_line() {
        let text = "1:name=systemd:/x\n0::/a/b.scope\n";
        assert_eq!(own_path(text), Some("/a/b.scope"));
        assert_eq!(own_path("1:name=systemd:/x\n"), None);
    }

    #[test]
    fn finds_the_session_from_any_of_its_leaves() {
        assert_eq!(session_of(SESSION), Some(SESSION));
        assert_eq!(session_of(&format!("{SESSION}/main")), Some(SESSION));
        assert_eq!(
            session_of(&format!("{SESSION}/job-bash-7-1")),
            Some(SESSION)
        );
        assert_eq!(
            session_of("/user.slice/user-1000.slice/user@1000.service/app.slice/app-x.scope"),
            None
        );
        assert_eq!(session_of("/x/orchestrator.slice/"), None);
    }

    #[test]
    fn finds_the_slice_under_the_user_manager() {
        assert_eq!(
            slice_path("/user.slice/user-1000.slice/user@1000.service/app.slice/x.service"),
            Some("/user.slice/user-1000.slice/user@1000.service/orchestrator.slice".into())
        );
        assert_eq!(slice_path("/system.slice/cron.service"), None);
    }

    #[test]
    fn sets_up_a_session_with_the_controllers_it_has() {
        let root = tempfile::tempdir().unwrap();
        let dir = dir(root.path(), SESSION);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cgroup.controllers"), "cpuset cpu io memory\n").unwrap();
        set_up_session(root.path(), SESSION, 42).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("main/cgroup.procs")).unwrap(),
            "42"
        );
        assert_eq!(
            fs::read_to_string(dir.join("cgroup.subtree_control")).unwrap(),
            "+cpu +memory"
        );
    }

    #[test]
    fn a_job_gets_its_own_leaf() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir(root.path(), SESSION)).unwrap();
        let job = enter_job(root.path(), SESSION, "job-bash-7-1", 7).unwrap();
        assert_eq!(job, dir(root.path(), SESSION).join("job-bash-7-1"));
        assert_eq!(fs::read_to_string(job.join("cgroup.procs")).unwrap(), "7");
        assert!(enter_job(root.path(), SESSION, "job-bash-7-1", 7).is_err());
    }
}
