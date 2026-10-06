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
  sub-group of its own.
- Admission: once a configuration exists, a Bash call whose commands were
  learned as memory-hungry waits, before it runs, until free memory covers its
  expected peak. Every other command starts at once. Nothing is throttled yet.
- `orchestrator sessions` prints the memory used by each Claude Code session
  (with its largest process) and the processes left behind by sessions that no
  longer exist.
- `orchestrator watch` runs as a service that sleeps until the kernel reports
  something, and appends one JSON line per event (`memory_pressure`,
  `orphans`) to an events file. It also measures the
  memory peak of each finished Bash call of an orchestrated session, learns it
  per repository and command, then removes the call's empty group.
- `orchestrator peaks` prints the peaks learned so far.

Everything else is design (throttling, the coordinator): see
[docs/design.md](docs/design.md).

## Requirements

- Linux with cgroup v2 and a systemd user manager that delegates to user
  scopes (`launch`). `sessions` and `watch` only read `/proc` and Claude Code's
  session files (`~/.claude/sessions/`).
- A Rust toolchain, Rust 1.88 or later.

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
orchestrator watch
```

When tasks stall on memory for `--stall-ms` (default 200) within a 2 s window,
`watch` ranks sessions by memory and writes a `memory_pressure` event, at most
once per `--cooldown-secs` (default 60). Five seconds after a Claude session's
process exits, it reports the processes the session left behind, each orphan
group once; a scan every `--orphan-interval-secs` (default 300) catches the
rest. Events go to
`$XDG_RUNTIME_DIR/orchestrator/events.jsonl` unless `--runtime-dir` says
otherwise. As soon as a Bash call of an orchestrated session ends, it appends
the call's peak, with its command, to `measurements.jsonl` in the same
directory, and learns the peak of each command of the call in the repository
it ran in. `--help` lists every option.

```sh
orchestrator peaks
```

Learned peaks live in `$XDG_STATE_HOME/orchestrator/peaks/` (by default
`~/.local/state/orchestrator/peaks/`) unless `--state-dir` says otherwise, for
`watch` as for `peaks`. A command is stored under a hash, with a label for
display: the command as recognised, cut to 60 characters. Only commands Claude
wrote land there; see "Recognising a command" in
[docs/design.md](docs/design.md).

## Configuration

Without a configuration, nothing waits. Admission reads
`$XDG_CONFIG_HOME/orchestrator/config.json`, by default
`~/.config/orchestrator/config.json`, and knows only the peaks `orchestrator
watch` has learned. The coordinator is meant to write this file, adapted to
the machine; until it exists, write it by hand. For a machine with about 30 GB
of RAM:

```json
{
  "admission": {
    "heavy_mb": 1024,
    "margin_mb": 2048,
    "max_wait_secs": 60
  }
}
```

- `heavy_mb`: a Bash call whose expected peak reaches this, in MB, waits for
  memory. A call's expected peak is the largest learned peak among its
  commands; a call with no learned command never waits.
- `margin_mb`: free memory kept on top of the expected peak. A heavy call
  starts once available memory, minus what the heavy calls already running
  still expect to use, covers its expected peak plus this margin.
- `max_wait_secs`: the longest a call waits; then it runs anyway. The wait
  counts toward the Bash call's timeout (2 min by default, 10 min at most).

While a call waits, its output starts with a notice naming the command by its
head, its expected peak and the memory it needs, then a second one when it
runs. Remove the file, or its `admission` section, to turn admission off. A
file that cannot be read, an unknown field included, also turns it off, and
the error goes to `$XDG_RUNTIME_DIR/orchestrator/prefix.log`.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
