//! What a job uses: the memory its group is charged, and its largest
//! process, named as events name a job (#24). Read only when an event is
//! written, so that watching costs nothing between events: one file per live
//! job to rank them, then a few per process of the jobs an event names.

use crate::events::{JobUsage, Process};
use claude_code::invocation::Kind;
use claude_code::messages;
use claude_code::sessions::{self, ClaudeSession};
use learning::peaks;
use prefix::JobRecord;
use std::fs;
use std::path::Path;
use system::{cgroup, procfs};

/// Where jobs are read.
pub struct Reader<'a> {
    /// The orchestrator slice, holding a scope per session.
    pub slice: &'a Path,
    /// The root of the job records.
    pub records: &'a Path,
    pub proc_root: &'a Path,
    pub sessions: &'a [ClaudeSession],
    pub home: Option<&'a Path>,
}

impl Reader<'_> {
    /// The job `name` of the session scope `scope`; None once its group is
    /// gone.
    pub fn job(&self, scope: &str, name: &str) -> Option<JobUsage> {
        let dir = self.slice.join(scope).join(name);
        let memory_mb = read_number(&dir.join("memory.current"))? / (1024 * 1024);
        let session = self.session(scope);
        Some(JobUsage {
            session: session.map(|s| messages::address(s).to_string()),
            session_id: session.map(|s| s.session_id.clone()),
            job: name.into(),
            command: self.label(scope, name),
            memory_mb,
            largest: self.largest(&dir),
        })
    }

    /// The `n` live jobs using the most memory, across sessions, most first.
    pub fn largest_jobs(&self, n: usize) -> Vec<JobUsage> {
        let mut all: Vec<(u64, String, String)> = Vec::new();
        for scope in names(self.slice).filter(|s| is_scope(s)) {
            for job in names(&self.slice.join(&scope)) {
                if prefix::parse_job_name(&job).is_none() {
                    continue;
                }
                let current = self.slice.join(&scope).join(&job).join("memory.current");
                if let Some(bytes) = read_number(&current) {
                    all.push((bytes, scope.clone(), job));
                }
            }
        }
        all.sort_by_key(|a| std::cmp::Reverse(a.0));
        all.into_iter()
            .take(n)
            .filter_map(|(_, scope, job)| self.job(&scope, &job))
            .collect()
    }

    /// The session of the scope `scope`, by its claude process.
    pub fn session(&self, scope: &str) -> Option<&ClaudeSession> {
        let pid = cgroup::main_pid(self.slice, scope)?;
        self.sessions.iter().find(|s| s.pid == pid)
    }

    /// A Bash call's label, from its job record.
    pub fn label(&self, scope: &str, name: &str) -> Option<String> {
        let (kind, _) = prefix::parse_job_name(name)?;
        if kind != Kind::Bash {
            return None;
        }
        let path = self.records.join(scope).join(format!("{name}.json"));
        let record: JobRecord = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        self.label_of(&record)
    }

    pub fn label_of(&self, record: &JobRecord) -> Option<String> {
        peaks::call_label(&record.command, Path::new(&record.cwd), self.home)
    }

    fn largest(&self, dir: &Path) -> Option<Process> {
        fs::read_to_string(dir.join("cgroup.procs"))
            .ok()?
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .filter_map(|pid| procfs::read_one(self.proc_root, pid, sessions::ID_VAR))
            .max_by_key(|p| p.rss_kb)
            .map(|p| Process {
                pid: p.pid,
                rss_mb: p.rss_kb / 1024,
                comm: p.comm,
                command: p.command_head,
            })
    }
}

/// The entries of `dir`, none when it does not exist.
fn names(dir: &Path) -> impl Iterator<Item = String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
}

fn is_scope(name: &str) -> bool {
    Path::new(name).extension().is_some_and(|e| e == "scope")
}

fn read_number(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Tree {
        _tmp: tempfile::TempDir,
        slice: PathBuf,
        records: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        Tree {
            slice: tmp.path().join("orchestrator.slice"),
            records: tmp.path().join("jobs"),
            _tmp: tmp,
        }
    }

    fn job(t: &Tree, scope: &str, name: &str, bytes: u64, pids: &[u32]) {
        let dir = t.slice.join(scope).join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("memory.current"), format!("{bytes}\n")).unwrap();
        let procs: Vec<String> = pids.iter().map(u32::to_string).collect();
        fs::write(dir.join("cgroup.procs"), procs.join("\n")).unwrap();
    }

    fn reader(t: &Tree) -> Reader<'_> {
        Reader {
            slice: &t.slice,
            records: &t.records,
            proc_root: Path::new("/proc"),
            sessions: &[],
            home: None,
        }
    }

    #[test]
    fn jobs_rank_by_the_memory_their_group_is_charged() {
        let t = tree();
        job(&t, "a.scope", "job-bash-1-1", 100 << 20, &[]);
        job(&t, "a.scope", "job-other-2-1", 900 << 20, &[]);
        job(&t, "b.scope", "job-bash-3-1", 500 << 20, &[]);
        job(&t, "b.scope", "main", 5000 << 20, &[]);
        let jobs: Vec<(String, u64)> = reader(&t)
            .largest_jobs(2)
            .into_iter()
            .map(|j| (j.job, j.memory_mb))
            .collect();
        assert_eq!(
            jobs,
            [("job-other-2-1".into(), 900), ("job-bash-3-1".into(), 500)]
        );
    }

    #[test]
    fn a_job_names_its_largest_process_and_its_call() {
        let t = tree();
        let me = std::process::id();
        job(&t, "a.scope", "job-bash-1-1", 1 << 20, &[me]);
        let records = t.records.join("a.scope");
        fs::create_dir_all(&records).unwrap();
        let record = JobRecord {
            command: "eval 'make test'".into(),
            cwd: "/".into(),
        };
        fs::write(
            records.join("job-bash-1-1.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let j = reader(&t).job("a.scope", "job-bash-1-1").unwrap();
        assert_eq!(j.command.as_deref(), Some("make test"));
        assert_eq!(j.largest.map(|p| p.pid), Some(me));
        assert_eq!(j.session, None);
        assert!(reader(&t).job("a.scope", "job-bash-9-9").is_none());
    }
}
