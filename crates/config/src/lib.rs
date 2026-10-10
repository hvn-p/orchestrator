//! The configuration: the thresholds the code applies, adapted to the machine.
//! The coordinator writes it through the validated `orchestrator config`
//! commands, in a setup conversation with the user; a human may write it
//! too. Without admission thresholds, nothing waits. Without `wake` in the
//! `coordinator` section, `watch` starts no coordinator: nothing spends
//! tokens unless the user opens one.

mod admission;
mod coordinator;
pub mod language;

pub use admission::{Admission, MAX_WAIT_SECS, check_admission, set_admission};
pub use coordinator::{
    Coordinator, DEFAULT_MODEL, MAX_MINUTES, check_coordinator, set_coordinator,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The whole file. A section left out turns its feature off.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<Admission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<Coordinator>,
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
/// file or the section is missing, `wake` is off, or the file cannot be read
/// or holds a section that makes no sense, which only a hand-written file
/// can: without a readable consent, nothing spends tokens, and a run that
/// could only fail does not start.
pub fn waking(path: &Path) -> Option<Coordinator> {
    let section = load(path).and_then(|config| {
        let section = config.and_then(|c| c.coordinator).filter(|c| c.wake);
        if let Some(c) = &section {
            check_coordinator(c, None)?;
        }
        Ok(section)
    });
    match section {
        Ok(section) => section,
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
        // A hand-written section that would only make runs fail.
        fs::write(
            &path,
            r#"{"coordinator": {"wake": true, "model": "haiku", "max_minutes": 5, "wait_secs": 20, "max_budget_usd": 0}}"#,
        )
        .unwrap();
        assert_eq!(waking(&path), None);
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
        let budget = |usd| Coordinator {
            max_budget_usd: Some(usd),
            ..ok.clone()
        };
        assert!(check_coordinator(&budget(0.5), None).is_ok());
        assert!(check_coordinator(&budget(0.0), None).is_err());
        assert!(check_coordinator(&budget(-1.0), None).is_err());
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
