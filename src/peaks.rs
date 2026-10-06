//! Learned peaks: the memory each command needs, per repository, so that
//! admission can look a command up before it runs. `watch` learns from every
//! measured Bash call; the store lives under the state directory and survives
//! a reboot.
//!
//! A command's id is a SHA-256 of its repository and the recognised command.
//! Next to it, a label keeps the recognised command, shortened, for display.
//! That text is a command Claude wrote, which Claude Code already keeps in its
//! transcripts, taken before the shell expands it: a command that fetches a
//! credential holds the call, not the value. Each repository has a directory,
//! named by a hash of its key, of up to sixteen files: a command lives in the
//! one named by the first hex digit of its id, so a lookup reads a sixteenth
//! of the repository. A file starts with a line naming the repository, then
//! holds one JSON line per command, its id first: a lookup finds the
//! command's line by its id and parses that line alone. Only `watch` writes;
//! each write replaces a file whole, through a rename.
//!
//! A call holds several commands but has a single peak. That peak bounds each
//! of its commands, and measures exactly a command that ran alone. A command's
//! expected peak is therefore the smallest peak among its latest calls,
//! walking back from the latest and stopping at the latest one it ran alone
//! in. A command found in a light call is light, whatever heavy calls it also
//! appears in; one found only in heavy calls carries their peak. Only the last
//! `RECENT` calls count, so the estimate follows a command whose weight
//! changes.

use crate::jobs::Measurement;
use crate::recognise::{self, Command};
use crate::{procfs, repository};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Calls remembered per command.
const RECENT: usize = 5;
/// Commands kept per file, the most recently seen, so that a lookup stays
/// cheap: up to 2000 per repository.
const MAX_PER_FILE: usize = 125;
/// Longest label kept, in characters.
pub const LABEL_MAX: usize = 60;

/// The first line of a repository's file.
#[derive(Debug, Serialize, Deserialize)]
struct Header {
    repository: String,
}

/// What is known of one command in one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Stays the first field: a lookup finds a command's line by its start.
    pub id: String,
    /// The command as `label` shows it. An entry stored before labels holds
    /// the command's head (`procfs::command_head`) until the command is seen
    /// again.
    #[serde(alias = "head")]
    pub label: String,
    /// The expected peak: see `expected`.
    pub peak_mb: u64,
    /// When the command last ran, in seconds since the Unix epoch.
    pub last_seen: u64,
    /// Its latest calls, oldest first.
    pub recent: Vec<Call>,
}

/// One call holding a command: the call's peak, and whether the command ran
/// alone in it. Kept as `[peak_mb, alone]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call(pub u64, pub bool);

/// The commands learned in one repository.
#[derive(Debug, PartialEq, Eq)]
pub struct Learned {
    pub repository: String,
    pub commands: Vec<Entry>,
}

/// Where the peaks are kept under the state directory.
pub fn dir(state: &Path) -> PathBuf {
    state.join("peaks")
}

/// Learns from one measured Bash call: each of its commands gets the call's
/// peak. A call that cannot be parsed teaches nothing. Neither does a command
/// run where only running the call would tell, nor one whose directory does
/// not exist: the `cd` before it failed, so it did not run.
pub fn learn(dir: &Path, m: &Measurement, home: Option<&Path>) -> Result<()> {
    let cwd = Path::new(&m.cwd);
    if !cwd.is_absolute() {
        return Ok(());
    }
    let Some(commands) =
        recognise::written(&m.command).and_then(|script| recognise::commands(&script, cwd, home))
    else {
        return Ok(());
    };
    let mut unknown = 0;
    let mut placed: Vec<Placed> = Vec::new();
    for c in &commands {
        let Some(d) = c.dir.as_deref().filter(|d| d.is_dir()) else {
            unknown += 1;
            continue;
        };
        let repository = repository::key(d);
        let id = id(&repository, c);
        if !placed
            .iter()
            .any(|p| p.repository == repository && p.id == id)
        {
            placed.push(Placed {
                repository,
                id,
                command: c,
            });
        }
    }
    let call = Call(m.peak_mb, placed.len() + unknown == 1);
    let mut by_file: HashMap<PathBuf, Vec<&Placed>> = HashMap::new();
    for p in &placed {
        by_file
            .entry(file(dir, &p.repository, &p.id))
            .or_default()
            .push(p);
    }
    for (path, seen) in by_file {
        update(&path, &seen, call, m.at)?;
    }
    Ok(())
}

/// A command of a call, in its repository.
struct Placed<'a> {
    repository: PathBuf,
    id: String,
    command: &'a Command,
}

/// The expected peak of `command` in the repository keyed `repository`, if
/// learned. Reads one file and parses one line.
pub fn lookup(dir: &Path, repository: &Path, command: &Command) -> Option<u64> {
    entry(dir, repository, command).map(|e| e.peak_mb)
}

/// What is known of `command` in the repository keyed `repository`, as
/// `lookup` finds it.
pub fn entry(dir: &Path, repository: &Path, command: &Command) -> Option<Entry> {
    let id = id(repository, command);
    let text = fs::read_to_string(file(dir, repository, &id)).ok()?;
    let start = text.find(&format!("{{\"id\":\"{id}\""))?;
    let line = text.get(start..)?.lines().next()?;
    serde_json::from_str(line).ok()
}

/// Everything learned, by repository.
pub fn list(dir: &Path) -> Result<Vec<Learned>> {
    let mut learned = Vec::new();
    for repo_dir in paths(dir)? {
        let mut repository = None;
        let mut commands = Vec::new();
        for path in paths(&repo_dir)? {
            if path.extension().is_some_and(|e| e == "jsonl") {
                let (named, mut entries) = read(&path)?;
                repository = repository.or(named);
                commands.append(&mut entries);
            }
        }
        if !commands.is_empty() {
            learned.push(Learned {
                repository: repository.unwrap_or_else(|| repo_dir.display().to_string()),
                commands,
            });
        }
    }
    learned.sort_by(|a, b| a.repository.cmp(&b.repository));
    Ok(learned)
}

/// The entries of `dir`, none when it does not exist.
fn paths(dir: &Path) -> Result<Vec<PathBuf>> {
    match fs::read_dir(dir) {
        Ok(rd) => Ok(rd.flatten().map(|e| e.path()).collect()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.display())),
    }
}

/// The expected peak from a command's latest calls, oldest first: walking back
/// from the latest call, the smallest peak down to the latest call the
/// command ran alone in.
pub fn expected(recent: &[Call]) -> u64 {
    let mut peak: Option<u64> = None;
    for &Call(call_peak, alone) in recent.iter().rev() {
        peak = Some(peak.map_or(call_peak, |p| p.min(call_peak)));
        if alone {
            break;
        }
    }
    peak.unwrap_or(0)
}

/// Adds `call` to the commands `seen`, all kept in the file at `path`, then
/// rewrites it.
fn update(path: &Path, seen: &[&Placed], call: Call, at: u64) -> Result<()> {
    let Some(first) = seen.first() else {
        return Ok(());
    };
    let (_, entries) = read(path)?;
    let mut by_id: HashMap<String, Entry> =
        entries.into_iter().map(|e| (e.id.clone(), e)).collect();
    for p in seen {
        let entry = by_id.entry(p.id.clone()).or_insert_with(|| Entry {
            id: p.id.clone(),
            label: String::new(),
            peak_mb: 0,
            last_seen: 0,
            recent: Vec::new(),
        });
        entry.label = label(p.command);
        entry.recent.push(call);
        let excess = entry.recent.len().saturating_sub(RECENT);
        entry.recent.drain(..excess);
        entry.peak_mb = expected(&entry.recent);
        entry.last_seen = at;
    }
    let mut entries: Vec<Entry> = by_id.into_values().collect();
    evict(&mut entries, MAX_PER_FILE);
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    write(path, &first.repository, &entries)
}

/// Keeps the `max` most recently seen commands.
fn evict(entries: &mut Vec<Entry>, max: usize) {
    if entries.len() > max {
        entries.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then_with(|| a.id.cmp(&b.id)));
        entries.truncate(max);
    }
}

/// The repository a file's first line names, then its commands. A missing
/// file is empty; a line that cannot be read is skipped.
fn read(path: &Path) -> Result<(Option<String>, Vec<Entry>)> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok((None, Vec::new())),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut lines = text.lines();
    let repository = lines
        .next()
        .and_then(|l| serde_json::from_str::<Header>(l).ok())
        .map(|h| h.repository);
    let entries = lines.filter_map(|l| serde_json::from_str(l).ok()).collect();
    Ok((repository, entries))
}

/// Replaces the file at once: a reader sees the old content or the new one.
fn write(path: &Path, repository: &Path, entries: &[Entry]) -> Result<()> {
    let header = Header {
        repository: repository.to_string_lossy().into_owned(),
    };
    let mut text = serde_json::to_string(&header).context("serializing a peaks header")?;
    text.push('\n');
    for e in entries {
        text.push_str(&serde_json::to_string(e).context("serializing a peak")?);
        text.push('\n');
    }
    let parent = path.parent().context("a peaks file has a directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        let _ = fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })
}

/// The file holding the command `id` in the repository keyed `repository`.
fn file(dir: &Path, repository: &Path, id: &str) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(repository.as_os_str().as_bytes());
    let shard = id.get(..1).unwrap_or("0");
    dir.join(hex(&hash.finalize()))
        .join(format!("{shard}.jsonl"))
}

/// A command's id: a SHA-256 of its repository, words and input, each field
/// prefixed with its length so that two commands never share an encoding.
fn id(repository: &Path, command: &Command) -> String {
    let mut hash = Sha256::new();
    field(&mut hash, repository.as_os_str().as_bytes());
    hash.update(len(command.words.len()));
    for w in &command.words {
        field(&mut hash, w.as_bytes());
    }
    field(
        &mut hash,
        command.input.as_deref().unwrap_or_default().as_bytes(),
    );
    hex(&hash.finalize())
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update(len(bytes.len()));
    hash.update(bytes);
}

fn len(n: usize) -> [u8; 8] {
    u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// What `orchestrator peaks` shows of a command: its words on one line, a
/// mark rather than its input, at most `LABEL_MAX` characters.
fn label(command: &Command) -> String {
    let mut label = command
        .words
        .iter()
        .flat_map(|w| w.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ");
    if command.input.as_deref().is_some_and(|i| !i.is_empty()) {
        label.push_str(" <<…");
    }
    procfs::truncate(&label, LABEL_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cr3t-T0KEN";

    fn invocation(eval: &str) -> String {
        format!(
            "source /home/u/.claude/shell-snapshots/snapshot-bash-1-x.sh 2>/dev/null || true && eval {eval} && pwd -P >| /tmp/claude-ab12-cwd"
        )
    }

    /// A Bash call Claude wrote as `script`, single-quoted as Claude Code does.
    fn measured(script: &str, cwd: &Path, peak_mb: u64, at: u64) -> Measurement {
        let quoted = format!("'{}'", script.replace('\'', r#"'"'"'"#));
        Measurement {
            at,
            session: "s.scope".into(),
            job: "job-bash-7-1".into(),
            peak_mb,
            command: invocation(&quoted),
            cwd: cwd.display().to_string(),
        }
    }

    struct Store {
        _tmp: tempfile::TempDir,
        peaks: PathBuf,
        repo: PathBuf,
        key: PathBuf,
    }

    fn store() -> Store {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("app");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("web")).unwrap();
        Store {
            peaks: root.join("state/peaks"),
            key: repo.join(".git"),
            repo,
            _tmp: tmp,
        }
    }

    fn command(words: &str) -> Command {
        Command {
            words: words.split(' ').map(str::to_string).collect(),
            input: None,
            dir: None,
        }
    }

    fn learn_all(s: &Store, calls: &[(&str, u64)]) {
        for (i, (script, peak)) in calls.iter().enumerate() {
            let at = 1000 + u64::try_from(i).unwrap();
            learn(&s.peaks, &measured(script, &s.repo, *peak, at), None).unwrap();
        }
    }

    #[test]
    fn a_command_run_alone_takes_its_latest_peak() {
        assert_eq!(expected(&[Call(300, true)]), 300);
        assert_eq!(expected(&[Call(300, true), Call(500, true)]), 500);
        assert_eq!(expected(&[Call(500, true), Call(300, true)]), 300);
    }

    #[test]
    fn otherwise_the_smallest_call_peak() {
        assert_eq!(
            expected(&[Call(2000, false), Call(5, false), Call(3000, false)]),
            5
        );
        // A later call shows the command got lighter than when it ran alone.
        assert_eq!(expected(&[Call(900, true), Call(400, false)]), 400);
        // A heavier call bounds nothing below what was measured alone.
        assert_eq!(expected(&[Call(900, true), Call(4000, false)]), 900);
        // Calls before the latest alone run no longer count.
        assert_eq!(expected(&[Call(5, false), Call(900, true)]), 900);
        assert_eq!(expected(&[]), 0);
    }

    #[test]
    fn learns_two_forms_of_one_command_as_one() {
        let s = store();
        let py = "python3 -c 'b = bytearray(300 << 20)'";
        learn_all(
            &s,
            &[(py, 310), (&format!("timeout 60 {py} 2>&1 | tail -1"), 305)],
        );
        let alone = recognise::commands(py, &s.repo, None).unwrap().remove(0);
        assert_eq!(lookup(&s.peaks, &s.key, &alone), Some(305));
        let learned = list(&s.peaks).unwrap();
        assert_eq!(learned.len(), 1);
        assert_eq!(learned[0].repository, s.key.display().to_string());
        let python = learned[0]
            .commands
            .iter()
            .find(|e| e.label == "python3 -c b = bytearray(300 << 20)")
            .unwrap();
        assert_eq!(python.recent, [Call(310, true), Call(305, false)]);
        assert_eq!(python.last_seen, 1001);
    }

    #[test]
    fn the_heavy_command_carries_the_peak_of_its_calls() {
        let s = store();
        learn_all(
            &s,
            &[
                ("cd web && pnpm exec vitest run 2>&1 | grep FAIL", 2048),
                ("cat build.log | grep FAIL", 4),
            ],
        );
        assert_eq!(
            lookup(&s.peaks, &s.key, &command("pnpm exec vitest run")),
            Some(2048)
        );
        assert_eq!(lookup(&s.peaks, &s.key, &command("grep FAIL")), Some(4));
        assert_eq!(lookup(&s.peaks, &s.key, &command("cat build.log")), Some(4));
        assert_eq!(
            lookup(&s.peaks, &s.key, &command("pnpm exec vitest run x")),
            None
        );
    }

    #[test]
    fn keeps_the_latest_calls_only() {
        let s = store();
        let calls: Vec<(&str, u64)> = (1..=7).map(|i| ("make | tee log", i * 100)).collect();
        learn_all(&s, &calls);
        let make = list(&s.peaks).unwrap().remove(0).commands.remove(0);
        assert_eq!(make.recent.len(), RECENT);
        assert_eq!(make.peak_mb, 300);
    }

    #[test]
    fn worktrees_and_repositories_are_told_apart() {
        let s = store();
        let other = s.repo.parent().unwrap().join("other");
        fs::create_dir_all(other.join(".git")).unwrap();
        learn(&s.peaks, &measured("make", &s.repo, 100, 1), None).unwrap();
        learn(
            &s.peaks,
            &measured("cd ../other && make", &s.repo, 900, 2),
            None,
        )
        .unwrap();
        assert_eq!(lookup(&s.peaks, &s.key, &command("make")), Some(100));
        assert_eq!(
            lookup(&s.peaks, &other.join(".git"), &command("make")),
            Some(900)
        );
        assert_eq!(list(&s.peaks).unwrap().len(), 2);
    }

    fn labels(s: &Store) -> Vec<String> {
        let mut labels: Vec<String> = list(&s.peaks)
            .unwrap()
            .into_iter()
            .flat_map(|l| l.commands)
            .map(|e| e.label)
            .collect();
        labels.sort();
        labels
    }

    #[test]
    fn labels_show_the_command_as_recognised() {
        let s = store();
        learn_all(
            &s,
            &[
                (
                    "cd web && NODE_ENV=test timeout 300 pnpm exec vitest run 2>&1 | tail -1",
                    900,
                ),
                (&format!("python3 - <<'PY'\nprint('{SECRET}')\nPY"), 20),
                (
                    "python3 -c 'import sys\nfor line in sys.stdin:\n    print(line.upper())' < data.txt",
                    30,
                ),
                (
                    r#"curl -H "Authorization: Bearer $(cat ~/.token)" https://x"#,
                    10,
                ),
            ],
        );
        assert_eq!(
            labels(&s),
            [
                // A credential fetched by the command shows as the call.
                "curl -H Authorization: Bearer $(cat ~/.token) https://x",
                "pnpm exec vitest run",
                "python3 - <<…",
                "python3 -c import sys for line in sys.stdin: print(line.upp…",
                "tail -1",
            ]
        );
    }

    #[test]
    fn an_entry_stored_with_a_head_shows_it_until_seen_again() {
        let s = store();
        let make = command("make test");
        let id = id(&s.key, &make);
        let path = file(&s.peaks, &s.key, &id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{{\"repository\":\"{}\"}}\n{{\"id\":\"{id}\",\"head\":\"make\",\"peak_mb\":40,\"last_seen\":1,\"recent\":[[40,true]]}}\n",
                s.key.display()
            ),
        )
        .unwrap();
        assert_eq!(lookup(&s.peaks, &s.key, &make), Some(40));
        assert_eq!(labels(&s), ["make"]);
        learn_all(&s, &[("make test", 50)]);
        assert_eq!(labels(&s), ["make test"]);
    }

    #[test]
    fn calls_that_teach_nothing() {
        let s = store();
        learn(&s.peaks, &measured("echo 'open", &s.repo, 1, 1), None).unwrap();
        learn(
            &s.peaks,
            &measured("cd \"$D\" && make", &s.repo, 1, 1),
            None,
        )
        .unwrap();
        let mut relative = measured("make", &s.repo, 1, 1);
        relative.cwd = String::new();
        learn(&s.peaks, &relative, None).unwrap();
        // The cd failed, so the command after it did not run.
        learn(
            &s.peaks,
            &measured("cd ../typo && make", &s.repo, 0, 1),
            None,
        )
        .unwrap();
        assert_eq!(list(&s.peaks).unwrap(), Vec::new());
    }

    #[test]
    fn a_command_beside_an_unknown_one_did_not_run_alone() {
        let s = store();
        learn_all(&s, &[("make; cd \"$D\" && make", 700)]);
        let make = list(&s.peaks).unwrap().remove(0).commands.remove(0);
        assert_eq!(make.recent, [Call(700, false)]);
        // The same command twice is still alone.
        learn_all(&s, &[("make && make", 800)]);
        let make = list(&s.peaks).unwrap().remove(0).commands.remove(0);
        assert_eq!(make.recent, [Call(700, false), Call(800, true)]);
    }

    #[test]
    fn writes_replace_the_file_and_skip_unreadable_lines() {
        let s = store();
        learn_all(&s, &[("make", 100)]);
        let path = file(&s.peaks, &s.key, &id(&s.key, &command("make")));
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{not json\n");
        fs::write(&path, text).unwrap();
        learn_all(&s, &[("make test", 200)]);
        assert_eq!(lookup(&s.peaks, &s.key, &command("make")), Some(100));
        assert_eq!(lookup(&s.peaks, &s.key, &command("make test")), Some(200));
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| n.to_string_lossy().ends_with(".jsonl")),
            "no temporary file left: {names:?}"
        );
    }

    #[test]
    fn evicts_the_least_recently_seen() {
        let entry = |id: &str, last_seen| Entry {
            id: id.into(),
            label: String::new(),
            peak_mb: 1,
            last_seen,
            recent: vec![Call(1, true)],
        };
        let mut entries = vec![entry("a", 5), entry("b", 9), entry("c", 1), entry("d", 9)];
        evict(&mut entries, 2);
        let kept: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(kept, ["b", "d"]);
    }

    #[test]
    fn ids_are_stable_and_unambiguous() {
        let repo = Path::new("/r/.git");
        // The store depends on this value across releases; computed
        // independently from the encoding described on `id`.
        assert_eq!(
            id(repo, &command("make test")),
            "06856035d84a642d06e33b449aade1c7c92cafec1dcab0ce32d0768ea47f4e3b"
        );
        assert_ne!(
            id(repo, &command("make test")),
            id(repo, &command("maket est"))
        );
        let mut fed = command("make test");
        fed.input = Some(String::new());
        assert_eq!(id(repo, &fed), id(repo, &command("make test")));
        fed.input = Some("x".into());
        assert_ne!(id(repo, &fed), id(repo, &command("make test")));
    }
}
