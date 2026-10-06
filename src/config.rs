//! The configuration: the thresholds the code applies, adapted to the machine.
//! The coordinator writes the admission thresholds, on its first start
//! through `orchestrator setup` or later on request; a human may write them
//! too. Without them, nothing waits. The `coordinator` section is the user's
//! consent to start coordinators: without it, nothing spends tokens.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The longest admission wait a configuration may hold: Claude Code stops
/// waiting for a Bash call after 10 minutes at most, the wait included.
pub const MAX_WAIT_SECS: u64 = 600;

/// The whole file. A section left out turns its feature off.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<Admission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<Coordinator>,
}

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

/// The coordinator: a Claude Code session `watch` starts for each batch of
/// events that need judgment. Its presence is the consent to spend tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coordinator {
    /// The model of the runs `watch` and `setup` start.
    pub model: String,
    /// The most one run may spend, in USD, as Claude Code estimates it.
    pub max_budget_usd: f64,
    /// The longest one run may last; then `watch` stops it.
    pub max_minutes: u64,
    /// A Bash call that admission has held back this long wakes the
    /// coordinator, once.
    pub wait_secs: u64,
}

impl Default for Coordinator {
    /// What `orchestrator setup` writes when the section is missing: a small
    /// model and small bounds, raised by hand when needed.
    fn default() -> Self {
        Coordinator {
            model: "haiku".into(),
            max_budget_usd: 0.25,
            max_minutes: 5,
            wait_secs: 20,
        }
    }
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

/// The user's priorities for the coordinator, next to the configuration: a
/// text the user writes, or asks an interactive coordinator to.
pub fn priorities_path(config: &Path) -> PathBuf {
    config.with_file_name("priorities.md")
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

/// The coordinator section, None when the file or the section is missing or
/// the file cannot be read: without a readable consent, nothing spends tokens.
pub fn coordinator(path: &Path) -> Option<Coordinator> {
    match load(path) {
        Ok(config) => config.and_then(|c| c.coordinator),
        Err(e) => {
            eprintln!("orchestrator: {e:#}; no coordinator starts");
            None
        }
    }
}

/// Replaces the file at once: the prefix reads the old content or the new.
pub fn save(path: &Path, config: &Config) -> Result<()> {
    let mut text = serde_json::to_string_pretty(config).context("serializing the configuration")?;
    text.push('\n');
    let dir = path
        .parent()
        .context("a configuration file has a directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        let _ = fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })
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
/// once the values make sense on a machine of `total_mb`. A file that exists
/// but cannot be read is left alone.
pub fn set_admission(path: &Path, admission: Admission, total_mb: u64) -> Result<Config> {
    check_admission(&admission, total_mb)?;
    let mut config = load(path)?.unwrap_or_default();
    config.admission = Some(admission);
    save(path, &config)?;
    Ok(config)
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

    const ADMISSION: Admission = Admission {
        heavy_mb: 1024,
        margin_mb: 2048,
        max_wait_secs: 60,
    };

    #[test]
    fn no_file_means_no_configuration() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&dir.path().join("config.json")).unwrap(), None);
        assert_eq!(coordinator(&dir.path().join("config.json")), None);
    }

    #[test]
    fn reads_the_admission_section() {
        let config = load_text(
            r#"{"admission": {"heavy_mb": 1024, "margin_mb": 2048, "max_wait_secs": 60}}"#,
        )
        .unwrap();
        assert_eq!(config.and_then(|c| c.admission), Some(ADMISSION));
    }

    #[test]
    fn reads_the_coordinator_section() {
        let config = load_text(
            r#"{"coordinator": {"model": "haiku", "max_budget_usd": 0.5, "max_minutes": 3, "wait_secs": 15}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            config.coordinator,
            Some(Coordinator {
                model: "haiku".into(),
                max_budget_usd: 0.5,
                max_minutes: 3,
                wait_secs: 15,
            })
        );
        assert_eq!(config.admission, None);
    }

    #[test]
    fn a_section_left_out_is_off() {
        assert_eq!(load_text("{}").unwrap(), Some(Config::default()));
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
        assert!(load_text(r#"{"coordinator": {"model": "haiku"}}"#).is_err());
        assert!(load_text("heavy_mb = 1024").is_err());
    }

    #[test]
    fn an_unreadable_file_starts_no_coordinator() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"coordinator": {"model": "haiku"}}"#).unwrap();
        assert_eq!(coordinator(&path), None);
    }

    #[test]
    fn admission_values_must_fit_the_machine() {
        assert!(check_admission(&ADMISSION, 32_000).is_ok());
        let with = |heavy_mb, margin_mb, max_wait_secs| Admission {
            heavy_mb,
            margin_mb,
            max_wait_secs,
        };
        assert!(check_admission(&with(0, 2048, 60), 32_000).is_err());
        assert!(check_admission(&with(40_000, 2048, 60), 32_000).is_err());
        assert!(check_admission(&with(1024, 32_000, 60), 32_000).is_err());
        assert!(check_admission(&with(1024, 2048, 601), 32_000).is_err());
        assert!(check_admission(&with(1024, 0, 0), 32_000).is_ok());
    }

    #[test]
    fn setting_admission_keeps_the_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orchestrator/config.json");
        let coordinator = Config {
            admission: None,
            coordinator: Some(Coordinator::default()),
        };
        save(&path, &coordinator).unwrap();
        set_admission(&path, ADMISSION, 32_000).unwrap();
        let back = load(&path).unwrap().unwrap();
        assert_eq!(back.admission, Some(ADMISSION));
        assert_eq!(back.coordinator, Some(Coordinator::default()));
        // A rejected value changes nothing.
        let wrong = Admission {
            heavy_mb: 0,
            ..ADMISSION
        };
        assert!(set_admission(&path, wrong, 32_000).is_err());
        assert_eq!(load(&path).unwrap().unwrap(), back);
        // Nor does a file that cannot be read.
        fs::write(&path, "{broken").unwrap();
        assert!(set_admission(&path, ADMISSION, 32_000).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.json"], "no temporary file left");
    }
}
