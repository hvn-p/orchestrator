//! The `admission` section: when a Bash call waits for memory.

use crate::{Config, check_coordinator, load, save};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The longest admission wait a configuration may hold: Claude Code stops
/// waiting for a Bash call after 10 minutes at most, the wait included.
pub const MAX_WAIT_SECS: u64 = 600;

/// When a Bash call waits for memory before it runs. Sizes in MB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admission {
    /// A call whose expected peak reaches this waits until memory covers it.
    pub heavy_mb: u64,
    /// Free memory kept on top of a heavy call's expected peak.
    pub margin_mb: u64,
    /// The longest a call waits; then it runs anyway.
    // claude-code: bash-call-timeout
    pub max_wait_secs: u64,
}

/// Admission thresholds that make sense on a machine of `total_mb`.
pub fn check_admission(a: &Admission, total_mb: u64) -> Result<()> {
    ensure!(
        a.heavy_mb > 0,
        "heavy_mb must be above 0: every call would wait"
    );
    ensure!(
        a.heavy_mb <= total_mb,
        "heavy_mb ({} MB) exceeds the machine's memory ({total_mb} MB): no call would ever wait",
        a.heavy_mb
    );
    ensure!(
        a.margin_mb < total_mb,
        "margin_mb ({} MB) leaves nothing of the machine's memory ({total_mb} MB)",
        a.margin_mb
    );
    if a.max_wait_secs > MAX_WAIT_SECS {
        bail!(
            "max_wait_secs ({}) exceeds the longest a Bash call can last, {MAX_WAIT_SECS} s",
            a.max_wait_secs
        );
    }
    Ok(())
}

/// Sets the admission section of the file at `path`, keeping the others,
/// once the values make sense on a machine of `total_mb` and with the
/// coordinator section. A file that exists but cannot be read is left alone.
pub fn set_admission(path: &Path, admission: Admission, total_mb: u64) -> Result<Config> {
    check_admission(&admission, total_mb)?;
    let mut config = load(path)?.unwrap_or_default();
    if let Some(c) = &config.coordinator {
        check_coordinator(c, Some(&admission))?;
    }
    config.admission = Some(admission);
    save(path, &config)?;
    Ok(config)
}
