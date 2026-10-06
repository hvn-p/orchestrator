//! The prefix never makes a command fail because of how many arguments Claude
//! Code passed it: one is the command line, anything else still runs.

// Clippy's test exemption covers `#[test]` functions, not their helpers.
#![allow(clippy::expect_used)]

use std::process::{Command, Output};

fn prefix(args: &[&str]) -> Output {
    let runtime = tempfile::tempdir().expect("a temporary runtime dir");
    Command::new(env!("CARGO_BIN_EXE_orchestrator-prefix"))
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .expect("the prefix runs")
}

#[test]
fn one_argument_is_a_command_line_run_by_bash() {
    let out = prefix(&["echo one; exit 3"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "one\n");
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn several_arguments_still_run_as_a_command() {
    let out = prefix(&["echo", "two", "words"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "two words\n");
    assert!(out.status.success());
}

#[test]
fn no_argument_runs_nothing_and_succeeds() {
    let out = prefix(&[]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert!(out.status.success());
}
