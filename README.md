# orchestrator

orchestrator keeps the Claude Code sessions running in parallel on one Linux
machine working within its finite resources. Rather than refusing work, it
observes what each session runs and schedules it: delay, queue, slow down,
pause, reorder.

## Status

Early. Two subcommands exist:

- `orchestrator sessions` prints the memory used by each Claude Code session
  (with its largest process) and the processes left behind by sessions that no
  longer exist.
- `orchestrator watch` runs as a long-lived loop and appends one JSON line per
  event (`memory_pressure`, `orphans`) to an events file.

Everything else is design (one process group per session, admission,
throttling, the coordinator): see [docs/design.md](docs/design.md).

## Requirements

- Linux. What exists today only reads `/proc` and Claude Code's session files
  (`~/.claude/sessions/`). The planned cgroup work needs cgroup v2 and a systemd
  user manager that delegates to user scopes.
- A Rust toolchain supporting edition 2024 (Rust 1.85 or later).

## Build

```sh
cargo build --release
```

The binary is `target/release/orchestrator`.

## Usage

```sh
orchestrator sessions
```

```sh
orchestrator watch --mem-min-mb 3000
```

`watch` checks available memory every `--interval-secs` (default 2) and, below
`--mem-min-mb`, ranks sessions by memory and writes a `memory_pressure` event,
at most once per `--cooldown-secs` (default 60). Every `--orphan-interval-secs`
(default 30) it reports newly orphaned processes once each. Events go to
`$XDG_RUNTIME_DIR/orchestrator/events.jsonl` unless `--runtime-dir` says
otherwise. `--help` lists every option.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
