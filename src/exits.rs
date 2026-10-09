//! Ends of Claude Code sessions, as the kernel reports them. inotify on Claude
//! Code's sessions directory shows each new session; a pidfd then holds the
//! session's claude process and becomes readable when that process exits.
//!
//! A pidfd per session reports only what matters, where the kernel process
//! connector would report every process of the machine, thousands per
//! second during a build.

use crate::procfs;
use crate::sessions::{self, ClaudeSession};
use inotify::{Inotify, WatchMask};
use rustix::process::{Pid, PidfdFlags, pidfd_open};
use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;

/// Room for a batch of inotify events; one takes 16 bytes plus its name.
const EVENT_BUFFER: usize = 4096;

pub struct Exits {
    inotify: Inotify,
    dir: PathBuf,
    proc_root: PathBuf,
    /// The claude process of each live session, by pid.
    held: HashMap<u32, OwnedFd>,
}

impl Exits {
    /// Watches `dir`, Claude Code's sessions directory, and holds the claude
    /// process of every live session in it.
    // claude-code: session-file
    pub fn new(dir: PathBuf, proc_root: PathBuf) -> io::Result<Exits> {
        let inotify = Inotify::init()?;
        // Claude Code rewrites a session file when the session's status changes.
        let mask = WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::ONLYDIR;
        inotify.watches().add(&dir, mask)?;
        let mut exits = Exits {
            inotify,
            dir,
            proc_root,
            held: HashMap::new(),
        };
        exits.hold_new();
        Ok(exits)
    }

    /// Readable when the sessions directory changed.
    pub fn sessions_fd(&self) -> BorrowedFd<'_> {
        self.inotify.as_fd()
    }

    /// The held processes: each descriptor is readable once its process exited.
    pub fn held(&self) -> impl Iterator<Item = (u32, BorrowedFd<'_>)> {
        self.held.iter().map(|(pid, fd)| (*pid, fd.as_fd()))
    }

    /// Drains what the sessions directory reported, then holds new sessions.
    pub fn sessions_changed(&mut self) -> io::Result<()> {
        let mut buffer = [0; EVENT_BUFFER];
        loop {
            match self.inotify.read_events(&mut buffer) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        self.hold_new();
        Ok(())
    }

    /// The claude process `pid` exited: stops holding it.
    pub fn release(&mut self, pid: u32) {
        self.held.remove(&pid);
    }

    // claude-code: session-file
    fn hold_new(&mut self) {
        let Ok(found) = sessions::read_sessions(&self.dir) else {
            return;
        };
        for s in found {
            if self.held.contains_key(&s.pid) || !self.alive(&s) {
                continue;
            }
            let pid = i32::try_from(s.pid).ok().and_then(Pid::from_raw);
            if let Some(fd) = pid.and_then(|p| pidfd_open(p, PidfdFlags::empty()).ok()) {
                self.held.insert(s.pid, fd);
            }
        }
    }

    /// A session file can outlive its process, whose pid may then be reused.
    // claude-code: session-file-fields
    fn alive(&self, s: &ClaudeSession) -> bool {
        let Some(started) = procfs::start_time(&self.proc_root, s.pid) else {
            return false;
        };
        s.proc_start
            .as_deref()
            .is_none_or(|p| p.parse::<u64>().is_ok_and(|p| p == started))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use std::path::Path;
    use std::process::{Child, Command};
    use std::time::Duration;

    fn session_file(dir: &Path, child: &Child, proc_start: Option<u64>) {
        let start = proc_start.map_or(String::new(), |s| format!(r#","procStart":"{s}""#));
        let json = format!(
            r#"{{"pid":{},"sessionId":"s","name":"n"{start}}}"#,
            child.id()
        );
        std::fs::write(dir.join(format!("{}.json", child.id())), json).unwrap();
    }

    fn readable(fd: BorrowedFd<'_>, ms: u64) -> bool {
        let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
        let timeout = Timespec::try_from(Duration::from_millis(ms)).unwrap();
        poll(&mut fds, Some(&timeout)).unwrap() == 1
    }

    #[test]
    fn a_session_is_held_until_its_process_exits() {
        let dir = tempfile::tempdir().unwrap();
        let proc_root = PathBuf::from("/proc");
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let start = procfs::start_time(&proc_root, pid);
        let mut exits = Exits::new(dir.path().into(), proc_root).unwrap();
        assert_eq!(exits.held().count(), 0);

        session_file(dir.path(), &child, start);
        assert!(readable(exits.sessions_fd(), 1000));
        exits.sessions_changed().unwrap();
        let held: Vec<u32> = exits.held().map(|(p, _)| p).collect();
        assert_eq!(held, [pid]);
        let fd = exits.held().next().unwrap().1;
        assert!(!readable(fd, 10));

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(readable(exits.held().next().unwrap().1, 1000));
        exits.release(pid);
        assert_eq!(exits.held().count(), 0);
    }

    #[test]
    fn a_stale_session_file_is_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        // The pid is alive but started at another time: it was reused.
        session_file(dir.path(), &child, Some(1));
        let exits = Exits::new(dir.path().into(), PathBuf::from("/proc")).unwrap();
        assert_eq!(exits.held().count(), 0);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
