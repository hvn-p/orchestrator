# Configuration

## config.json

`$XDG_CONFIG_HOME/orchestrator/config.json`, by default
`~/.config/orchestrator/config.json` (a relative `XDG_CONFIG_HOME` is
ignored), resolved from the environment Claude Code was started with. Without
it, nothing waits and no coordinator starts by itself. The prefix reads it at
every Bash call of an orchestrated session, `watch` at each event and each
waiting call, and `setup`, `coordinator`, `admission` and `config` when they
run: a change applies at once, with nothing to restart.

It is written by `orchestrator config admission` and `orchestrator config
coordinator`, which the setup conversation uses (commands.md), or by hand.
The commands refuse values that make no sense on the machine; a file written
by hand is checked only for its form.

For example, for a machine with about 30 GB of RAM:

```json
{
  "admission": {
    "heavy_mb": 1024,
    "margin_mb": 2048,
    "max_wait_secs": 60,
    "max_background_wait_secs": 1800
  }
}
```

| Field | Meaning |
| :- | :- |
| `admission.heavy_mb` | A Bash call whose expected peak reaches this, in MB, waits for memory. At 0 (only by hand), every call with a learned command is heavy. |
| `admission.margin_mb` | Free memory kept on top of the expected peak, in MB. At 0, a heavy call starts as soon as free memory covers its expected peak. |
| `admission.max_wait_secs` | The longest a call waits; then it is refused, and told how to wait longer. At most 119, under a Bash call's default timeout. At 0, a heavy call that does not fit is refused at once. |
| `admission.max_background_wait_secs` | The longest a call whose command assigns `ORCHESTRATOR_BACKGROUND` waits; then it is refused. Above `max_wait_secs`, at most 1800. |

- The sections are `admission` and `coordinator`; the fields of each are
  required, except those the table below marks optional.
- A file that cannot be read whole (invalid JSON, a missing or unknown field,
  a misspelt section) turns admission off, each Bash call logging the error
  to `prefix.log` in the runtime directory, and starts no coordinator,
  `watch` printing `orchestrator: <error>; no coordinator starts`. A typo
  never makes calls wait nor spends tokens. `setup`, `coordinator` and the
  `config` commands then stop with `Error: reading <path>` and its cause:
  the setup conversation cannot repair the file; fix or remove it by hand.
  `admission` reports admission off with the reason.
- Removing the file, or its `admission` section, turns admission off, with
  nothing logged.
- MB here, as everywhere in orchestrator, means 1024 × 1024 bytes.

### The coordinator section

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

| Field | Meaning |
| :- | :- |
| `coordinator.wake` | `true`: `watch` starts coordinator runs by itself for `memory_pressure` and `admission_wait` events, using tokens of the account without asking. `false`: it starts none and queues nothing. |
| `coordinator.model` | The model of those runs, as `claude --model` takes it. The setup conversation proposes `claude-sonnet-5-5`. |
| `coordinator.max_minutes` | The longest a run lasts; then `watch` stops it. 5 when `config coordinator` creates the section. |
| `coordinator.wait_secs` | How long admission holds a Bash call back before `watch` writes an `admission_wait` event for it. 20 when `config coordinator` creates the section. At or above `admission.max_wait_secs`, only calls waiting in the background are reported; at or above `admission.max_background_wait_secs`, none is. |
| `coordinator.language` | Optional. The language coordinators write in, a tag such as `fr` or `pt-BR`. Unset: English for runs, the system's language for setup and the interactive coordinator. |
| `coordinator.max_budget_usd` | Optional, absent by default, written only by hand, above 0. The most a run may spend, checked by Claude Code against its estimate at API list price; a subscription counts tokens against its quota instead. |

Apart from `language`, which every coordinator follows, the section governs
the runs `watch` starts. Setup and the interactive coordinator open whenever
asked, with Claude Code's default model, without `max_minutes` or
`max_budget_usd`; the interactive one receives events only while `wake` is
on, since `watch` queues nothing otherwise.

A section that makes no sense (a model name with a space, `max_minutes`
outside 1 to 60, `wait_secs` outside 1 to 1800, a malformed `language`,
`max_budget_usd` at 0 or below), which only a hand-written file can hold,
starts no run: `watch` prints `orchestrator: <why>; no coordinator starts`.
`config admission` and `config coordinator` check the whole section on each
write, so they refuse to write until it is fixed by hand; the setup
conversation, which writes through them, cannot fix it either. How
coordinators work: coordinator.md.

## The coordinators' instructions

`$XDG_CONFIG_HOME/orchestrator/CLAUDE.md`, next to `config.json`, holds
instructions every coordinator follows, priorities included, and no other
session reads. Its imports and limits: coordinator.md.

## Choosing values

The setup conversation proposes values from the machine's facts. The facts
it and a human weigh:

- **The wait holds the conversation and counts toward the Bash call's
  timeout**: 2 min by default (`BASH_DEFAULT_TIMEOUT_MS`); Claude can ask for
  more, up to the larger of `BASH_MAX_TIMEOUT_MS` (10 min by default) and the
  default. The prefix cannot see it. A foreground call that reaches its
  timeout is moved to the background by Claude Code, not stopped, unless it
  starts with `sleep` or background tasks are turned off
  (`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`); its result then holds none of
  its output. Hence `max_wait_secs` under 120: the refusal reaches Claude
  with the call's result.
- **Waiting in the background holds nothing**: a call Claude runs again in
  the background returns at once, and Claude is told when it ends. Claude
  Code lets a background command run with no time limit in an interactive
  session, 30 min in one that runs unattended, 10 in a `claude -p` run whose
  prompt is text: `max_background_wait_secs` above what applies is cut short
  there.
- **What the margin absorbs**: a heavy call is admitted on the kernel's
  `MemAvailable` at that moment, net of the reservations of heavy calls
  already running. Memory that anything else takes afterwards (other
  sessions, light calls, other programs) is covered only by `margin_mb`.
- **A need the machine can never meet**: a call needs its expected peak plus
  `margin_mb` free. When that exceeds what the machine can free, the call
  waits its longest wait every time, then is refused.
- **What `heavy_mb` catches**: `orchestrator peaks` lists each command's
  expected peak; those at or above `heavy_mb` are the ones that may wait.

## How admission decides

Admission applies to Bash calls and `!` commands of orchestrated sessions
only. Hooks, the status line and MCP servers never wait, nor does anything
typed in a terminal outside Claude Code.

### Recognising a command

The command Claude wrote is the argument of the last `eval` of the invocation
Claude Code hands the prefix. It is parsed as shell, without running
anything:

- The call is split into simple commands at `&&`, `||`, `;`, `|`, `|&`, `&`
  and newlines.
- Removed: the wrappers `timeout`, `time`, `nice`, `nohup`, `stdbuf`,
  `command` and `builtin` with their options, leading environment
  assignments, redirections, and `cd`, whose target still decides the
  directory of the commands after it. So
  `cd app && timeout 300 pnpm exec vitest run X 2>&1 | grep FAIL` yields
  `pnpm exec vitest run X` and `grep FAIL`.
- The bodies of brace groups, subshells, loops and conditionals are commands
  of the call. A `cd` inside a subshell, inside a pipeline of several
  commands, or in a command started with `&` stays there. A command
  substitution stays part of its word. A here-document or here-string stays
  with its command, as its input. Function definitions, `[[ … ]]` tests and
  `(( … ))` arithmetic are not commands.
- What remains is kept word for word, quotes removed and variables as
  written: `vitest run` and `vitest run one.test.ts` are two commands.
- After a `cd` whose target only running the call would tell (`cd "$dir"`,
  `cd -`), the commands are never learned and never make a call wait. A call
  that cannot be parsed teaches nothing and never waits.
- A command whose directory does not exist once the call has ended is not
  learned: the `cd` into it failed, so the command did not run. The check
  comes after the call, so `mkdir -p b && cd b && make` is learned. Admission
  never checks that a directory exists.

### Learning

- `watch` learns per repository (the git common directory of the directory
  the command runs in, all worktrees together; outside a repository, the
  directory) and per command. A command's id is its repository, its words and
  its input: within a repository, the directory does not matter, so
  `pnpm test` run in two packages of one repository is one command with one
  expected peak. Admission looks a command up the same way, in the repository
  holding the directory it runs in.
- Each command of a measured call gets the call's peak, marked "alone" when
  it was the call's only command; a command that is not learned (see above)
  still counts, so the others are not alone. The latest five calls are kept.
- A command's expected peak is the smallest peak among its latest calls,
  walking back from the latest and stopping at the latest call it ran alone
  in. So:
  - A call that ran the command alone sets its expected peak to that call's
    peak: one heavy run alone makes it heavy, one light run alone makes it
    light.
  - A heavy call that shares the command with other commands never raises it
    above its latest run alone, nor above a lighter call made since.
  - A filter such as `tail -1`, seen only beside a heavy command, carries
    that command's peak; a lighter call holding it lowers it, until that call
    leaves its latest five or a later call runs the filter alone.
- One measured call is enough: from then on the command has an expected peak.
- A repository's commands are spread over sixteen files by hash, each keeping
  its 125 most recently seen commands: up to 2,000 per repository.

### Admitting

- A call's expected peak is the largest of its commands', never their sum. A
  call with no learned command, or under `heavy_mb`, starts at once and
  reserves nothing.
- A heavy call waits until free memory covers its expected peak plus
  `margin_mb`. Free memory is the kernel's `MemAvailable` minus the
  reservations of heavy calls already running: from its admission until its
  job group empties, a heavy call reserves its expected peak minus what its
  group already uses.
- Reservations are shared by all the user's orchestrated sessions, through
  the runtime directory. Admission reads the peaks already learned: it works
  while `watch` is stopped.
- There are no fixed slots: two heavy calls run together when memory covers
  both.
- Waiting calls are served in order: first the calls given priority with
  `orchestrator admission priority` (commands.md), in the order given, then
  the others by arrival. Each check hands free memory out in that order: a
  call ahead that fits takes its expected peak, and one that does not fit is
  passed, unless it was given priority. A call runs when what the calls
  ahead leave covers its need. So a call that fits never waits for a larger
  one, except one given priority, which no call passes until it runs or is
  refused.
- A waiting call checks again as soon as a job holding a reservation changes,
  and every second otherwise, which is when it sees a change of the calls
  ahead of it. After `max_wait_secs` it is refused: the
  command does not run, reserves nothing, and the call exits with code 75
  (`EX_TEMPFAIL`), its notice saying how to wait longer (events.md).
- A call whose command assigns `ORCHESTRATOR_BACKGROUND`, whatever the value,
  waits `max_background_wait_secs` instead. The prefix cannot see whether
  Claude Code runs a call in the background: the variable is how Claude says
  so. The refusal gives the value `wait-for-memory`, which states the reason
  in the command, the part of a call Claude Code's auto mode classifier
  reads; a mention that assigns nothing, in an `echo` for instance, counts
  too.
- A refused call, or one killed while it waits, teaches nothing: only a call
  that runs is measured.
- A heavy call that leaves a process running, such as a server started in the
  background, keeps its reservation, net of what its group uses, until that
  process ends.
- An admission lock held for more than a second, or any other error, lets the
  call run at once; the error goes to `prefix.log`.

## Environment variables

| Variable | Read by | Effect |
| :- | :- | :- |
| `XDG_CONFIG_HOME` | the prefix, `watch`, `config`, `setup`, `coordinator`, `admission` | Where `config.json` and the coordinators' `CLAUDE.md` are (absolute paths only). |
| `XDG_STATE_HOME` | the prefix, `watch`, `peaks`, `admission`, `setup`, `coordinator`, coordinators | Where learned peaks and the coordinator's journal are (absolute paths only). |
| `XDG_RUNTIME_DIR` | the prefix, `watch`, `admission`, `setup`, `coordinator`, coordinators | The runtime directory, else `/run/user/<uid>`, the uid read from `/proc/self/status`. |
| `HOME` | the prefix, `watch`, `sessions`, `peaks`, `admission`, `config`, `setup`, `coordinator`, coordinators | The fallback for `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `CLAUDE_CONFIG_DIR`; resolves `cd` and `cd ~` when recognising commands, and `~/` in the coordinators' imports, which `setup`, `coordinator` and `watch` resolve. `launch` does not read it. |
| `LC_ALL`, `LC_MESSAGES`, `LANG` | `setup`, `coordinator` | The first set gives the language a coordinator starts in when none is configured. |
| `PATH` | `watch`, `setup`, `coordinator` | Where `claude` is found to start a coordinator. A coordinator's own `orchestrator` commands are those of the binary that started it, put first on its `PATH`. |
| `CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD` | Claude Code | Removed from a coordinator's environment, so Claude Code does not load the coordinators' `CLAUDE.md` itself. |
| `CLAUDE_CONFIG_DIR` | `watch`, `sessions`, `admission`, `setup`, `coordinator`, coordinators | Claude Code's configuration directory, holding `sessions/`. |
| `CLAUDE_CODE_SHELL_PREFIX` | Claude Code | Set by `launch` to the prefix's path. |
| `CLAUDE_CODE_SHELL` | Claude Code | The shell Claude Code uses to run Bash tool commands, a bash or zsh binary. The prefix runs them with bash whatever it is. |
| `CLAUDE_CODE_SESSION_ID` | `watch`, `sessions` | Set by Claude Code in every command it starts; attributes a process reparented away from claude, and marks orphans. |
| `BASH_DEFAULT_TIMEOUT_MS`, `BASH_MAX_TIMEOUT_MS` | Claude Code | The Bash timeout the admission wait counts toward. |
| `ORCHESTRATOR_BACKGROUND` | the prefix, in the command's text | Assigned in a Bash call's command, the call waits `max_background_wait_secs` instead of `max_wait_secs`. |
| `CLAUDE_CODE_TOOL_MEMORY_LIMIT` | Claude Code | On Linux, a size such as `4G` caps the memory of all of a session's Bash, PowerShell and Monitor commands together, through a memory cgroup of Claude Code's own: past it, the kernel kills a command, and nothing in its result names the cap. `CLAUDE_CODE_TOOL_MEMORY_CGROUP_EXCLUDE` lists the other kinds of processes Claude Code exempts from that cap. orchestrator never kills a command. |

The prefix reads these from the environment Claude Code was started with.
