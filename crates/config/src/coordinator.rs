//! The `coordinator` section: whether `watch` may wake a coordinator, and
//! its bounds.

use crate::{Admission, Config, MAX_WAIT_SECS, load, save};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

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
    if let Some(usd) = c.max_budget_usd {
        // Claude Code refuses anything else, and the run with it.
        ensure!(
            usd.is_finite() && usd > 0.0,
            "max_budget_usd ({usd}) must be above 0"
        );
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
