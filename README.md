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
- A first coordinator, once enabled: a Claude Code session that `watch`
  starts when memory runs short or admission holds a call back long. It asks
  sessions to free memory, or tells them why a call waits; it pulls no
  lever. `orchestrator setup` is a conversation with it that ends with the
  configuration written.

Everything else is design (throttling, priorities between waiting calls):
see [docs/design.md](docs/design.md).

## Requirements

- Linux with cgroup v2, a systemd user manager that delegates to user scopes,
  a user D-Bus, and `busctl`, which ships with systemd (`launch`). `sessions`
  and `watch` only read `/proc` and Claude Code's session files
  (`~/.claude/sessions/`).
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

Arguments after `--` go to `claude` unchanged. `launch` replaces itself with
`claude`, keeping its pid and environment, and prints nothing. When the systemd
user manager does not give the session its cgroup within 2 s, or the cgroup
cannot be set up, the session still starts, unorchestrated, with a warning.

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

```sh
orchestrator admission
orchestrator machine
```

`admission` prints the Bash calls waiting for memory and the memory reserved
by heavy calls running; `machine` prints memory, swap, CPUs and what the
systemd user manager delegates.

Learned peaks live in `$XDG_STATE_HOME/orchestrator/peaks/` (by default
`~/.local/state/orchestrator/peaks/`) unless `--state-dir` says otherwise, for
`watch` as for `peaks`. A command is stored under a hash, with a label for
display: the command as recognised, cut to 60 characters. Only commands Claude
wrote land there; see "Recognising a command" in
[docs/design.md](docs/design.md).

## Configuration

Without a configuration, nothing waits and nothing spends tokens. Admission
reads `$XDG_CONFIG_HOME/orchestrator/config.json`, by default
`~/.config/orchestrator/config.json`, and knows only the peaks `orchestrator
watch` has learned.

```sh
orchestrator setup
```

`setup` opens a conversation with the coordinator (Claude Code, `claude` on
the `PATH`), in the system's language, taken from the locale (`LC_ALL`, then
`LC_MESSAGES`, then `LANG`; English without one), which it offers to change.
It says what orchestrator does, looks at the machine, then asks you, in
plain words, whether it may be woken automatically, with which model (it
proposes claude-sonnet-5-5), what your priorities are, and proposes
admission thresholds from the machine's facts, adjusted to your answers. It
writes each part once you agree, through the commands below, which refuse
values that make no sense on the machine, and ends by saying what it wrote.
Run it again to review the configuration; `orchestrator coordinator` starts
the same conversation when there is no configuration yet.

`orchestrator config` prints the file. The commands setup uses also work by
hand: `orchestrator config admission --heavy-mb … --margin-mb …
--max-wait-secs …` sets the thresholds, and `orchestrator config
coordinator --wake yes --model … --language …` the coordinator. Written by hand, for a
machine with about 30 GB of RAM:

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
label, its expected peak and the memory it needs, then a second one when it
runs. Remove the file, or its `admission` section, to turn admission off. A
file that cannot be read, an unknown field included, also turns it off, and
the error goes to `$XDG_RUNTIME_DIR/orchestrator/prefix.log`.

### The coordinator

```json
{
  "coordinator": {
    "wake": true,
    "model": "claude-sonnet-5-5",
    "max_minutes": 5,
    "wait_secs": 20,
    "language": "en"
  }
}
```

`wake` is your consent: with it, `orchestrator watch` starts a coordinator by
itself, one at a time, for each batch of `memory_pressure` events and calls
that admission has held back `wait_secs`, each run with `model` and stopped
past `max_minutes`, a guard against a stuck run. Without it, nothing spends
tokens unless you open a coordinator. `language`, a tag such as `fr`, `en` or
`pt-BR` (two or three lowercase letters, then subtags joined by `-`), is the
language every coordinator writes in: replies, journal notes and messages to
sessions; English when unset. What orchestrator itself prints, admission's
notices included, stays in English. Nothing caps a run's spending unless
you add `"max_budget_usd"`, which Claude Code checks against its estimate at
API list price, not against a subscription's quota.

`watch` gathers the state a run needs (the machine, the sessions, the calls
waiting for memory and the reservations, the heavy commands learned, the
latest lines of the journal) and puts it in the run's prompt,
so the run only decides, messages sessions and notes what it did. Each queued
event is pending, in progress, then done; a run that fails gives its events
back for the next one, and gives up on an event after three failed runs. A
run may read more of the state, note in its journal
(`orchestrator coordinator note`) and message sessions, nothing else; it
messages under the name `orchestrator-coordinator`.

```sh
orchestrator coordinator
```

opens an interactive coordinator, after the setup conversation when there is
no configuration yet: you talk to it, and it receives the events for as long
as it stays open; meanwhile `watch` starts no run. Events it took
but did not handle when it closes go back to the next run. Tell it a
priority or a lasting instruction, and it offers to write it in your
instructions for coordinators (below). It remembers what it did in a
journal under
`$XDG_STATE_HOME/orchestrator/coordinator/`. Each run is summarised in
`$XDG_RUNTIME_DIR/orchestrator/coordinator/runs.jsonl`: turns, seconds and
tokens first, then its reply, and Claude Code's dollar estimate at list
price, which a subscription does not pay.

### Instructions for coordinators

`$XDG_CONFIG_HOME/orchestrator/CLAUDE.md`, by default
`~/.config/orchestrator/CLAUDE.md`, is a CLAUDE.md for coordinators only:
every coordinator reads it (runs, setup, the interactive one), no other
session does. Your priorities go there, with anything else coordinators
should know of your machine and habits; setup offers to create it.

It is yours to keep, wherever you keep such files. It may be a symlink, into
a dotfiles repository for instance, and it may import other files with
`@path` lines, as Claude Code's own CLAUDE.md does: an absolute path, `~/`
for your home, or a path relative to the importing file (once symlinks are
resolved), outside code spans and fenced blocks, `\ ` for a space, four
hops deep at most. orchestrator resolves the imports itself and hands the
result, verbatim, to each coordinator it starts, so an edit applies to the
next one. A file imported twice, or in a cycle, is read once; a file over
128 KiB, or past 256 KiB in all, is left out and the coordinator is told.

## Claude Code dependency

orchestrator relies on Claude Code only through the contracts listed in
[docs/claude-code-dependency.md](docs/claude-code-dependency.md), with the
Claude Code version they were last verified on. To check them against the
installed Claude Code:

```sh
cargo test --test claude_code -- --ignored --nocapture
```

This starts a real headless session through `orchestrator launch`, then a
coordinator, so it needs a signed-in `claude`, a systemd user manager with
cgroup v2, and spends a few tens of thousands of tokens, most read from the
prompt cache. It never runs in CI.

## Continuous integration

Every pull request and every push to `main` runs `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test`, and a build with
Rust 1.88. A pull request that changes code relying on Claude Code must also
update `docs/claude-code-dependency.md`, unless it carries the label
`claude-code-dependency-unchanged`; `scripts/claude-code-guard.sh <base>
<head>` runs that check locally.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
