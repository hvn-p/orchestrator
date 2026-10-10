//! The coordinator's journal: one line per action, kept by code, so that a
//! coordinator writes it in one call without reading it first. Each line
//! starts with its time; past `KEEP` lines, the oldest go.

use super::run::utc;
use anyhow::{Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// Lines kept.
pub const KEEP: usize = 200;

/// Appends `text`, one line per line of it, at `now` in seconds since the
/// Unix epoch.
pub fn note(path: &Path, text: &str, now: u64) -> Result<()> {
    let mut lines = read_lines(path)?;
    let stamp = utc(now);
    lines.extend(
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| format!("{stamp}  {l}")),
    );
    let excess = lines.len().saturating_sub(KEEP);
    lines.drain(..excess);
    let dir = path.parent().context("a journal has a directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        let _ = fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })
}

/// The last `n` lines, None when there are none.
pub fn tail(path: &Path, n: usize) -> Option<String> {
    let lines = read_lines(path).ok()?;
    let start = lines.len().saturating_sub(n);
    let tail = lines.get(start..)?;
    (!tail.is_empty()).then(|| tail.join("\n"))
}

fn read_lines(path: &Path) -> Result<Vec<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text.lines().map(str::to_string).collect()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_dated_lines_and_the_oldest_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coordinator/journal.md");
        assert_eq!(tail(&path, 5), None);
        note(
            &path,
            "asked alpha to stop its dev server\n\n  closed: beta  ",
            0,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "1970-01-01 00:00 UTC  asked alpha to stop its dev server\n1970-01-01 00:00 UTC  closed: beta\n"
        );
        for i in 0..KEEP {
            note(&path, &format!("line {i}"), 60).unwrap();
        }
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), KEEP);
        assert!(
            text.starts_with("1970-01-01 00:01 UTC  line 0\n"),
            "the oldest went"
        );
        assert_eq!(
            tail(&path, 2).unwrap(),
            format!(
                "1970-01-01 00:01 UTC  line {}\n1970-01-01 00:01 UTC  line {}",
                KEEP - 2,
                KEEP - 1
            )
        );
    }
}
