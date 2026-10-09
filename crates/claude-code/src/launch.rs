//! Starting a session that runs every command through the prefix.

/// The variable Claude Code runs every command through: the prefix's path,
/// which Claude Code runs as one quoted path, so it holds no argument.
// claude-code: shell-prefix-variable
pub const PREFIX_VAR: &str = "CLAUDE_CODE_SHELL_PREFIX";
