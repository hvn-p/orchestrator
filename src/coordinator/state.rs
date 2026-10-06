//! What a coordinator is told when it wakes. Gathering the state takes no
//! judgment, so code does it, and the coordinator starts from it instead of
//! running the read commands one by one: the machine, the sessions, the
//! admission's waiting calls and reservations, the heavy commands learned,
//! the recent part of its journal and the user's priorities. The same text
//! as the read commands print, which it may still run for more.

use super::{Paths, journal};
use crate::{admission, cgroup, config, machine, report, runtime};
use anyhow::Result;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Journal lines a coordinator is shown.
const JOURNAL_LINES: usize = 40;
/// The longest priorities a coordinator is shown, in characters.
const PRIORITIES_MAX: usize = 8000;
/// Learned commands shown, the heaviest.
const PEAKS_MAX: usize = 15;

/// Where the state is read.
#[derive(Debug, Clone)]
pub struct Places {
    pub proc_root: PathBuf,
    pub sessions_dir: PathBuf,
    pub admission: admission::Paths,
    pub state_dir: PathBuf,
    pub config: PathBuf,
}

impl Places {
    /// The places the read commands use.
    pub fn from_env() -> Result<Places> {
        let proc_root = PathBuf::from("/proc");
        Ok(Places {
            sessions_dir: crate::sessions::default_dir()
                .ok_or_else(|| anyhow::anyhow!("neither CLAUDE_CONFIG_DIR nor HOME is set"))?,
            admission: admission::Paths {
                cgroup_root: PathBuf::from(cgroup::ROOT),
                meminfo: proc_root.join("meminfo"),
                runtime: runtime::default_dir()?,
            },
            proc_root,
            state_dir: crate::state::default_dir()?,
            config: config::default_path()?,
        })
    }
}

/// What a coordinator starts from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Briefing {
    /// The machine's state, in sections.
    pub state: String,
    /// The recent part of its journal.
    pub journal: Option<String>,
    pub priorities: Option<String>,
}

/// The briefing of a coordinator waking now.
pub fn briefing(places: &Places, paths: &Paths) -> Briefing {
    Briefing {
        state: gather(places),
        journal: journal::tail(&paths.journal(), JOURNAL_LINES),
        priorities: fs::read_to_string(&paths.priorities)
            .ok()
            .map(|p| p.chars().take(PRIORITIES_MAX).collect::<String>())
            .filter(|p| !p.trim().is_empty()),
    }
}

/// The machine's state now, one section per read command. A section that
/// cannot be read says why.
pub fn gather(places: &Places) -> String {
    let heavy_mb = config::load(&places.config)
        .ok()
        .flatten()
        .and_then(|c| c.admission)
        .map(|a| a.heavy_mb);
    let peaks_title = match heavy_mb {
        Some(mb) => {
            format!("Commands learned at {mb} MB or more, the heaviest (`orchestrator peaks`)")
        }
        None => "Commands learned, the heaviest (`orchestrator peaks`)".to_string(),
    };
    let sections = [
        (
            "Configuration (`orchestrator config`)".to_string(),
            report::config(&places.config),
        ),
        (
            "Machine (`orchestrator machine`)".to_string(),
            machine::read(&places.proc_root, &places.admission.cgroup_root)
                .map(|m| machine::describe(&m)),
        ),
        (
            "Sessions (`orchestrator sessions --heads`)".to_string(),
            report::sessions(&places.proc_root, &places.sessions_dir, true),
        ),
        (
            "Admission (`orchestrator admission`)".to_string(),
            report::admission(&places.admission, &places.config, &places.sessions_dir),
        ),
        (
            peaks_title,
            report::peaks(&places.state_dir, heavy_mb, Some(PEAKS_MAX)),
        ),
    ];
    let mut out = String::new();
    for (title, text) in sections {
        let text = text.unwrap_or_else(|e| format!("unavailable: {e:#}\n"));
        let _ = write!(out, "### {title}\n\n{}\n", text.trim_end());
        out.push('\n');
    }
    out
}

/// Where the coordinator's runtime files are, for `places`.
pub fn paths(places: &Places) -> Paths {
    Paths::new(&places.admission.runtime, &places.state_dir, &places.config)
}

/// A file a briefing names that does not exist yet.
pub fn missing(path: &Path) -> &'static str {
    if path.exists() {
        ""
    } else {
        " (does not exist yet)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_briefing_holds_every_section_even_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let proc_root = base.join("proc");
        fs::create_dir_all(proc_root.join("self")).unwrap();
        fs::write(
            proc_root.join("meminfo"),
            "MemTotal: 32000000 kB\nMemAvailable: 8192000 kB\n",
        )
        .unwrap();
        let places = Places {
            proc_root: proc_root.clone(),
            sessions_dir: base.join("sessions"),
            admission: admission::Paths {
                cgroup_root: base.join("cgroup"),
                meminfo: proc_root.join("meminfo"),
                runtime: base.join("run"),
            },
            state_dir: base.join("state"),
            config: base.join("config/config.json"),
        };
        let paths = paths(&places);
        journal::note(&paths.journal(), "asked alpha", 0).unwrap();
        fs::create_dir_all(base.join("config")).unwrap();
        fs::write(&paths.priorities, "beta matters most\n").unwrap();
        let b = briefing(&places, &paths);
        for section in [
            "### Configuration",
            "### Machine",
            "### Sessions",
            "### Admission",
            "### Commands learned",
        ] {
            assert!(b.state.contains(section), "{section} in {}", b.state);
        }
        assert!(
            b.state
                .contains("Memory: 31250 MB total, 8000 MB available")
        );
        assert!(b.state.contains("No call waits for memory."));
        assert!(b.state.contains("No peak learned yet."));
        assert_eq!(
            b.journal.as_deref(),
            Some("1970-01-01 00:00 UTC  asked alpha")
        );
        assert_eq!(b.priorities.as_deref(), Some("beta matters most\n"));
    }
}
