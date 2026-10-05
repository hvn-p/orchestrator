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

Runtime data lives in `$XDG_RUNTIME_DIR/orchestrator/`, never in this repository.
