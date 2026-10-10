//! Claude Code's own record of its live sessions: one `<pid>.json` per session
//! in its sessions directory. The `.key` files next to them hold credentials
//! and are never opened.

use inotify::WatchMask;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// The environ variable naming the session a process belongs to, as
/// `NAME=`. Every process a session starts inherits it, with the id the
/// session had then: after `/clear` or a resume, it names an id the session
/// no longer has. The environ that holds it also holds the session's
/// messaging token, so nothing else of it is kept.
// claude-code: session-id-variable
pub const ID_VAR: &[u8] = b"CLAUDE_CODE_SESSION_ID=";

/// What to watch the sessions directory for: a new session's file appears,
/// and Claude Code rewrites a session's file when its status changes.
// claude-code: session-file
pub const CHANGES: WatchMask = WatchMask::CLOSE_WRITE
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::ONLYDIR);

// claude-code: session-file-fields
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSession {
    /// The session's claude process: every process of the session descends
    /// from it.
    pub pid: u32,
    pub session_id: String,
    pub name: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub status: Option<String>,
    /// Start time of the claude process in clock ticks, as a string. Lets a
    /// file left behind by a dead session be told apart from a reused pid.
    #[serde(default)]
    pub proc_start: Option<String>,
}

/// `$CLAUDE_CONFIG_DIR/sessions`, or `~/.claude/sessions`.
// claude-code: config-dir
pub fn default_dir() -> Option<PathBuf> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))?;
    Some(base.join("sessions"))
}

/// Reads every `<pid>.json` in `dir`. A file that cannot be parsed is reported
/// on stderr and skipped: Claude Code may be rewriting it.
// claude-code: session-file
pub fn read_sessions(dir: &Path) -> std::io::Result<Vec<ClaudeSession>> {
    let mut sessions = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !is_session_file(&path) {
            continue;
        }
        let parsed = fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()));
        match parsed {
            Ok(session) => sessions.push(session),
            Err(e) => eprintln!("orchestrator: skipping {}: {e}", path.display()),
        }
    }
    Ok(sessions)
}

impl ClaudeSession {
    /// Whether the process now at `pid`, started at `start_time` in clock
    /// ticks since boot, is this session's: a session file can outlive its
    /// process, whose pid may then be reused.
    // claude-code: session-file-fields
    pub fn is_process(&self, start_time: u64) -> bool {
        self.proc_start
            .as_deref()
            .is_none_or(|p| p.parse::<u64>().is_ok_and(|p| p == start_time))
    }
}

fn is_session_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "json")
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_session_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::write(
            d.join("100.json"),
            r#"{"pid":100,"sessionId":"s1","cwd":"/w","name":"alpha","status":"idle","procStart":"42","extra":1}"#,
        )
        .unwrap();
        fs::write(d.join("100.0123abcd.key"), "not json, must not be read").unwrap();
        fs::write(d.join("notes.json"), "{}").unwrap();
        fs::write(d.join("200.json"), "{ truncated").unwrap();

        let sessions = read_sessions(d).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(
            (s.pid, s.session_id.as_str(), s.name.as_str()),
            (100, "s1", "alpha")
        );
        assert_eq!(s.status.as_deref(), Some("idle"));
        assert_eq!(s.proc_start.as_deref(), Some("42"));
    }
}
