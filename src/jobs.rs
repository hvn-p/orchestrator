//! Finished jobs. Once every process of a job has exited, its group still
//! holds the job's memory peak. The service reads it, keeps it with the
//! command of a Bash call, then removes the group. systemd removes every group
//! of a session when the session ends, so peaks are read while it lives.

use crate::prefix::{self, JobRecord, Kind};
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, DirEntry};
use std::io::{self, ErrorKind};
use std::path::Path;

/// A job younger than this is left alone: between creating its group and
/// moving into it, the prefix leaves the group empty for an instant.
const MIN_AGE_MS: u128 = 5_000;

/// One finished Bash call.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Measurement {
    pub at: u64,
    /// The session's scope.
    pub session: String,
    pub job: String,
    pub peak_mb: u64,
    /// The whole invocation Claude Code assembled.
    pub command: String,
    pub cwd: String,
}

/// Measures and removes the finished jobs of every session in `slice`, then
/// drops the records of sessions that are gone. `records` is the root of the
/// job records; `remove` deletes a job group (`fs::remove_dir` on cgroupfs).
/// A job whose group cannot be removed stays for the next pass, record
/// included, so it is measured once.
pub fn collect(
    slice: &Path,
    records: &Path,
    now_ms: u128,
    remove: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<Vec<Measurement>> {
    let mut measured = Vec::new();
    let mut live = HashSet::new();
    for scope in entries(slice)? {
        let session = scope.file_name().to_string_lossy().into_owned();
        if Path::new(&session).extension().is_none_or(|e| e != "scope") {
            continue;
        }
        for job in entries(&scope.path())? {
            let name = job.file_name().to_string_lossy().into_owned();
            let Some((kind, started)) = prefix::parse_job_name(&name) else {
                continue;
            };
            let dir = job.path();
            if now_ms.saturating_sub(started) < MIN_AGE_MS || !finished(&dir) {
                continue;
            }
            let peak = read_peak_mb(&dir);
            let record_path = records.join(&session).join(format!("{name}.json"));
            let record = (kind == Kind::Bash)
                .then(|| read_record(&record_path))
                .flatten();
            if remove(&dir).is_err() {
                continue;
            }
            let _ = fs::remove_file(&record_path);
            if let (Some(record), Some(peak_mb)) = (record, peak) {
                measured.push(Measurement {
                    at: u64::try_from(now_ms / 1000).unwrap_or(u64::MAX),
                    session: session.clone(),
                    job: name,
                    peak_mb,
                    command: record.command,
                    cwd: record.cwd,
                });
            }
        }
        live.insert(session);
    }
    for dir in entries(records)? {
        if !live.contains(&*dir.file_name().to_string_lossy()) {
            let _ = fs::remove_dir_all(dir.path());
        }
    }
    Ok(measured)
}

/// The entries of `dir`, none when it does not exist (yet, or any more).
fn entries(dir: &Path) -> io::Result<Vec<DirEntry>> {
    match fs::read_dir(dir) {
        Ok(rd) => Ok(rd.flatten().collect()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn finished(job: &Path) -> bool {
    fs::read_to_string(job.join("cgroup.events"))
        .is_ok_and(|e| e.lines().any(|l| l == "populated 0"))
}

fn read_peak_mb(job: &Path) -> Option<u64> {
    let bytes: u64 = fs::read_to_string(job.join("memory.peak"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(bytes / (1024 * 1024))
}

fn read_record(path: &Path) -> Option<JobRecord> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const NOW: u128 = 100_000;

    struct Tree {
        _tmp: tempfile::TempDir,
        slice: PathBuf,
        records: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        let slice = tmp.path().join("orchestrator.slice");
        let records = tmp.path().join("jobs");
        fs::create_dir_all(&slice).unwrap();
        Tree {
            _tmp: tmp,
            slice,
            records,
        }
    }

    fn job(t: &Tree, scope: &str, name: &str, populated: bool, peak_bytes: u64) -> PathBuf {
        let dir = t.slice.join(scope).join(name);
        fs::create_dir_all(&dir).unwrap();
        let events = format!("populated {}\nfrozen 0\n", u8::from(populated));
        fs::write(dir.join("cgroup.events"), events).unwrap();
        fs::write(dir.join("memory.peak"), format!("{peak_bytes}\n")).unwrap();
        dir
    }

    fn record(t: &Tree, scope: &str, name: &str, command: &str) -> PathBuf {
        let dir = t.records.join(scope);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.json"));
        let rec = JobRecord {
            command: command.into(),
            cwd: "/repo".into(),
        };
        fs::write(&path, serde_json::to_vec(&rec).unwrap()).unwrap();
        path
    }

    fn collect_all(t: &Tree) -> Vec<Measurement> {
        collect(&t.slice, &t.records, NOW, |p| fs::remove_dir_all(p)).unwrap()
    }

    #[test]
    fn a_finished_bash_job_is_measured_once_then_removed() {
        let t = tree();
        let dir = job(&t, "s.scope", "job-bash-7-1000", false, 3 << 30);
        let rec = record(&t, "s.scope", "job-bash-7-1000", "eval 'pnpm typecheck'");
        let measured = collect_all(&t);
        assert_eq!(measured.len(), 1);
        assert_eq!(measured[0].peak_mb, 3072);
        assert_eq!(measured[0].session, "s.scope");
        assert_eq!(measured[0].command, "eval 'pnpm typecheck'");
        assert!(!dir.exists());
        assert!(!rec.exists());
        assert_eq!(collect_all(&t), Vec::new());
    }

    #[test]
    fn running_young_and_foreign_groups_stay() {
        let t = tree();
        let running = job(&t, "s.scope", "job-bash-7-1000", true, 1 << 20);
        let young = job(&t, "s.scope", "job-bash-8-99000", false, 1 << 20);
        let main = job(&t, "s.scope", "main", false, 1 << 20);
        assert_eq!(collect_all(&t), Vec::new());
        assert!(running.exists() && young.exists() && main.exists());
    }

    #[test]
    fn other_jobs_are_removed_without_a_measurement() {
        let t = tree();
        let dir = job(&t, "s.scope", "job-other-7-1000", false, 1 << 20);
        assert_eq!(collect_all(&t), Vec::new());
        assert!(!dir.exists());
    }

    #[test]
    fn a_group_that_cannot_be_removed_keeps_its_record() {
        let t = tree();
        let dir = job(&t, "s.scope", "job-bash-7-1000", false, 1 << 20);
        let rec = record(&t, "s.scope", "job-bash-7-1000", "x");
        let failing = |_: &Path| Err(io::Error::from(ErrorKind::ResourceBusy));
        let measured = collect(&t.slice, &t.records, NOW, failing).unwrap();
        assert_eq!(measured, Vec::new());
        assert!(dir.exists() && rec.exists());
        assert_eq!(collect_all(&t).len(), 1);
    }

    #[test]
    fn records_of_gone_sessions_are_dropped() {
        let t = tree();
        job(&t, "live.scope", "job-bash-7-99000", true, 0);
        let kept = record(&t, "live.scope", "job-bash-7-99000", "x");
        let gone = record(&t, "gone.scope", "job-bash-8-1000", "y");
        collect_all(&t);
        assert!(kept.exists());
        assert!(!gone.exists());
    }

    #[test]
    fn nothing_to_do_before_any_session() {
        let t = tree();
        fs::remove_dir(&t.slice).unwrap();
        assert_eq!(collect_all(&t), Vec::new());
    }
}
