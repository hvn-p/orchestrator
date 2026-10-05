//! Events for the coordinator: one JSON object per line, appended to a file
//! it monitors. They are inputs for scheduling work (delay, queue, throttle,
//! reorder), never orders to stop it. Sizes are in MB.

use crate::attribution::{Attribution, Orphan, SessionUsage};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// Sessions listed after the largest one in `memory_pressure`.
const NEXT: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionSummary {
    pub session: String,
    pub session_id: String,
    pub rss_mb: u64,
    /// The session's largest process.
    pub process_pid: u32,
    pub process_rss_mb: u64,
    pub process_comm: String,
    pub process_cmdline: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionBrief {
    pub session: String,
    pub rss_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrphanSummary {
    pub pid: u32,
    pub session_id: String,
    pub rss_mb: u64,
    pub processes: usize,
    pub comm: String,
    pub cmdline: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    MemoryPressure {
        available_mb: u64,
        /// The trigger: some task stalled on memory this long within a PSI
        /// window.
        stall_ms: u64,
        largest: Option<SessionSummary>,
        next: Vec<SessionBrief>,
    },
    Orphans {
        orphans: Vec<OrphanSummary>,
    },
}

impl Event {
    pub fn memory_pressure(available_mb: u64, stall_ms: u64, att: &Attribution) -> Self {
        Event::MemoryPressure {
            available_mb,
            stall_ms,
            largest: att.sessions.first().map(summary),
            next: att
                .sessions
                .iter()
                .skip(1)
                .take(NEXT)
                .map(|s| SessionBrief {
                    session: s.name.clone(),
                    rss_mb: s.rss_kb / 1024,
                })
                .collect(),
        }
    }

    pub fn orphans(orphans: &[&Orphan]) -> Self {
        Event::Orphans {
            orphans: orphans
                .iter()
                .map(|o| OrphanSummary {
                    pid: o.root.pid,
                    session_id: o.session_id.clone(),
                    rss_mb: o.rss_kb / 1024,
                    processes: o.processes,
                    comm: o.root.comm.clone(),
                    cmdline: o.root.cmdline.clone(),
                })
                .collect(),
        }
    }
}

fn summary(s: &SessionUsage) -> SessionSummary {
    SessionSummary {
        session: s.name.clone(),
        session_id: s.session_id.clone(),
        rss_mb: s.rss_kb / 1024,
        process_pid: s.largest.pid,
        process_rss_mb: s.largest.rss_kb / 1024,
        process_comm: s.largest.comm.clone(),
        process_cmdline: s.largest.cmdline.clone(),
    }
}

#[derive(Serialize)]
struct Line<'a> {
    at: u64,
    #[serde(flatten)]
    event: &'a Event,
}

pub fn to_line(at: u64, event: &Event) -> Result<String> {
    serde_json::to_string(&Line { at, event }).context("serializing event")
}

/// Appends one line. The file is opened per event: the coordinator tails it
/// and may truncate it between two events.
pub fn append(path: &Path, at: u64, event: &Event) -> Result<()> {
    append_line(path, &Line { at, event })
}

/// Appends `value` as one JSON line.
pub fn append_line(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_string(value).context("serializing a line")?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::ProcRef;

    #[test]
    fn memory_pressure_line_shape() {
        let att = Attribution {
            sessions: vec![SessionUsage {
                name: "alpha".into(),
                session_id: "a".into(),
                rss_kb: 4096 * 1024,
                largest: ProcRef {
                    pid: 7,
                    rss_kb: 3072 * 1024,
                    comm: "python3".into(),
                    cmdline: "python3 -c x".into(),
                },
            }],
            orphans: vec![],
        };
        let line = to_line(12, &Event::memory_pressure(900, 200, &att)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["at"], 12);
        assert_eq!(v["kind"], "memory_pressure");
        assert_eq!(v["available_mb"], 900);
        assert_eq!(v["stall_ms"], 200);
        assert_eq!(v["largest"]["session"], "alpha");
        assert_eq!(v["largest"]["rss_mb"], 4096);
        assert_eq!(v["largest"]["process_rss_mb"], 3072);
        assert_eq!(v["next"], serde_json::json!([]));
    }

    #[test]
    fn appends_one_line_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let event = Event::Orphans { orphans: vec![] };
        append(&path, 1, &event).unwrap();
        append(&path, 2, &event).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "{\"at\":1,\"kind\":\"orphans\",\"orphans\":[]}\n{\"at\":2,\"kind\":\"orphans\",\"orphans\":[]}\n"
        );
    }
}
