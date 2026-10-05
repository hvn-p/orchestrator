//! Where orchestrator keeps what it produces while running: in memory, cleared
//! at reboot, never versioned.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// `$XDG_RUNTIME_DIR/orchestrator`, else `/run/user/<uid>/orchestrator`.
pub fn default_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir).join("orchestrator"));
    }
    let status = std::fs::read_to_string("/proc/self/status").context("reading own uid")?;
    let uid = status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|v| v.split_whitespace().next())
        .context("no Uid line in /proc/self/status")?;
    Ok(Path::new("/run/user").join(uid).join("orchestrator"))
}
