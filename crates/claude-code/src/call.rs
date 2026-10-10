//! What a Bash call shows the model, and how long it may last.

use std::io::Stderr;

/// The longest a Bash call may last, in seconds, whatever its timeout: a
/// call reaching it moves to the background. An admission wait counts
/// toward it.
// claude-code: bash-call-timeout
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// Where a Bash call's notices go: Claude reads the call's standard error
/// with its output. Anything else the prefix has to say goes elsewhere, since
/// a hook or the status line must not show it.
// claude-code: bash-call-output
pub fn notices() -> Stderr {
    std::io::stderr()
}
