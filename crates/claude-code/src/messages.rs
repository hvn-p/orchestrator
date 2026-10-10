//! Messages between the sessions of the machine.

use crate::sessions::ClaudeSession;

/// What messages address `session` by: its name.
// claude-code: cross-session-message
pub fn address(session: &ClaudeSession) -> &str {
    &session.name
}
