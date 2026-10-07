# orchestrator

Keeps the Claude Code sessions running in parallel on one Linux machine working
within its finite resources: it delays, queues, slows down, pauses and reorders
their work rather than refusing it. `docs/design.md` holds the design, what has
been measured and the open questions: start there.

- `src/`: the Rust binary (`orchestrator sessions`, `orchestrator watch`). Done
  means `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
  `cargo test` all pass.
- Code, comments and docs are in English; the repository is public, so nothing
  specific to one person's machine, accounts or setup goes in.
- What orchestrator relies on in Claude Code is listed in
  `docs/claude-code-dependency.md`. Code relying on a contract carries a
  `// claude-code: <id>` line; a pull request changing such a file must update
  that document, or carry the label `claude-code-dependency-unchanged` (CI
  guard). `cargo test --test claude_code -- --ignored` checks the installed
  Claude Code against it, by hand only.
- `plugins/orchestrator/` is the guide, a Claude Code plugin describing the
  code it ships with. Every pull request updates it, or its description holds
  a line `Guide: unchanged, <reason>` (CI guard).

Runtime data lives in `$XDG_RUNTIME_DIR/orchestrator/`, never in this repository.
