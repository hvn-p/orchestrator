# claude-code

This crate holds everything orchestrator relies on in Claude Code. Other
crates use its API, in orchestrator's terms (a session, a command, an agent,
a message), never a Claude Code format directly.

- `claude-code-dependency.md` lists the contracts. Code relying on one
  carries its `// claude-code: <id>` marker, and a new reliance adds its
  contract first. The document's "Keeping it current" says what the tests
  and the CI guard check.
- A change to how orchestrator uses Claude Code is checked against the
  installed version with `cargo test --test claude_code -- --ignored`, by
  hand: it starts a real session and spends tokens.
- After a Claude Code update, the project skill `claude-code-compatibility`
  runs that check and updates the verified version.
