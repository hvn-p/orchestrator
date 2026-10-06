//! The code and docs/claude-code-dependency.md agree on what orchestrator
//! relies on in Claude Code. Each marker comment names a contract the
//! document lists, each contract has a marker, and each contract's Code line
//! lists exactly the files holding its markers. See the document's "Keeping
//! it current".

// Clippy exempts only `#[test]` functions; every function here is test code.
#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const DOC: &str = "docs/claude-code-dependency.md";
/// Where markers live, from the crate root.
const SOURCES: [&str; 2] = ["src", "tests"];
/// What a marker comment starts with, the contract's id following.
const MARKER: &str = "// claude-code: ";
/// What a comment that means to be a marker holds.
const MARKER_WORD: &str = "claude-code:";
/// The line the check-claude-code skill reads and updates.
const LAST_VERIFIED: &str = "Last verified: Claude Code ";

/// Files by contract id.
type Places = BTreeMap<String, BTreeSet<String>>;

#[test]
fn markers_and_contracts_agree() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut problems = Vec::new();
    let marked = markers(root, &mut problems);
    let doc = fs::read_to_string(root.join(DOC)).expect("reading the dependency document");
    let listed = contracts(&doc, &mut problems);
    for (id, files) in &marked {
        match listed.get(id) {
            None => problems.push(format!(
                "unknown contract `{id}`, marked in {}: add it to {DOC}",
                join(files)
            )),
            Some(code) if code != files => problems.push(format!(
                "contract `{id}`: its Code line lists {}, its markers are in {}",
                join(code),
                join(files)
            )),
            Some(_) => {}
        }
    }
    for id in listed.keys().filter(|id| !marked.contains_key(*id)) {
        problems.push(format!(
            "contract `{id}` has no marker: mark the code relying on it, or remove it from {DOC}"
        ));
    }
    let verified = doc.lines().filter(|l| l.starts_with(LAST_VERIFIED)).count();
    if verified != 1 {
        problems.push(format!(
            "{DOC} holds {verified} lines starting with \"{LAST_VERIFIED}\", not one"
        ));
    }
    assert!(
        problems.is_empty(),
        "the code and {DOC} disagree:\n- {}",
        problems.join("\n- ")
    );
}

/// The files holding a marker, by contract id. A comment that mentions the
/// marker word without being a marker is a problem.
fn markers(root: &Path, problems: &mut Vec<String>) -> Places {
    let mut files = Vec::new();
    for dir in SOURCES {
        rust_files(&root.join(dir), &mut files);
    }
    let mut found = Places::new();
    for path in files {
        let text = fs::read_to_string(&path).expect("reading a source file");
        let name = path
            .strip_prefix(root)
            .expect("a file under the crate root")
            .to_string_lossy()
            .replace('\\', "/");
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if let Some(id) = line.strip_prefix(MARKER).filter(|id| is_id(id)) {
                found.entry(id.into()).or_default().insert(name.clone());
            } else if line.starts_with("//") && line.contains(MARKER_WORD) {
                problems.push(format!(
                    "{name}:{}: malformed marker, expected `{MARKER}<id>`",
                    n + 1
                ));
            }
        }
    }
    found
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for path in entries.map(|e| e.expect("listing a source directory").path()) {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The contracts of the document, each `### <id>` heading, with the files
/// its Code line lists.
fn contracts(doc: &str, problems: &mut Vec<String>) -> Places {
    let mut found = Places::new();
    let mut current: Option<String> = None;
    let mut in_code = false;
    for line in doc.lines() {
        if let Some(heading) = line.strip_prefix("### ") {
            if !is_id(heading) {
                problems.push(format!("{DOC}: heading `{heading}` is not a contract id"));
            }
            if found.insert(heading.into(), BTreeSet::new()).is_some() {
                problems.push(format!("{DOC}: contract `{heading}` is listed twice"));
            }
            current = Some(heading.into());
            in_code = false;
            continue;
        }
        if line.starts_with('#') {
            current = None;
        }
        // The Code line, and its continuation lines.
        in_code = line.starts_with("- Code:") || (in_code && line.starts_with("  "));
        if let (Some(id), true) = (&current, in_code) {
            let files = found.entry(id.clone()).or_default();
            files.extend(
                line.split('`')
                    .skip(1)
                    .step_by(2)
                    .filter(|s| SOURCES.iter().any(|d| s.starts_with(&format!("{d}/"))))
                    .map(str::to_string),
            );
        }
    }
    found
}

/// Lowercase words of letters and digits joined by single hyphens.
fn is_id(s: &str) -> bool {
    s.split('-').all(|w| {
        !w.is_empty()
            && w.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

fn join(files: &BTreeSet<String>) -> String {
    if files.is_empty() {
        return "no file".into();
    }
    files.iter().cloned().collect::<Vec<_>>().join(", ")
}
