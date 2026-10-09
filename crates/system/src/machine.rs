//! What the coordinator needs to know of the machine to choose the
//! configuration: memory, swap, CPUs, whether the kernel reports memory
//! pressure, and which cgroup controllers the systemd user manager hands to
//! the groups below it, the orchestrator slice and its sessions included.

use crate::cgroup;

use crate::memory;
use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// The controllers a session needs: CPU weight and caps, memory accounting
/// and limits, a process count.
const NEEDED: [&str; 3] = ["cpu", "memory", "pids"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub mem_total_mb: u64,
    pub mem_available_mb: u64,
    pub swap_total_mb: u64,
    pub swap_free_mb: u64,
    /// CPUs this process may run on.
    pub cpus: usize,
    /// `/proc/pressure/memory` exists: `watch` can arm a pressure trigger.
    pub pressure: bool,
    /// The controllers the user manager enables for its children, None when
    /// this process runs outside a systemd user manager.
    pub delegated: Option<Vec<String>>,
    /// The orchestrator slice exists: a session has been launched since boot.
    pub slice: bool,
}

/// Reads the machine from `proc_root` and `cgroup_root`.
pub fn read(proc_root: &Path, cgroup_root: &Path) -> Result<Machine> {
    let meminfo_path = proc_root.join("meminfo");
    let meminfo = fs::read_to_string(&meminfo_path)
        .with_context(|| format!("reading {}", meminfo_path.display()))?;
    let mb = |name| memory::field_kb(&meminfo, name).map(|kb| kb / 1024);
    let own = fs::read_to_string(proc_root.join("self/cgroup")).unwrap_or_default();
    let slice = cgroup::own_path(&own).and_then(cgroup::slice_path);
    let manager = slice
        .as_deref()
        .and_then(|s| Path::new(s).parent())
        .map(|m| cgroup_root.join(m.strip_prefix("/").unwrap_or(m)));
    Ok(Machine {
        mem_total_mb: mb("MemTotal").context("no MemTotal line in meminfo")?,
        mem_available_mb: mb("MemAvailable").context("no MemAvailable line in meminfo")?,
        swap_total_mb: mb("SwapTotal").unwrap_or(0),
        swap_free_mb: mb("SwapFree").unwrap_or(0),
        cpus: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        pressure: proc_root.join("pressure/memory").exists(),
        delegated: manager.as_ref().and_then(|m| {
            fs::read_to_string(m.join("cgroup.subtree_control"))
                .ok()
                .map(|c| c.split_whitespace().map(str::to_string).collect())
        }),
        slice: manager.is_some_and(|m| m.join(cgroup::SLICE).is_dir()),
    })
}

/// What `orchestrator machine` prints.
pub fn describe(m: &Machine) -> String {
    let mut out = format!(
        "Memory: {} MB total, {} MB available\nSwap: {} MB total, {} MB free\nCPUs: {}\n",
        m.mem_total_mb, m.mem_available_mb, m.swap_total_mb, m.swap_free_mb, m.cpus
    );
    let _ = writeln!(
        out,
        "Memory pressure reports (PSI): {}",
        if m.pressure {
            "available"
        } else {
            "missing, so watch reports no memory pressure"
        }
    );
    match &m.delegated {
        Some(controllers) => {
            let missing: Vec<&str> = NEEDED
                .iter()
                .copied()
                .filter(|n| !controllers.iter().any(|c| c == n))
                .collect();
            let _ = write!(
                out,
                "Controllers the systemd user manager delegates: {}",
                controllers.join(" ")
            );
            if missing.is_empty() {
                out.push('\n');
            } else {
                let _ = writeln!(
                    out,
                    " (missing {}: sessions cannot be measured or slowed down by it)",
                    missing.join(" ")
                );
            }
        }
        None => out.push_str(
            "Controllers the systemd user manager delegates: unknown, this process runs outside it\n",
        ),
    }
    let _ = writeln!(
        out,
        "{}: {}",
        cgroup::SLICE,
        if m.slice {
            "present"
        } else {
            "absent, no session launched since boot"
        }
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANAGER: &str = "user.slice/user-1000.slice/user@1000.service";

    #[test]
    fn reads_memory_swap_and_delegation() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_root = tmp.path().join("proc");
        let cgroup_root = tmp.path().join("cgroup");
        fs::create_dir_all(proc_root.join("self")).unwrap();
        fs::create_dir_all(proc_root.join("pressure")).unwrap();
        fs::write(proc_root.join("pressure/memory"), "").unwrap();
        fs::write(
            proc_root.join("meminfo"),
            "MemTotal: 32000000 kB\nMemAvailable: 16384000 kB\nSwapTotal: 4194300 kB\nSwapFree: 2097152 kB\n",
        )
        .unwrap();
        fs::write(
            proc_root.join("self/cgroup"),
            format!("0::/{MANAGER}/app.slice/term.scope\n"),
        )
        .unwrap();
        let manager = cgroup_root.join(MANAGER);
        fs::create_dir_all(manager.join(cgroup::SLICE)).unwrap();
        fs::write(manager.join("cgroup.subtree_control"), "cpu io memory\n").unwrap();

        let m = read(&proc_root, &cgroup_root).unwrap();
        assert_eq!(
            (m.mem_total_mb, m.mem_available_mb),
            (31_250, 16_000),
            "{m:?}"
        );
        assert_eq!((m.swap_total_mb, m.swap_free_mb), (4095, 2048));
        assert!(m.pressure && m.slice);
        assert_eq!(
            m.delegated.as_deref(),
            Some(&["cpu".to_string(), "io".into(), "memory".into()][..])
        );
        let text = describe(&m);
        assert!(
            text.contains("Memory: 31250 MB total, 16000 MB available"),
            "{text}"
        );
        assert!(
            text.contains("delegates: cpu io memory (missing pids"),
            "{text}"
        );
        assert!(text.contains("orchestrator.slice: present"), "{text}");
    }

    #[test]
    fn outside_a_user_manager_delegation_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_root = tmp.path();
        fs::create_dir_all(proc_root.join("self")).unwrap();
        fs::write(
            proc_root.join("meminfo"),
            "MemTotal: 2048000 kB\nMemAvailable: 1024000 kB\n",
        )
        .unwrap();
        fs::write(
            proc_root.join("self/cgroup"),
            "0::/system.slice/cron.service\n",
        )
        .unwrap();
        let m = read(proc_root, &proc_root.join("cgroup")).unwrap();
        assert_eq!(m.delegated, None);
        assert!(!m.pressure && !m.slice);
        assert_eq!((m.swap_total_mb, m.swap_free_mb), (0, 0));
        assert!(describe(&m).contains("unknown, this process runs outside it"));
    }
}
