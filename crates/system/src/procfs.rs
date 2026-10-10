//! Process table read straight from a proc root, so tests can point it at a
//! fake tree.

use std::fs;
use std::path::Path;

/// Longest command line kept, in characters.
const CMDLINE_MAX: usize = 200;
/// Words after the program a command head keeps.
const HEAD_WORDS: usize = 2;
/// Longest word a command head keeps, in characters.
const HEAD_WORD_MAX: usize = 32;

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
    /// See `command_head`.
    pub command_head: String,
    /// The value of the session variable `read_processes` was given.
    pub session_env: Option<String>,
}

/// Reads every process under `root`. A process that exits while being read is
/// skipped, and so is one whose stat cannot be parsed.
/// The processes under `root`. `session_var`, `NAME=`, is the only environ
/// variable read, into `session_env`: nothing else in environ is ever kept,
/// since it may hold credentials.
pub fn read_processes(root: &Path, session_var: &[u8]) -> std::io::Result<Vec<ProcInfo>> {
    let mut procs = Vec::new();
    for entry in fs::read_dir(root)? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        if let Some(proc_info) = read_process(&entry.path(), pid, session_var) {
            procs.push(proc_info);
        }
    }
    Ok(procs)
}

/// Process `pid` under `root`, as `read_processes` reads it. None once it
/// is gone.
pub fn read_one(root: &Path, pid: u32, session_var: &[u8]) -> Option<ProcInfo> {
    read_process(&root.join(pid.to_string()), pid, session_var)
}

fn read_process(dir: &Path, pid: u32, session_var: &[u8]) -> Option<ProcInfo> {
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
        .and_then(|e| parse_session_env(&e, session_var));
    let raw_cmdline = fs::read(dir.join("cmdline")).unwrap_or_default();
    let cmdline = parse_cmdline(&raw_cmdline);
    let command_head = command_head(&raw_cmdline);
    Some(ProcInfo {
        pid,
        ppid,
        start_time,
        rss_kb,
        comm,
        cmdline,
        command_head,
        session_env,
    })
}

/// Start time of process `pid` under `root`, in clock ticks since boot. None
/// once it is gone.
pub fn start_time(root: &Path, pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(root.join(pid.to_string()).join("stat")).ok()?;
    parse_stat(&stat).map(|(_, _, start)| start)
}

/// Whether process `pid` under `root` is the one that started at
/// `start_time` and has not exited: a zombie waiting for its parent has.
pub fn running(root: &Path, pid: u32, start_time: u64) -> bool {
    let Ok(stat) = fs::read_to_string(root.join(pid.to_string()).join("stat")) else {
        return false;
    };
    let state = stat
        .rfind(')')
        .and_then(|close| stat.get(close + 1..))
        .and_then(|rest| rest.split_whitespace().next());
    parse_stat(&stat).is_some_and(|(_, _, start)| start == start_time)
        && !matches!(state, Some("Z" | "X" | "x"))
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

fn parse_session_env(environ: &[u8], session_var: &[u8]) -> Option<String> {
    environ
        .split(|b| *b == 0)
        .find_map(|var| var.strip_prefix(session_var))
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

/// What a command line says about a process, without the arguments that
/// could hold a credential: the program, then at most two plain words, a path
/// reduced to its last component. It stops at the first option or at a word
/// that could carry data: a URL, `=`, `:`, quotes, or more than 32 characters.
/// A process that rewrote its title into one string with spaces is split into
/// words first. A secret passed as one of the first two plain positional
/// arguments would still show; that form is rare.
pub fn command_head(raw: &[u8]) -> String {
    let mut words = raw
        .split(|b| *b == 0)
        .map(String::from_utf8_lossy)
        .flat_map(|arg| {
            arg.split_ascii_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
    let Some(program) = words.next().as_deref().and_then(plain_word) else {
        return String::new();
    };
    let mut head = vec![program];
    for word in words.take(HEAD_WORDS) {
        match plain_word(&word) {
            Some(w) if !word.starts_with('-') => head.push(w),
            _ => break,
        }
    }
    head.join(" ")
}

fn plain_word(word: &str) -> Option<String> {
    if word.contains("://") {
        return None;
    }
    let last = word.rsplit('/').next().unwrap_or(word);
    let plain = !last.is_empty()
        && last.chars().count() <= HEAD_WORD_MAX
        && last
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._@+-".contains(c));
    plain.then(|| last.to_string())
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

    const VAR: &[u8] = b"SESSION_ID=";

    fn stat_line(pid: u32, comm: &str, ppid: u32, start: u64) -> String {
        // 52 fields as in Linux 6.x; only ppid (4) and starttime (22) matter.
        let mut rest: Vec<String> = vec!["S".into(), ppid.to_string()];
        rest.extend((5..22).map(|i| i.to_string()));
        rest.push(start.to_string());
        rest.extend((23..=52).map(|i| i.to_string()));
        format!("{pid} ({comm}) {}\n", rest.join(" "))
    }

    #[test]
    fn a_zombie_or_a_reused_pid_is_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_proc(
            root,
            7,
            &stat_line(7, "claude (x)", 1, 500),
            Some(1),
            b"",
            b"",
        );
        assert!(running(root, 7, 500));
        assert!(!running(root, 7, 501));
        assert!(!running(root, 8, 500));
        let zombie = stat_line(7, "claude (x)", 1, 500).replacen(") S ", ") Z ", 1);
        fs::write(root.join("7/stat"), zombie).unwrap();
        assert!(!running(root, 7, 500));
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
        let env = b"PATH=/bin\0MESSAGING_TOKEN=secret\0SESSION_ID=abc-123\0";
        assert_eq!(parse_session_env(env, VAR).as_deref(), Some("abc-123"));
        assert_eq!(parse_session_env(b"MESSAGING_TOKEN=secret\0", VAR), None);
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
            b"SESSION_ID=s1\0",
            b"node\0server.js\0",
        );
        write_proc(r, 11, &stat_line(11, "kworker/0:1", 2, 3), None, b"", b"");
        // A non-pid entry and a pid whose stat is gone (process exited mid-read).
        fs::create_dir_all(r.join("self")).unwrap();
        fs::create_dir_all(r.join("12")).unwrap();

        let mut procs = read_processes(r, VAR).unwrap();
        procs.sort_by_key(|p| p.pid);
        assert_eq!(procs.len(), 2);
        let node = &procs[0];
        assert_eq!(
            (node.pid, node.ppid, node.start_time, node.rss_kb),
            (10, 1, 500, 2048)
        );
        assert_eq!(node.cmdline, "node server.js");
        assert_eq!(node.command_head, "node server.js");
        assert_eq!(node.session_env.as_deref(), Some("s1"));
        assert_eq!(
            (procs[1].rss_kb, procs[1].session_env.as_deref()),
            (0, None)
        );
    }

    #[test]
    fn a_command_head_keeps_no_argument_that_could_hold_a_secret() {
        let head = |raw: &str| command_head(raw.as_bytes());
        let url = "mongodb://user:secret@host:10255/db";
        assert_eq!(
            head(&format!("npm\0exec\0mongodb-lens@latest\0{url}\0")),
            "npm exec mongodb-lens@latest"
        );
        // A process title rewritten into a single string.
        assert_eq!(
            head(&format!("npm exec mongodb-lens@latest {url}\0")),
            "npm exec mongodb-lens@latest"
        );
        assert_eq!(head(&format!("tool\0{url}\0")), "tool");
        assert_eq!(
            head("curl\0https://hooks.example.com/T0/B0/SECRET\0"),
            "curl"
        );
        assert_eq!(head("tool\0--token=abc\0"), "tool");
        assert_eq!(head("tool\0user:secret\0"), "tool");
        assert_eq!(head("/usr/bin/python3\0-c\0print(1)\0"), "python3");
        assert_eq!(
            head("node\0/x/node_modules/.bin/mcp-server\0Org\0-d\0core\0"),
            "node mcp-server Org"
        );
        assert_eq!(head("sleep\x00300\x00"), "sleep 300");
        assert_eq!(head(""), "");
    }
}
