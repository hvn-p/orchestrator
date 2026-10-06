//! Where orchestrator keeps what it learns: on disk, kept across reboots,
//! never versioned.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// `$XDG_STATE_HOME/orchestrator`, else `~/.local/state/orchestrator`. A
/// relative `XDG_STATE_HOME` is ignored, as the XDG specification asks.
pub fn default_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
    {
        return Ok(dir.join("orchestrator"));
    }
    let home = std::env::var_os("HOME").context("neither XDG_STATE_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".local/state/orchestrator"))
}
