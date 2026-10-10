# orchestrator

Keeps the Claude Code sessions running in parallel on one Linux machine working
within its finite resources without refusing their work. Today it measures
what each command uses and holds a memory-hungry Bash call back until memory
covers it; planned work is in the GitHub issues. The documentation at the top
of `src/main.rs` describes the components and the principles: start there.

- An issue carries a piece of work from start to end: its intent, the options
  weighed, the decision, what was measured and in which context, dated notes.
  Once closed it is history and is never updated: read it to learn what
  happened, never to learn what the code does. Each issue carries exactly one
  type label (`type: feature`, `type: bug`, `type: task`), its area labels,
  and `needs decision` or `proposal` when it applies.
- The code and the doc comments at the top of each crate and module say what
  it does and why, as of now. A pull request that changes a behaviour updates
  them. When a decision or a measurement explains the current code, that doc
  states what the code relies on or rules out, and links the issue it comes
  from.
- A Cargo workspace, one crate per component under `crates/`; the root
  package holds the two binaries, `orchestrator` and `orchestrator-prefix`.
  Done means `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
  and `cargo test` all pass; run from the root, they cover every crate.
- Code, comments and docs are in English; the repository is public, so nothing
  specific to one person's machine, accounts or setup goes in.
- Everything orchestrator relies on in Claude Code lives in
  `crates/claude-code/`; other crates go through its API. Its `CLAUDE.md`
  says how to change it.
- `plugins/orchestrator/` is the guide, a Claude Code plugin: a user manual
  for the code it ships with, only what exists, never what is planned. The
  project skill `guide-writing` says how to write it. Every pull request
  updates it, or its description holds a line `Guide: unchanged, <reason>`
  (CI guard).

Runtime data lives in `$XDG_RUNTIME_DIR/orchestrator/`, never in this repository.
