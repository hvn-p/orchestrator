//! The configuration: the thresholds the code applies, adapted to the machine.
//! The coordinator writes it through the validated `orchestrator config`
//! commands, in a setup conversation with the user; a human may write it
//! too. Without admission thresholds, nothing waits. Without `wake` in the
//! `coordinator` section, `watch` starts no coordinator: nothing spends
//! tokens unless the user opens one.

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

/// The model a coordinator runs with unless the user chooses another.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

/// The longest run a configuration may allow, in minutes.
pub const MAX_MINUTES: u64 = 60;

/// The coordinator: a Claude Code session `watch` starts for each batch of
/// events that need judgment, once the user has agreed to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coordinator {
    /// `watch` may start a coordinator by itself for events: the user's
    /// consent to spend tokens without being asked.
    pub wake: bool,
    /// The model of the runs `watch` starts: the user's choice.
    pub model: String,
    /// The most one run may spend, in USD at list price as Claude Code
    /// estimates it; none by default. The estimate is not what a
    /// subscription counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    /// The longest one run may last; then `watch` stops it. A guard against
    /// a stuck run, not a spending limit.
    pub max_minutes: u64,
    /// A Bash call that admission has held back this long wakes the
    /// coordinator, once.
    pub wait_secs: u64,
    /// The language a coordinator writes in, a tag such as `fr` or `pt-BR`
    /// (see `language`): chosen in the setup conversation, English when
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl Default for Coordinator {
    /// The values a new section starts from: no waking until the user says
    /// so.
    fn default() -> Self {
        Coordinator {
            wake: false,
            model: DEFAULT_MODEL.into(),
            max_budget_usd: None,
            max_minutes: 5,
            wait_secs: 20,
            language: None,
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

/// The user's instructions for coordinators, next to the configuration: a
/// CLAUDE.md that only coordinators read (see `coordinator::instructions`).
pub fn instructions_path(config: &Path) -> PathBuf {
    config.with_file_name("CLAUDE.md")
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

/// The coordinator section if `watch` may wake coordinators, None when the
/// file or the section is missing, `wake` is off or the file cannot be read:
/// without a readable consent, nothing spends tokens.
pub fn waking(path: &Path) -> Option<Coordinator> {
    match load(path) {
        Ok(config) => config.and_then(|c| c.coordinator).filter(|c| c.wake),
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

/// A coordinator section that makes sense, with the admission thresholds
/// it goes with, if any.
pub fn check_coordinator(c: &Coordinator, admission: Option<&Admission>) -> Result<()> {
    let model_chars = c
        .model
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "._-[]".contains(ch));
    ensure!(
        !c.model.is_empty() && c.model.len() <= 100 && model_chars,
        "model {:?} is not a model name or alias, such as {DEFAULT_MODEL}, sonnet or haiku",
        c.model
    );
    if let Some(tag) = &c.language {
        crate::language::check(tag)?;
    }
    ensure!(
        (1..=MAX_MINUTES).contains(&c.max_minutes),
        "max_minutes ({}) must be between 1 and {MAX_MINUTES}",
        c.max_minutes
    );
    ensure!(
        (1..=MAX_WAIT_SECS).contains(&c.wait_secs),
        "wait_secs ({}) must be between 1 and {MAX_WAIT_SECS}",
        c.wait_secs
    );
    if let Some(a) = admission.filter(|_| c.wake) {
        ensure!(
            c.wait_secs < a.max_wait_secs,
            "wait_secs ({}) must be under admission's max_wait_secs ({}): a call waits no longer, so it would never wake the coordinator",
            c.wait_secs,
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

/// Changes the coordinator section of the file at `path` with `change`,
/// starting from the defaults when there is none, once it makes sense.
pub fn set_coordinator(path: &Path, change: impl FnOnce(&mut Coordinator)) -> Result<Config> {
    let mut config = load(path)?.unwrap_or_default();
    let mut c = config.coordinator.clone().unwrap_or_default();
    change(&mut c);
    check_coordinator(&c, config.admission.as_ref())?;
    config.coordinator = Some(c);
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
        assert_eq!(waking(&dir.path().join("config.json")), None);
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
            r#"{"coordinator": {"wake": true, "model": "haiku", "max_budget_usd": 0.5, "max_minutes": 3, "wait_secs": 15}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            config.coordinator,
            Some(Coordinator {
                wake: true,
                model: "haiku".into(),
                max_budget_usd: Some(0.5),
                max_minutes: 3,
                wait_secs: 15,
                language: None,
            })
        );
        assert_eq!(config.admission, None);
        // No budget cap unless one is written.
        let config = load_text(
            r#"{"coordinator": {"wake": false, "model": "claude-sonnet-5-5", "max_minutes": 5, "wait_secs": 20}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.coordinator, Some(Coordinator::default()));
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
    fn only_a_readable_consent_wakes_coordinators() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{"coordinator": {"wake": true, "model": "haiku"}}"#,
        )
        .unwrap();
        assert_eq!(waking(&path), None, "unreadable");
        set_coordinator(&path, |c| c.model = "haiku".into()).unwrap_err();
        fs::remove_file(&path).unwrap();
        set_coordinator(&path, |c| c.model = "haiku".into()).unwrap();
        assert_eq!(waking(&path), None, "configured, not agreed to");
        set_coordinator(&path, |c| c.wake = true).unwrap();
        assert_eq!(waking(&path).map(|c| c.model), Some("haiku".into()));
    }

    #[test]
    fn coordinator_values_must_make_sense() {
        let ok = Coordinator {
            wake: true,
            ..Coordinator::default()
        };
        assert!(check_coordinator(&ok, Some(&ADMISSION)).is_ok());
        for model in [
            "claude-sonnet-5-5",
            "sonnet",
            "opus[1m]",
            "claude-haiku-4.5",
        ] {
            let c = Coordinator {
                model: model.into(),
                ..ok.clone()
            };
            assert!(check_coordinator(&c, None).is_ok(), "{model}");
        }
        for model in ["", "sonnet please", "x;rm -rf /"] {
            let c = Coordinator {
                model: model.into(),
                ..ok.clone()
            };
            assert!(check_coordinator(&c, None).is_err(), "{model:?}");
        }
        let speaking = |tag: &str| Coordinator {
            language: Some(tag.into()),
            ..ok.clone()
        };
        assert!(check_coordinator(&speaking("fr"), None).is_ok());
        assert!(check_coordinator(&speaking("French"), None).is_err());
        let minutes = |max_minutes| Coordinator {
            max_minutes,
            ..ok.clone()
        };
        assert!(check_coordinator(&minutes(0), None).is_err());
        assert!(check_coordinator(&minutes(61), None).is_err());
        // A call never waits longer than admission lets it.
        let wait = |wait_secs| Coordinator {
            wait_secs,
            ..ok.clone()
        };
        assert!(check_coordinator(&wait(59), Some(&ADMISSION)).is_ok());
        assert!(check_coordinator(&wait(60), Some(&ADMISSION)).is_err());
        assert!(check_coordinator(&wait(60), None).is_ok());
        let asleep = Coordinator {
            wake: false,
            ..wait(60)
        };
        assert!(check_coordinator(&asleep, Some(&ADMISSION)).is_ok());
    }

    #[test]
    fn admission_and_coordinator_are_checked_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        set_coordinator(&path, |c| {
            c.wake = true;
            c.wait_secs = 45;
        })
        .unwrap();
        let short = Admission {
            max_wait_secs: 30,
            ..ADMISSION
        };
        let refused = set_admission(&path, short, 32_000).unwrap_err();
        assert!(
            format!("{refused:#}").contains("wait_secs (45)"),
            "{refused:#}"
        );
        set_admission(&path, ADMISSION, 32_000).unwrap();
        set_coordinator(&path, |c| c.wait_secs = 90).unwrap_err();
        assert_eq!(
            load(&path).unwrap().unwrap().coordinator.unwrap().wait_secs,
            45
        );
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
