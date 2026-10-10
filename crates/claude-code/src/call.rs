//! What a Bash call shows the model, how long it may last, and how it runs
//! in the background.

use std::io::Stderr;

/// A Bash call's timeout when Claude asks for none. A foreground call
/// reaching its timeout moves to the background, and its result then holds
/// none of its output: an admission wait in the foreground stays under it,
/// so that its outcome reaches Claude with the call's result.
// claude-code: bash-call-timeout
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// The longest a command runs in the background in a session that runs
/// unattended, such as `claude -p`; an interactive session sets no limit.
// claude-code: bash-call-background
pub const BACKGROUND_LIMIT_SECS: u64 = 1800;

/// How Claude lets a call wait longer without being held: it runs the call
/// again in the background, with `assignment` before the command. The
/// session is told when it ends.
// claude-code: bash-call-background
pub fn in_background(assignment: &str) -> String {
    format!("run it in the background with {assignment} before the command")
}

/// Where a Bash call's notices go: Claude reads the call's standard error
/// with its output. Anything else the prefix has to say goes elsewhere, since
/// a hook or the status line must not show it.
// claude-code: bash-call-output
pub fn notices() -> Stderr {
    std::io::stderr()
}
