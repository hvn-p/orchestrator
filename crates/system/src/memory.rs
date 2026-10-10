//! Machine-wide available memory, as the kernel estimates it.

use anyhow::{Context, Result};
use std::path::Path;

/// `MemAvailable` from a meminfo file, in MB.
pub fn available_mb(meminfo: &Path) -> Result<u64> {
    let text = std::fs::read_to_string(meminfo)
        .with_context(|| format!("reading {}", meminfo.display()))?;
    parse_available_kb(&text)
        .map(|kb| kb / 1024)
        .with_context(|| format!("no MemAvailable line in {}", meminfo.display()))
}

fn parse_available_kb(meminfo: &str) -> Option<u64> {
    field_kb(meminfo, "MemAvailable")
}

/// The value of the meminfo line `name`, in kB.
pub fn field_kb(meminfo: &str, name: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_mem_available() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meminfo");
        std::fs::write(
            &path,
            "MemTotal:  32000000 kB\nMemFree: 1 kB\nMemAvailable:    8192000 kB\n",
        )
        .unwrap();
        assert_eq!(available_mb(&path).unwrap(), 8000);
    }

    #[test]
    fn fails_without_mem_available() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meminfo");
        std::fs::write(&path, "MemTotal: 1 kB\n").unwrap();
        assert!(available_mb(&path).is_err());
    }
}
