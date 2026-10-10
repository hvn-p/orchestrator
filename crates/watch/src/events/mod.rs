//! Events for the coordinator: one JSON object per line, appended to a file
//! it monitors. They are inputs for scheduling work (delay, queue, throttle,
//! reorder), never orders to stop it. Sizes are in MB.
//!
//! A process is named by its pid, its `comm` and the head of its command line
//! (`procfs::command_head`), never by the full command line: arguments can
//! hold credentials, and the coordinator hands what it reads to a model. A
//! Bash call is named by its label instead (see `peaks`): a command Claude
//! wrote, which went through the model already.

mod admission_wait;
mod memory_pressure;
mod orphans;

pub use admission_wait::AdmissionWait;
pub use memory_pressure::{MemoryPressure, SessionBrief, SessionSummary};
pub use orphans::{OrphanSummary, Orphans};

use crate::attribution::{Attribution, Orphan};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// One kind of event per variant, its payload in a module of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    MemoryPressure(MemoryPressure),
    Orphans(Orphans),
    AdmissionWait(AdmissionWait),
}

impl Event {
    pub fn memory_pressure(available_mb: u64, stall_ms: u64, att: &Attribution) -> Self {
        Event::MemoryPressure(MemoryPressure::new(available_mb, stall_ms, att))
    }

    pub fn orphans(orphans: &[&Orphan]) -> Self {
        Event::Orphans(Orphans::new(orphans))
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

/// The event as its line holds it.
pub fn to_value(at: u64, event: &Event) -> Result<serde_json::Value> {
    serde_json::to_value(Line { at, event }).context("serializing event")
}

/// Appends one line. The file is opened per event: the coordinator tails it
/// and may truncate it between two events.
pub fn append(path: &Path, at: u64, event: &Event) -> Result<()> {
    append_line(path, &Line { at, event })
}

/// Appends one line, then hands it to `hub`'s subscribers.
pub fn emit(path: &Path, hub: &Hub, at: u64, event: &Event) -> Result<()> {
    append(path, at, event)?;
    hub.publish(&to_line(at, event)?);
    Ok(())
}

/// The subscribers to the events as they come, the API's event streams.
#[derive(Debug, Clone, Default)]
pub struct Hub(Arc<Mutex<Vec<Sender<String>>>>);

impl Hub {
    /// Every line published from now on, as `to_line` writes it.
    pub fn subscribe(&self) -> Receiver<String> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut subscribers) = self.0.lock() {
            subscribers.push(tx);
        }
        rx
    }

    /// Hands `line` to every subscriber still listening.
    pub fn publish(&self, line: &str) {
        if let Ok(mut subscribers) = self.0.lock() {
            subscribers.retain(|s| s.send(line.to_string()).is_ok());
        }
    }
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
    use crate::attribution::{ProcRef, SessionUsage};

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
                    command_head: "python3".into(),
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
        assert_eq!(v["largest"]["process_comm"], "python3");
        assert_eq!(v["largest"]["process_command"], "python3");
        assert!(v["largest"].get("process_cmdline").is_none());
        assert_eq!(v["next"], serde_json::json!([]));
    }

    #[test]
    fn admission_wait_line_shape() {
        let event = Event::AdmissionWait(AdmissionWait {
            session: Some("alpha".into()),
            session_id: Some("a".into()),
            job: "job-bash-7-1".into(),
            command: "pnpm typecheck".into(),
            waited_secs: 21,
            peak_mb: 3000,
            need_mb: 5048,
            free_mb: 1200,
        });
        let v: serde_json::Value = serde_json::from_str(&to_line(5, &event).unwrap()).unwrap();
        assert_eq!(v["kind"], "admission_wait");
        assert_eq!(v["session"], "alpha");
        assert_eq!(v["command"], "pnpm typecheck");
        assert_eq!(v["waited_secs"], 21);
        assert_eq!(v["need_mb"], 5048);
    }

    #[test]
    fn appends_one_line_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let event = Event::Orphans(Orphans { orphans: vec![] });
        append(&path, 1, &event).unwrap();
        append(&path, 2, &event).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "{\"at\":1,\"kind\":\"orphans\",\"orphans\":[]}\n{\"at\":2,\"kind\":\"orphans\",\"orphans\":[]}\n"
        );
    }
}
