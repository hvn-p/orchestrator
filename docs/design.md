# orchestrator design

orchestrator keeps the Claude Code sessions running in parallel on one Linux
machine working within its finite resources. It schedules their work: it delays,
queues, slows down, pauses and reorders it. It never refuses work. A job may take
longer, it is not prevented. Stopping a process remains possible as a last
resort, when memory is about to run out and nothing else holds.

Anything that can be decided without judgment is done by code and spends no
tokens. Judgment (priorities, exceptions, negotiating with a session) is left to
a coordinator: a Claude Code session that reads orchestrator's events.

Rust was chosen for a strict compiler, a start time around a millisecond (the
shell prefix runs before every command of every session) and a resident service
of a few MB.

## Status

`orchestrator launch` and the shell prefix exist: a session runs in a cgroup
of its own, and each command it starts in a job group of its own. Nothing is
queued or throttled yet. `orchestrator sessions` and `orchestrator watch` still
attribute processes to sessions by process ancestry and report memory pressure
and orphaned processes. Everything else here is design; the parts validated by
throwaway prototypes are listed under "Measured".

## One process group per session

orchestrator does not recognise commands by name. It relies on cgroups: a group
of processes kept by the kernel, which every descendant joins whatever its name,
language or depth. The kernel accounts the group's memory and CPU, and can slow
it down or freeze it.

```
session A        delegated systemd user scope, started by `orchestrator launch`
├─ main/         claude itself
├─ job-bash-N/   one Bash call: pnpm typecheck, then its three tsc
├─ job-bash-M/   one Bash call: next dev, which listens on a port
└─ job-other-K/  a hook, the status line, an MCP server
```

Every command Claude Code starts goes through `CLAUDE_CODE_SHELL_PREFIX`, which
places it in its own sub-group before running it. The service observes these
groups and classifies them by behaviour, without any list of commands.

## Levers, gentlest first

1. **Slow down**: lower a job's CPU weight or cap (`cpu.weight`, `cpu.max`).
   Jest, pnpm and Vitest size their worker pools on the available parallelism,
   which follows `cpu.max`, so a cap also reduces their workers. The memory
   limit (`memory.high`) applies to the whole session, never to a job: Node
   sizes its default heap from the `memory.high` of its own cgroup, so a limit
   on the job would shrink the heap and a large type check would fail with
   "heap out of memory".
2. **Queue**: a Bash call already measured as memory-hungry waits, before it
   runs, until free memory covers its known peak (see "Admission"). Every other
   command starts at once. The wait counts toward the Bash call's timeout.
3. **Do it elsewhere**: when a project has a CI, whole-project checks (full test
   suite, type check, build) belong there. Whether to enforce this is a user
   policy, not part of the foundation.
4. **Reorder**: when several jobs wait, the coordinator decides which goes
   first.
5. **Pause**: freeze a job (`cgroup.freeze`). A frozen job stops growing; its
   memory stays allocated until it is swapped out.
6. **Stop**: last resort, when memory is about to run out and no other lever is
   enough.

## Principles

- **Never block work by failing**: outside an orchestrated session, without a
  configuration, or on any cgroup error, the prefix runs the command unchanged.
- **Commands typed outside Claude Code never wait**: they run outside any
  orchestrated session. A `!` command typed inside Claude Code goes through the
  prefix like any other Bash call.
- **Hooks, the status line and MCP servers never queue**: they get their own
  group, to be measured, and start immediately.

## Components

One binary, `orchestrator`, with subcommands.

- **Installation** (planned): one step puts the two binaries, `orchestrator`
  and `orchestrator-prefix`, on the `PATH` and the service under the systemd
  user manager. `cargo install` covers the binaries today.
- **`orchestrator launch -- claude …`** (exists; the short command is planned),
  exposed as a short command available from any directory: starts the session
  in a delegated systemd user scope in `orchestrator.slice`, moves claude into the `main/` leaf (a group that hands controllers to
  its children cannot hold processes itself), then enables `+cpu +memory +pids`
  for the sub-groups. A session started with `claude` alone stays outside
  orchestration.
- **The shell prefix**, `orchestrator-prefix` (exists, admission planned):
  Claude Code calls it with the full command line as a single argument, for
  every Bash call, hook, status line refresh and MCP stdio server start. It
  creates the sub-group, moves itself in, keeps the command of a Bash call for
  the service, waits for admission when it runs a Bash call, then replaces
  itself with the command. Output and exit code pass through unchanged. Claude
  Code runs the prefix as one quoted path, hence a binary of its own rather
  than a subcommand.
- **`orchestrator watch`**, a systemd user service:
  - exists: memory pressure and orphaned processes, as JSON lines in
    `events.jsonl`;
  - planned: process creation, exec and exit events from the kernel process
    connector instead of polling; each job's life and memory (`cgroup.events`,
    `memory.events`, `memory.peak`) and memory pressure (PSI); listening sockets
    every one or two seconds, attributed to their job; classification, levers,
    admission; reading each finished job's peak, then removing its empty
    group; the token quota left by the status line.
- **The coordinator** (planned): a Claude Code session started with its role
  appended to the system prompt. On its first start it examines the machine
  (memory, swap, CPU, what the systemd user manager delegates) and writes the
  configuration: the thresholds the code then applies. It waits for the next
  event with a background command that exits when one arrives, sets priorities,
  tells sessions about their delays, negotiates a slot when the limit of
  long-running servers is reached, and answers "who is working on X?".
- **`orchestrator sessions`** (exists): memory per session and orphaned
  processes, for a human.

Runtime data lives in `$XDG_RUNTIME_DIR/orchestrator/`: in memory, cleared at
reboot, never versioned.

## Classifying by behaviour

- **Lifetime**: a job that ends quickly, or one that keeps running.
- **Server**: a process of the job listens on a port, whatever the service (web,
  database, emulator).
- **Profile**: the job's CPU and memory over time, peak included. The memory
  peak feeds admission.
- **Attribution**: the group, inherited through the kernel, names the job and the
  session. For a process outside any orchestrated session, attribution falls back
  to process ancestry, then to `CLAUDE_CODE_SESSION_ID`.

## Admission

The prefix decides before a command runs. At that point it has only the command
line, which says nothing about memory: `pnpm typecheck` itself uses little, the
`tsc` processes it starts use gigabytes. Admission therefore learns from what it
measures.

- Every Bash call is measured: the peak memory of its job group, children
  included. The peak is remembered per repository, all its worktrees together,
  and per command.
- A command never seen before, or one whose known peak stays under the
  threshold, starts at once.
- A command whose known peak is above the threshold waits until free memory
  covers that peak plus a margin. There is no fixed number of slots: two such
  commands run together when the machine can hold both.
- Free memory is counted net of reservations. A memory-hungry command does not
  reach its peak at once: a type check starts near zero and climbs for tens of
  seconds. From its admission until it ends, it reserves its expected peak
  minus the memory it already uses, so a second command arriving during that
  climb does not count on the same memory. Unknown and light commands reserve
  nothing.

Admission gets more accurate as commands are measured, with no list to
maintain. What it cannot foresee (a first run, a form of the command it does not
recognise) is left to the other levers.

### Recognising a command

Claude writes the same command in many forms. Admission recognises it the way
Claude Code matches its Bash permission rules. The call is split into simple
commands at `&&`, `||`, `;`, `|`, `|&`, `&` and newlines. What does not change
the work is then removed: the wrappers `timeout`, `time`, `nice`, `nohup` and
`stdbuf`, leading environment assignments, redirections, and `cd`, whose target
still decides the repository. So `cd app && timeout 300 pnpm exec vitest run X
2>&1 | grep FAIL` yields `pnpm exec vitest run X` and `grep FAIL`.

Each remaining command is learned word for word. Unlike a permission rule, no
wildcard widens the match: `vitest run` (the whole suite) and
`vitest run one.test.ts` stay distinct, since they do not weigh the same. A call
holds several commands but has a single peak: the command that carries it is
the one found in heavy calls and never in light ones.

Where Claude Code asks when in doubt, admission lets the command start.

Peaks are kept in `$XDG_STATE_HOME/orchestrator/` so that they survive a reboot.

## Measured

On a reference machine: 30 GB of RAM, 4 GB of swap, Linux 7.0, cgroup v2,
systemd 259, Claude Code 2.1.289, October 2026.

Cgroup prototype, with a shell simulating a session that calls the prefix the
way Claude Code does:

- A descendant double-forked with `setsid`, then reparented to the user's
  systemd, stays in its job's group.
- Memory is readable per job and per session (`memory.current`, `memory.peak`).
- A job freezes and thaws: it stops at once, then resumes.
- With one slot, a second Bash call waits for the first to end (3.5 s); the
  waiting notice goes to its standard error and the first call's exit code (7)
  is preserved.
- A simulated hook does not queue even with no free slot; outside an
  orchestrated session the prefix runs the command unchanged.
- A listening server is found with `ss -ltnp` and attributed to its job.
- `MemoryHigh=3G` on the session leaves Node's default heap at 4288 MB; on the
  job's leaf it drops to 1728 MB.
- The delegated scope's files belong to the user, no root needed. `OOMPolicy`
  defaults to `continue` for a delegated scope (systemd.scope(5), systemd 259),
  so a process killed for lack of memory does not take the session down.

A real Claude Code session started through the prototype launcher:

- Bash calls (including `!` commands), hooks, the status line and the MCP stdio
  servers all went through the prefix, and the MCP servers connected normally.
- Clipboard helpers that Claude Code starts itself bypass the prefix and stay in
  `main/`.
- Empty job groups pile up, one per hook call and status line refresh:
  something has to remove them.

Other measurements:

- `MemoryHigh` slows a process down instead of killing it: 300 MB allocated under
  `MemoryHigh=100M` completes, with 207 MB pushed to swap.
- A slice with `ConcurrencySoftMax=1` (systemd 258 and later) makes a second
  `systemd-run --user --scope` wait for the first; exit code, working directory
  and environment are preserved. See "Rejected" for why it is not used.
- A terminal emulator launched from GNOME runs in one systemd scope together with
  everything started from it. With the default `OOMPolicy=stop`, one kernel OOM
  kill inside it stops the whole terminal and every session it hosts. A prefix
  drop-in (`app-gnome-<app id>-.scope.d/`) setting `OOMPolicy=continue`, followed
  by a reload of the user manager, keeps the terminal alive.
- Claude Code keeps `~/.claude/sessions/<pid>.json` (pid, session id, name,
  working directory, busy or idle status) and prints the same list with
  `claude agents --json`.
- Claude Code runs a Bash call as one shell script: it sources the session's
  shell snapshot, then runs the command Claude wrote inside `eval '…'`, from
  which the command can be extracted.
- `CLAUDE_CODE_SHELL_PREFIX` is run as one quoted path: a prefix holding an
  argument fails with "not found". Bash calls and MCP servers are started by
  claude itself, hooks through `/bin/sh -c`.
- When a session ends, systemd removes its scope together with every job group
  in it: a job's peak has to be read while its session lives.
- `CLAUDE_CODE_SESSION_ID` is inherited by the commands a session runs, but keeps
  the old id after `/clear` or a resume: ancestry is checked first.
- The kernel process connector delivers fork, exec and exit events to an
  unprivileged process on this kernel.
- Coordinator loop, first prototype (the Monitor tool on an events file): 11 s
  from the event to the message reaching the session, no token spent between
  events. A Monitor expires after 30 min; re-arming it costs about 0.07 USD and
  handling an event about 0.09 USD, as estimated by Claude Code at list price.
  Each coordinator turn re-reads about 72,000 tokens of base context. A
  background command ran for 36 min without being cut, which makes it the
  preferred way to wait.
- The status line input carries `rate_limits.five_hour` and
  `rate_limits.seven_day` (used percentage, reset time). The documentation lists
  this for some subscription types only; it was present on the reference
  machine's account.

From the Claude Code documentation:

- `CLAUDE_CODE_SHELL_PREFIX` wraps Bash calls, hooks, the status line and MCP
  stdio servers, but not PowerShell hooks nor exec-form hooks. For a Bash call
  the argument holds the whole invocation, environment setup included.
- A Bash call times out after 2 min by default and 10 min at most, tunable with
  `BASH_DEFAULT_TIMEOUT_MS` and `BASH_MAX_TIMEOUT_MS`.
- Bash permission rules are matched after splitting compound commands and
  stripping a fixed list of wrappers and known-safe environment assignments.
  The same program invoked in another form (`/usr/bin/curl`, `sh -c '…'`) is
  not recognised, and a command that cannot be parsed prompts.
- A `PreToolUse` hook can rewrite the Bash command (`updatedInput`), but cannot
  wait past its own timeout; the tool call then proceeds.
- An appended system prompt is reused on resume until the conversation is
  compacted, so a launcher passes it again on every resume.

## Known gaps

- **Outside the session's group**: Docker containers, anything started through
  `systemd-run --user`, services activated over D-Bus, an `xdg-open` handed to an
  already running browser. A shared service counts for the session that started
  it.
- **Freezing frees no RAM** by itself; with little swap it only stops growth.
- **systemd-oomd**: some distributions arm it on `user@.service` (kill above 50 %
  memory pressure for 20 s on the reference machine). Throttling too hard might
  trigger it: plausible, not verified.
- **The admission wait counts toward the Bash timeout.**
- **Processes Claude Code starts without the prefix** are counted with claude in
  `main/`.

## Open questions

- Maximum hook timeout: two readings of the documentation disagree (30 s for
  `PreToolUse`, 600 s by default for command hooks). To retest.
- Admission: which shell parser the prefix uses.
- Long-running servers: how many before the coordinator negotiates.
- Orphans and idle sessions holding resources: reported to the coordinator, or
  released automatically.
- Docker: regulated separately (`docker pause`, `docker update`, attribution by
  compose label), or left out of scope.
- Launching: the short command's name, and whether sessions started by other
  tools (a worktree manager, for instance) go through it.
- Where the coordinator runs: a terminal tab, or a background session (which
  needs a permission rule only the user can add). It is needed from the first
  start, to write the configuration.
- `memory.reclaim` on a frozen job, and the coordinator waiting through a
  background command: not tested yet.

## Inspirations

- **turnstile** (macOS only): slots per class of command with a free memory
  criterion, priority to commands typed by a human, reduced parallelism then
  pause under pressure, free passage when the service is absent.
- **AgentCgroup**: one group per tool call, reproduced here without eBPF through
  the prefix.
- **agent-throttle**, **agent-job-scheduler**, **MemMux**: a machine-wide
  semaphore, jobs in systemd scopes, budgeted admission. None of them classifies
  by behaviour.

## Rejected

- **Refusing a command**: contrary to the intent.
- **Recognising commands by name**: a hook only sees the command typed, not the
  `tsc` processes that `pnpm typecheck` starts; command shims first in `PATH`
  are bypassed by `node_modules/.bin`. Tool-specific knobs
  (`VITEST_MAX_WORKERS`, `PNPM_CONFIG_WORKSPACE_CONCURRENCY`) remain useful
  complements.
- **System-level interception**: `LD_PRELOAD` is fragile and misses static
  binaries; seccomp breaks `sudo` and stalls the session if its supervisor dies;
  ptrace breaks strace and gdb; eBPF, fanotify and audit need root.
- **systemd's native slots** (`ConcurrencySoftMax`): they work on systemd units.
  Moving a job into one would take it out of its session's group, hence an
  admission of the prefix's own.
- **Vercel Workflow**: it makes application functions durable; it does not run
  shell commands and does not see memory.
- **A coordinator that authorises every command**: a bottleneck, a token cost
  per request, and everything waits while it is busy.
- **A home-made message transport** and **agent teams**: Claude Code's messaging
  between sessions is enough, and agent team members are started by the lead,
  not opened by hand in their own worktrees.
