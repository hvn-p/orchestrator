# orchestrator

orchestrator keeps the Claude Code sessions running in parallel on one Linux
machine working within its finite resources. Rather than refusing work, it
observes what each session runs and schedules it: delay, queue, slow down,
pause, reorder.

## Status

Early. What exists:

- `orchestrator launch -- claude …` starts a Claude Code session in a cgroup of
  its own, and points Claude Code at `orchestrator-prefix`, which places every
  command the session starts (Bash calls, hooks, status line, MCP servers) in a
  sub-group of its own. Nothing is queued or throttled yet.
- `orchestrator sessions` prints the memory used by each Claude Code session
  (with its largest process) and the processes left behind by sessions that no
  longer exist.
- `orchestrator watch` runs as a long-lived loop and appends one JSON line per
  event (`memory_pressure`, `orphans`) to an events file. It also measures the
  memory peak of each finished Bash call of an orchestrated session, then
  removes the call's empty group.

Everything else is design (admission, throttling, the coordinator): see
[docs/design.md](docs/design.md).

## Requirements

- Linux with cgroup v2 and a systemd user manager that delegates to user
  scopes (`launch`). `sessions` and `watch` only read `/proc` and Claude Code's
  session files (`~/.claude/sessions/`).
- A Rust toolchain supporting edition 2024 (Rust 1.85 or later).

## Build

```sh
cargo install --path .
```

This installs two binaries side by side: `orchestrator` and
`orchestrator-prefix`. `launch` finds the prefix next to itself.

## Usage

```sh
orchestrator launch -- claude
```

Arguments after `--` go to `claude` unchanged. When the session's cgroup cannot
be set up, the session still starts, unorchestrated, with a warning.

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
otherwise. On every check it also appends the peak of each finished Bash call,
with its command, to `measurements.jsonl` in the same directory. `--help` lists
every option.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
