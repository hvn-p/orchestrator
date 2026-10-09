//! Which Claude session each process belongs to, and which processes outlived
//! theirs. Pure: everything comes in as data.
//!
//! A process belongs to the live session whose claude process it descends
//! from, else to the one its `CLAUDE_CODE_SESSION_ID` names, whether the
//! session was started through `launch` or not. Ancestry comes first: the
//! variable is inherited, but keeps the old id after `/clear` or a resume.
//! A job needs none of this: its group lies in its session's scope, so a
//! measurement or a waiting call names its session by it.

use crate::procfs::ProcInfo;
use crate::sessions::ClaudeSession;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Guards the ancestry walk against a malformed ppid cycle.
const MAX_DEPTH: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcRef {
    pub pid: u32,
    pub rss_kb: u64,
    pub comm: String,
    pub cmdline: String,
    pub command_head: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUsage {
    pub name: String,
    pub session_id: String,
    pub rss_kb: u64,
    pub largest: ProcRef,
}

/// The topmost process of a group left behind by a dead session, with the
/// memory of the whole group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub root: ProcRef,
    pub start_time: u64,
    pub session_id: String,
    pub rss_kb: u64,
    pub processes: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Attribution {
    /// Biggest first.
    pub sessions: Vec<SessionUsage>,
    /// Biggest first.
    pub orphans: Vec<Orphan>,
}

/// A process belongs to the live session whose claude process it descends
/// from. Failing that (reparented by nohup or setsid), to the live session
/// named by its `CLAUDE_CODE_SESSION_ID`. That variable keeps the id the
/// session had when the process started, and goes stale after `/clear` or a
/// resume: such a reparented process is then reported as an orphan.
// claude-code: session-id-variable
pub fn attribute(procs: &[ProcInfo], sessions: &[ClaudeSession]) -> Attribution {
    let by_pid: HashMap<u32, &ProcInfo> = procs.iter().map(|p| (p.pid, p)).collect();
    let live: Vec<&ClaudeSession> = sessions.iter().filter(|s| is_alive(s, &by_pid)).collect();
    let live_by_pid: HashMap<u32, &ClaudeSession> = live.iter().map(|s| (s.pid, *s)).collect();
    let live_by_id: HashMap<&str, &ClaudeSession> =
        live.iter().map(|s| (s.session_id.as_str(), *s)).collect();

    let mut usage: HashMap<&str, SessionUsage> = HashMap::new();
    let mut orphan_pids: HashSet<u32> = HashSet::new();
    for p in procs {
        let owner = owning_session(p, &by_pid, &live_by_pid).or_else(|| {
            p.session_env
                .as_deref()
                .and_then(|id| live_by_id.get(id).copied())
        });
        match owner {
            Some(s) => {
                let entry = usage
                    .entry(s.session_id.as_str())
                    .or_insert_with(|| SessionUsage {
                        name: s.name.clone(),
                        session_id: s.session_id.clone(),
                        rss_kb: 0,
                        largest: proc_ref(p),
                    });
                entry.rss_kb += p.rss_kb;
                if p.rss_kb > entry.largest.rss_kb {
                    entry.largest = proc_ref(p);
                }
            }
            None if p.session_env.is_some() => {
                orphan_pids.insert(p.pid);
            }
            None => {}
        }
    }

    let mut sessions: Vec<SessionUsage> = usage.into_values().collect();
    sessions.sort_by(|a, b| b.rss_kb.cmp(&a.rss_kb).then_with(|| a.name.cmp(&b.name)));
    Attribution {
        sessions,
        orphans: group_orphans(&orphan_pids, &by_pid),
    }
}

// claude-code: session-file-fields
fn is_alive(s: &ClaudeSession, by_pid: &HashMap<u32, &ProcInfo>) -> bool {
    match (by_pid.get(&s.pid), s.proc_start.as_deref()) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(p), Some(start)) => start.parse::<u64>().is_ok_and(|s| s == p.start_time),
    }
}

// claude-code: session-file
fn owning_session<'a>(
    p: &ProcInfo,
    by_pid: &HashMap<u32, &ProcInfo>,
    live_by_pid: &HashMap<u32, &'a ClaudeSession>,
) -> Option<&'a ClaudeSession> {
    let mut pid = p.pid;
    for _ in 0..MAX_DEPTH {
        if let Some(s) = live_by_pid.get(&pid) {
            return Some(s);
        }
        pid = by_pid.get(&pid)?.ppid;
        if pid <= 1 {
            return None;
        }
    }
    None
}

/// Folds each orphan into its topmost orphan ancestor, so a dev server shows
/// up once rather than as npm, sh and node.
fn group_orphans(orphan_pids: &HashSet<u32>, by_pid: &HashMap<u32, &ProcInfo>) -> Vec<Orphan> {
    let mut groups: HashMap<u32, Orphan> = HashMap::new();
    for pid in orphan_pids {
        let mut root = *pid;
        for _ in 0..MAX_DEPTH {
            match by_pid.get(&root) {
                Some(p) if orphan_pids.contains(&p.ppid) => root = p.ppid,
                _ => break,
            }
        }
        let (Some(root_proc), Some(member)) = (by_pid.get(&root), by_pid.get(pid)) else {
            continue;
        };
        let group = groups.entry(root).or_insert_with(|| Orphan {
            root: proc_ref(root_proc),
            start_time: root_proc.start_time,
            session_id: root_proc.session_env.clone().unwrap_or_default(),
            rss_kb: 0,
            processes: 0,
        });
        group.rss_kb += member.rss_kb;
        group.processes += 1;
    }
    let mut orphans: Vec<Orphan> = groups.into_values().collect();
    orphans.sort_by(|a, b| {
        b.rss_kb
            .cmp(&a.rss_kb)
            .then_with(|| a.root.pid.cmp(&b.root.pid))
    });
    orphans
}

fn proc_ref(p: &ProcInfo) -> ProcRef {
    ProcRef {
        pid: p.pid,
        rss_kb: p.rss_kb,
        comm: p.comm.clone(),
        cmdline: p.cmdline.clone(),
        command_head: p.command_head.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, rss_kb: u64, env: Option<&str>) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            start_time: u64::from(pid) * 10,
            rss_kb,
            comm: format!("p{pid}"),
            cmdline: format!("cmd {pid}"),
            command_head: format!("cmd {pid}"),
            session_env: env.map(str::to_string),
        }
    }

    fn session(pid: u32, id: &str, name: &str, proc_start: Option<u64>) -> ClaudeSession {
        ClaudeSession {
            pid,
            session_id: id.into(),
            name: name.into(),
            cwd: String::new(),
            status: None,
            proc_start: proc_start.map(|s| s.to_string()),
        }
    }

    #[test]
    fn attributes_by_ancestry_then_environment() {
        let procs = vec![
            proc(100, 50, 300, None),        // claude process of alpha
            proc(101, 100, 1000, Some("a")), // its MCP server
            proc(102, 101, 2000, Some("old-id-after-clear")),
            proc(200, 1, 700, Some("a")), // reparented dev server of alpha
            proc(400, 50, 100, None),     // claude process of beta
        ];
        let sessions = vec![
            session(100, "a", "alpha", Some(1000)),
            session(400, "b", "beta", None),
        ];

        let att = attribute(&procs, &sessions);
        assert_eq!(att.orphans, vec![]);
        assert_eq!(att.sessions.len(), 2);
        let alpha = &att.sessions[0];
        assert_eq!(
            (alpha.name.as_str(), alpha.rss_kb, alpha.largest.pid),
            ("alpha", 4000, 102)
        );
        assert_eq!(
            (att.sessions[1].name.as_str(), att.sessions[1].rss_kb),
            ("beta", 100)
        );
    }

    #[test]
    fn groups_processes_of_dead_sessions_into_orphans() {
        let procs = vec![
            proc(300, 1, 50, Some("dead")),    // npm exec, reparented
            proc(301, 300, 10, Some("dead")),  // sh
            proc(302, 301, 900, Some("dead")), // node dev server
            proc(310, 1, 5, Some("other-dead")),
            proc(320, 1, 999, None), // unrelated process: ignored
        ];
        let att = attribute(&procs, &[]);
        assert_eq!(att.sessions, []);
        assert_eq!(att.orphans.len(), 2);
        let big = &att.orphans[0];
        assert_eq!((big.root.pid, big.rss_kb, big.processes), (300, 960, 3));
        assert_eq!(big.session_id, "dead");
        assert_eq!(att.orphans[1].root.pid, 310);
    }

    #[test]
    fn a_session_file_with_a_reused_pid_is_not_alive() {
        // pid 500 now runs something else: its start time no longer matches.
        let procs = vec![proc(500, 1, 10, None), proc(501, 500, 400, Some("c"))];
        let att = attribute(&procs, &[session(500, "c", "gamma", Some(1))]);
        assert_eq!(att.sessions, []);
        assert_eq!(att.orphans.len(), 1);
        assert_eq!(att.orphans[0].root.pid, 501);
    }

    #[test]
    fn a_session_file_whose_process_is_gone_is_not_alive() {
        let procs = vec![proc(601, 1, 400, Some("d"))];
        let att = attribute(&procs, &[session(600, "d", "delta", None)]);
        assert_eq!(att.sessions, []);
        assert_eq!(att.orphans[0].session_id, "d");
    }
}
