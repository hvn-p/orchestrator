//! The `admission` section: when a Bash call waits for memory, and how
//! long before it is refused.

use crate::{Config, check_coordinator, load, save};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The longest foreground wait a configuration may hold: under a Bash
/// call's default timeout, past which the call moves to the background and
/// its refusal no longer reaches Claude with its result (#16).
pub const MAX_WAIT_SECS: u64 = claude_code::call::DEFAULT_TIMEOUT_SECS - 1;

/// The longest background wait a configuration may hold: the longest a
/// command runs in the background in a session that runs unattended.
pub const MAX_BACKGROUND_WAIT_SECS: u64 = claude_code::call::BACKGROUND_LIMIT_SECS;

/// When a Bash call waits for memory before it runs. Sizes in MB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admission {
    /// A call whose expected peak reaches this waits until memory covers it.
    pub heavy_mb: u64,
    /// Free memory kept on top of a heavy call's expected peak.
    pub margin_mb: u64,
    /// The longest a call waits in the foreground; then it is refused.
    pub max_wait_secs: u64,
    /// The longest a call run in the background to wait longer waits; then
    /// it is refused.
    pub max_background_wait_secs: u64,
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
    ensure!(
        a.max_wait_secs <= MAX_WAIT_SECS,
        "max_wait_secs ({}) must stay under a Bash call's default timeout, {} s: past it, the call moves to the background before its refusal reaches Claude",
        a.max_wait_secs,
        claude_code::call::DEFAULT_TIMEOUT_SECS
    );
    ensure!(
        a.max_background_wait_secs > a.max_wait_secs,
        "max_background_wait_secs ({}) must exceed max_wait_secs ({}): it is how a refused call waits longer",
        a.max_background_wait_secs,
        a.max_wait_secs
    );
    ensure!(
        a.max_background_wait_secs <= MAX_BACKGROUND_WAIT_SECS,
        "max_background_wait_secs ({}) exceeds the longest a command runs in the background in an unattended session, {MAX_BACKGROUND_WAIT_SECS} s",
        a.max_background_wait_secs
    );
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
