//! The configuration: the thresholds the code applies, adapted to the machine.
//! The coordinator is meant to write it on its first start; until it exists,
//! a human writes it. Without it, nothing waits.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The whole file. A section left out turns its feature off.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub admission: Option<Admission>,
}

/// When a Bash call waits for memory before it runs. Sizes in MB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

/// `$XDG_CONFIG_HOME/orchestrator/config.json`, else
/// `~/.config/orchestrator/config.json`. A relative `XDG_CONFIG_HOME` is
/// ignored, as the XDG specification asks.
pub fn default_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
    {
        return Ok(dir.join("orchestrator/config.json"));
    }
    let home = std::env::var_os("HOME").context("neither XDG_CONFIG_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".config/orchestrator/config.json"))
}

/// The configuration at `path`, None when there is no file. A file that
/// cannot be read whole is an error: an unknown or missing field is more
/// likely a typo than an intent.
pub fn load(path: &Path) -> Result<Option<Config>> {
    let text = match fs::read(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_slice(&text)
        .map(Some)
        .with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_text(text: &str) -> Result<Option<Config>> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, text).unwrap();
        load(&path)
    }

    #[test]
    fn no_file_means_no_configuration() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&dir.path().join("config.json")).unwrap(), None);
    }

    #[test]
    fn reads_the_admission_section() {
        let config = load_text(
            r#"{"admission": {"heavy_mb": 1024, "margin_mb": 2048, "max_wait_secs": 60}}"#,
        )
        .unwrap();
        assert_eq!(
            config.and_then(|c| c.admission),
            Some(Admission {
                heavy_mb: 1024,
                margin_mb: 2048,
                max_wait_secs: 60,
            })
        );
    }

    #[test]
    fn a_section_left_out_is_off() {
        assert_eq!(load_text("{}").unwrap(), Some(Config { admission: None }));
    }

    #[test]
    fn typos_are_errors() {
        assert!(load_text(r#"{"admission": {"heavy_mb": 1024, "margin_mb": 2048}}"#).is_err());
        assert!(
            load_text(
                r#"{"admission": {"heavy_mb": 1, "margin": 2, "margin_mb": 2, "max_wait_secs": 3}}"#
            )
            .is_err()
        );
        assert!(load_text(r#"{"admision": {}}"#).is_err());
        assert!(load_text("heavy_mb = 1024").is_err());
    }
}
