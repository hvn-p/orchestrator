//! Process table read straight from a proc root, so tests can point it at a
//! fake tree.

use std::fs;
use std::path::Path;

/// The only environ variable orchestrator reads. Every process a Claude session
/// launches inherits it. Nothing else in environ is ever kept: it also holds
/// the session's messaging token.
const SESSION_VAR: &[u8] = b"CLAUDE_CODE_SESSION_ID=";

/// Longest command line kept, in characters.
const CMDLINE_MAX: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    /// Start time in clock ticks since boot: with the pid, it identifies a
    /// process even after its pid is reused.
    pub start_time: u64,
    pub rss_kb: u64,
    pub comm: String,
    pub cmdline: String,
    pub session_env: Option<String>,
}

/// Reads every process under `root`. A process that exits while being read is
/// skipped, and so is one whose stat cannot be parsed.
pub fn read_processes(root: &Path) -> std::io::Result<Vec<ProcInfo>> {
    let mut procs = Vec::new();
    for entry in fs::read_dir(root)? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        if let Some(proc_info) = read_process(&entry.path(), pid) {
            procs.push(proc_info);
        }
    }
    Ok(procs)
}

fn read_process(dir: &Path, pid: u32) -> Option<ProcInfo> {
    let stat = fs::read_to_string(dir.join("stat")).ok()?;
    let (comm, ppid, start_time) = parse_stat(&stat)?;
    // Kernel threads have no VmRSS line.
    let rss_kb = fs::read_to_string(dir.join("status"))
        .ok()
        .and_then(|s| parse_rss_kb(&s))
        .unwrap_or(0);
    // Unreadable for other users' processes, which is fine: they are not ours.
    let session_env = fs::read(dir.join("environ"))
        .ok()
        .and_then(|e| parse_session_env(&e));
    let cmdline = fs::read(dir.join("cmdline"))
        .map(|c| parse_cmdline(&c))
        .unwrap_or_default();
    Some(ProcInfo {
        pid,
        ppid,
        start_time,
        rss_kb,
        comm,
        cmdline,
        session_env,
    })
}

/// Returns (comm, ppid, starttime). comm sits between the first `(` and the
/// last `)` and may itself contain spaces or parentheses, so fields are counted
/// from the last `)`: state is field 3, ppid field 4, starttime field 22.
fn parse_stat(stat: &str) -> Option<(String, u32, u64)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    let fields: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let start_time = fields.get(19)?.parse().ok()?;
    Some((comm, ppid, start_time))
}

fn parse_rss_kb(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

fn parse_session_env(environ: &[u8]) -> Option<String> {
    environ
        .split(|b| *b == 0)
        .find_map(|var| var.strip_prefix(SESSION_VAR))
        .and_then(|id| std::str::from_utf8(id).ok())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn parse_cmdline(raw: &[u8]) -> String {
    let joined = raw
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ");
    truncate(&joined, CMDLINE_MAX)
}

pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat_line(pid: u32, comm: &str, ppid: u32, start: u64) -> String {
        // 52 fields as in Linux 6.x; only ppid (4) and starttime (22) matter.
        let mut rest: Vec<String> = vec!["S".into(), ppid.to_string()];
        rest.extend((5..22).map(|i| i.to_string()));
        rest.push(start.to_string());
        rest.extend((23..=52).map(|i| i.to_string()));
        format!("{pid} ({comm}) {}\n", rest.join(" "))
    }

    fn write_proc(root: &Path, pid: u32, stat: &str, rss: Option<u64>, environ: &[u8], cmd: &[u8]) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("stat"), stat).unwrap();
        let status = match rss {
            Some(kb) => format!("Name:\tx\nPPid:\t1\nVmRSS:\t  {kb} kB\n"),
            None => "Name:\tkthread\n".to_string(),
        };
        fs::write(dir.join("status"), status).unwrap();
        fs::write(dir.join("environ"), environ).unwrap();
        fs::write(dir.join("cmdline"), cmd).unwrap();
    }

    #[test]
    fn parses_stat_with_awkward_comm() {
        let (comm, ppid, start) = parse_stat(&stat_line(7, "Web (Co) x", 42, 999)).unwrap();
        assert_eq!((comm.as_str(), ppid, start), ("Web (Co) x", 42, 999));
    }

    #[test]
    fn keeps_only_the_session_variable() {
        let env =
            b"PATH=/bin\0CLAUDE_CODE_MESSAGING_TOKEN=secret\0CLAUDE_CODE_SESSION_ID=abc-123\0";
        assert_eq!(parse_session_env(env).as_deref(), Some("abc-123"));
        assert_eq!(
            parse_session_env(b"CLAUDE_CODE_MESSAGING_TOKEN=secret\0"),
            None
        );
    }

    #[test]
    fn truncates_long_command_lines() {
        let raw = format!("node\0{}\0", "a".repeat(500));
        let cmd = parse_cmdline(raw.as_bytes());
        assert_eq!(cmd.chars().count(), CMDLINE_MAX);
        assert!(cmd.starts_with("node aaa"));
        assert!(cmd.ends_with('…'));
    }

    #[test]
    fn reads_a_fake_tree_and_skips_noise() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        write_proc(
            r,
            10,
            &stat_line(10, "node", 1, 500),
            Some(2048),
            b"CLAUDE_CODE_SESSION_ID=s1\0",
            b"node\0server.js\0",
        );
        write_proc(r, 11, &stat_line(11, "kworker/0:1", 2, 3), None, b"", b"");
        // A non-pid entry and a pid whose stat is gone (process exited mid-read).
        fs::create_dir_all(r.join("self")).unwrap();
        fs::create_dir_all(r.join("12")).unwrap();

        let mut procs = read_processes(r).unwrap();
        procs.sort_by_key(|p| p.pid);
        assert_eq!(procs.len(), 2);
        let node = &procs[0];
        assert_eq!(
            (node.pid, node.ppid, node.start_time, node.rss_kb),
            (10, 1, 500, 2048)
        );
        assert_eq!(node.cmdline, "node server.js");
        assert_eq!(node.session_env.as_deref(), Some("s1"));
        assert_eq!(
            (procs[1].rss_kb, procs[1].session_env.as_deref()),
            (0, None)
        );
    }
}
