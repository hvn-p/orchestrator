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
of its own, and each command it starts in a job group of its own. `orchestrator
watch` measures the memory peak of each finished Bash call, learns it per
repository and command, and removes empty job groups; `orchestrator peaks`
shows what it learned. Once a configuration exists, a Bash call learned as
memory-hungry waits for memory before it runs (admission). Nothing is
throttled yet. `orchestrator sessions` and
`orchestrator watch` still attribute processes to sessions by process ancestry
and report memory pressure and orphaned processes. A first coordinator
exists, once the user enables it: it writes the admission thresholds at
setup, and asks sessions to free memory or tells them why a call waits,
from the state `watch` gathers for it; it pulls no lever. Everything else here is design; the parts validated by
throwaway prototypes are listed under "Measured". What orchestrator relies on
in Claude Code is listed in [claude-code-dependency.md](claude-code-dependency.md),
with the version it was last verified on.

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
   command starts at once. The wait counts toward the Bash call's timeout, so
   it is bounded: past the longest wait, the call runs anyway.
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
- **No full command line in an event**: events name a process by its pid, its
  `comm` and the head of its command line: the program and at most two plain
  words, stopping at the first option, URL or word that could carry data.
  Arguments can hold credentials, and the coordinator hands what it reads to a
  model.
- **Hooks, the status line and MCP servers never queue**: they get their own
  group, to be measured, and start immediately.
- **Claude Code is today's only host, behind a listed boundary**: everything
  orchestrator relies on in it is a contract of
  [claude-code-dependency.md](claude-code-dependency.md), and the code
  relying on one carries its marker. Nothing else of Claude Code is relied
  on. A merge guard keeps the list current, and a manual integration test
  checks it against a new Claude Code version.

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
- **The shell prefix**, `orchestrator-prefix` (exists):
  Claude Code calls it with the full command line as a single argument, for
  every Bash call, hook, status line refresh and MCP stdio server start. It
  creates the sub-group, moves itself in, keeps the command of a Bash call for
  the service, waits for admission when it runs a Bash call, then replaces
  itself with the command. Output and exit code pass through unchanged. Claude
  Code runs the prefix as one quoted path, hence a binary of its own rather
  than a subcommand.
- **`orchestrator watch`**, a systemd user service:
  - exists: it sleeps until the kernel reports something. Memory pressure
    comes from a PSI trigger on `/proc/pressure/memory`; the end of a Claude
    session from a pidfd on its claude process, the session being found
    through inotify on Claude Code's sessions directory, and its orphans are
    looked for five seconds later; both go as JSON lines to `events.jsonl`. A
    scan every five minutes catches orphans no session end announces. Each
    finished job's peak (`memory.peak`), read as soon as
    inotify reports its `cgroup.events` unpopulated, kept with the command of a
    Bash call in `measurements.jsonl` and learned per repository and command
    (see "Recognising a command"), then the job's empty group removed. A
    sweep every minute catches what inotify cannot see, such as a job that
    ended before its watch was in place;
  - planned: `memory.events` and memory pressure (PSI) per job; listening sockets every one or two seconds, attributed to their job;
    classification, levers; the token quota left by the status line.
- **The coordinator** (exists, first version): a fresh Claude Code session,
  started by `watch` with its role (`src/coordinator/role.md`) appended to the
  system prompt, for each batch of events that need judgment. It is never
  resumed: what it must remember between wakes lives in files, a journal of
  its actions and of the exchanges still open (in its directory under the
  state directory, which is also its working directory), and the user's
  priorities (`priorities.md` next to the configuration).
  - **Briefed by code**: gathering the state takes no judgment, so the
    prompt carries it: the events, the machine, the sessions (as
    `orchestrator sessions --heads` shows them), the calls waiting for memory
    and the reservations, the heavy commands learned, the latest lines of
    the journal and the priorities. The coordinator decides, messages and
    notes (`orchestrator coordinator note`, which dates the line and keeps
    the latest ones); it runs a read command only for what the prompt lacks.
  - **Consent**: nothing spends tokens until the configuration has a
    `coordinator` section. `orchestrator setup`, run by the user, asks which
    model the coordinator runs with (Sonnet 5.5 by default; `--model`
    answers without asking), writes the section, then starts a coordinator
    that examines the machine and writes the admission thresholds through
    `orchestrator config admission`, which refuses values that make no sense
    on the machine.
  - **What wakes it**: `memory_pressure`, and `admission_wait`, which
    `watch` writes once per Bash call that admission has held back for the
    configured time. Orphans do not wake it yet.
  - **One at a time**: a holder file names the process running the
    coordinator by pid and start time, under a file lock; `watch`'s runs,
    `setup` and `orchestrator coordinator` all take it, and it frees itself
    when that process ends. Events arriving meanwhile wait in a queue,
    pending ones merge (memory pressure into the latest, an admission wait
    once per call), and the next run gets them together as soon as the
    current one ends. An admission wait whose call has run meanwhile needs
    nothing.
  - **Event status**: a queued event is pending, in progress once a
    coordinator takes it, done once handled: when the run that took it ends
    successfully, or when the interactive coordinator asks for the next
    batch. A coordinator that ends with events in progress gives them back,
    pending for the next one. A failed run delays the next by a minute, and
    an event three failed runs took is closed.
  - **Talking to it**: `orchestrator coordinator` opens an interactive one. It
    takes the coordinator, once any running one has ended, and receives the
    events itself, waiting for them with `orchestrator coordinator next` in
    the background, which prints them with the state, for as long as it
    stays open. When it closes, `watch` starts fresh runs again with what
    came in meanwhile and what it left unhandled.
  - **What it may do**: read more of the state, note in its journal and,
    outside setup, message sessions with Claude Code's messages between
    sessions. It loads
    none of the user's settings, hooks, plugins, MCP servers or CLAUDE.md,
    only the project settings of its own directory. A run started by `watch`
    or `setup` is denied anything else without asking (`dontAsk`); an
    interactive coordinator asks its user, for the thresholds and the
    priorities among others. Both modes prompt for permissions, like the
    sessions they message. A run for events cannot change the thresholds: it
    reports them when they look wrong.
  - **Bounds**: each run has the configured model and a time limit, past
    which `watch` stops its process group: a guard against a stuck run, not
    a spending limit. No spending cap by default; `max_budget_usd` sets one,
    against Claude Code's estimate at API list price, which is not what a
    subscription counts. `runs.jsonl` in the runtime directory keeps each
    run's turns, seconds and tokens, then its reply.
  - **Later**: setting priorities between waiting calls, slowing down,
    pausing or stopping, negotiating a slot when the limit of long-running
    servers is reached, orphans, and answering "who is working on X?".
- **`orchestrator sessions`** (exists): memory per session and orphaned
  processes, for a human.
- **`orchestrator peaks`** (exists): the learned peaks, per repository, for a
  human: each command's expected peak, its label and its latest calls.
- **`orchestrator admission`**, **`orchestrator machine`**,
  **`orchestrator config`** (exist): the calls waiting for memory and the
  reservations of running ones; what the configuration is chosen from; the
  configuration, and a validated write of its admission thresholds. With
  `orchestrator sessions --heads`, they are how the coordinator reads the
  state.

Runtime data lives in `$XDG_RUNTIME_DIR/orchestrator/`: in memory, cleared at
reboot, never versioned. Learned peaks live in `$XDG_STATE_HOME/orchestrator/`
and survive a reboot, the configuration in `$XDG_CONFIG_HOME/orchestrator/`.

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

The prefix admits a Bash call after moving into its job group and before
replacing itself with the shell:

- The call's expected peak is the largest of its commands' (see "Recognising
  a command"), never their sum: a call has a single peak, so a filter learned
  only beside a heavy command carries that same peak.
- Free memory is the kernel's `MemAvailable` minus the reservations. A
  reservation is a file in the runtime directory naming the job group and its
  expected peak. It holds nothing once the group is empty or gone, and the
  next check removes it. Counting and reserving happen under one file lock, so
  calls arriving together never count on the same memory.
- A waiting call checks again as soon as inotify reports a change in the
  `cgroup.events` of a job holding a reservation, which is how memory mostly
  frees up, and every second otherwise: available memory has no notification.
- It writes one notice to its standard error when it starts waiting and one
  when it runs; Claude reads them with the call's output. A notice names the
  call by the label of its first heavy command that has run alone (its peak
  was measured, not shared), else of its first heavy command, and gives the
  expected peak, the memory needed and the memory free.
- After the longest wait, the call runs anyway, with its reservation, and its
  notice says memory is still short.

### Configuration

The thresholds live in `$XDG_CONFIG_HOME/orchestrator/config.json`, by default
`~/.config/orchestrator/config.json`. The coordinator is meant to write it on
its first start, adapted to the machine; until it exists, a human does.
Without the file, or without its `admission` section, nothing waits. A file
that cannot be read whole, an unknown field included, lets everything through
too, and the error goes to `prefix.log` in the runtime directory.

- `admission.heavy_mb`: a call whose expected peak reaches it waits for
  memory.
- `admission.margin_mb`: free memory kept on top of the expected peak.
- `admission.max_wait_secs`: the longest a call waits before it runs anyway.

The `coordinator` section enables the coordinator: `model`, the user's
choice; `max_minutes` per run; `wait_secs`, how long admission holds a call
before `watch` reports it; and optionally `max_budget_usd` per run.

### Recognising a command

The command Claude wrote reaches the prefix as the argument of an `eval` near
the end of the invocation. Both the invocation and that argument are parsed as
shell, with brush-parser.

Claude writes the same command in many forms. Admission recognises it the way
Claude Code matches its Bash permission rules. The call is split into simple
commands at `&&`, `||`, `;`, `|`, `|&`, `&` and newlines. What does not change
the work is then removed: the wrappers `timeout`, `time`, `nice`, `nohup`,
`stdbuf`, `command` and `builtin` with their options, leading environment
assignments, redirections, and `cd`, whose target still decides the
repository. So `cd app && timeout 300 pnpm exec vitest run X 2>&1 | grep FAIL`
yields `pnpm exec vitest run X` and `grep FAIL`.

- The bodies of brace groups, subshells, loops and conditionals are commands
  of the call. A `cd` inside a subshell, a pipeline or a background command
  stays there. A function definition runs nothing.
- A command substitution (`$(…)`) stays part of the word that holds it.
- A here-document or here-string is the command's input: like the script of
  `python3 -c`, it decides the work, so it stays with the command.
- After a `cd` whose target only running the call would tell (`cd "$dir"`,
  `cd -`), or one into a directory that does not exist (it failed, so what
  follows did not run), the commands are not learned.

Each remaining command is learned word for word, unquoted. Unlike a permission
rule, no wildcard widens the match: `vitest run` (the whole suite) and
`vitest run one.test.ts` stay distinct, since they do not weigh the same. The
repository is the git common directory, shared by all worktrees, found by
walking up from the command's directory without starting git; outside a
repository, the directory itself.

A call holds several commands but has a single peak: the command that carries
it is the one found in heavy calls and never in light ones. A call's peak
bounds each of its commands, and measures exactly a command that ran alone. A
command's expected peak is therefore the smallest peak among its last five
calls, walking back from the latest and stopping at the latest call it ran
alone in.

Where Claude Code asks when in doubt, admission lets the command start: a call
that cannot be parsed teaches nothing, and an unknown command starts at once.

Peaks are kept in `$XDG_STATE_HOME/orchestrator/peaks/` so that they survive a
reboot. A command is stored under a SHA-256 of its repository, words and
input, with a label for display: its words as recognised, joined by spaces,
a here-document marked rather than included, cut to 60 characters. A
repository keeps its 2,000 most recently seen commands in sixteen files,
chosen by the hash, so that a lookup reads one small file.

The label puts command text on disk, and that is deliberate. The store only
ever holds commands Claude wrote: text that already went through the model,
and that Claude Code already keeps in clear in its transcripts under
`~/.claude/projects/`. A command that fetches a secret
(`"$(az keyvault secret show …)"`, `"$TOKEN"`) holds the call, not the value,
since the prefix records the text before the shell expands it. Command lines
read from `/proc` are another matter: any process may carry one, an MCP
server started with a connection string for instance, and it never went
through the model. Those stay out of events (see Principles).

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
- The command Claude wrote reaches `eval` quoted in more than one way (single
  quotes, or bare when it is one word), followed by `< /dev/null` unless it
  reads its own input; Claude Code may add other setup lines before it, on
  several lines. Claude Code keeps a `cd` within the project for the next
  Bash call.
- Parsing a real invocation and its command with brush-parser takes about
  0.25 ms in a fresh process; looking a command up in a repository holding
  2,000 learned commands about 0.05 ms, finding the repository included
  (release build).
- Admission, with a simulated session calling the release build of the
  prefix the way Claude Code does:
  - Outside an orchestrated session, the prefix adds about 0.7 ms to
    `bash -c`; inside one, about 1 ms (its job group and record), with or
    without a configuration. Recognising the call and looking its commands up
    adds about 0.1 ms. Linking brush-parser makes the prefix 1.5 MB, from
    0.7 MB.
  - With memory for one call learned at 2000 MB, a second one waits for the
    first's job to end and starts 7 to 10 ms after the first's last output.
    With memory for two, two run together and a third waits for the first of
    them to end.
  - `MemAvailable` drifts by up to 350 MB within a few seconds while other
    sessions work: the margin has to exceed that drift.
  - In a headless Claude Code session, a Bash call recognised as heavy waited,
    and the model quoted both notices from the call's output.
- `CLAUDE_CODE_SHELL_PREFIX` is run as one quoted path: a prefix holding an
  argument fails with "not found". Bash calls and MCP servers are started by
  claude itself, hooks through `/bin/sh -c`.
- When a session ends, systemd removes its scope together with every job group
  in it: a job's peak has to be read while its session lives.
- inotify reports the creation of a job group in a session scope, each change
  of the job's `cgroup.events` (`populated 1`, then `populated 0` within a
  millisecond of the job's end) and the group's removal.
- `CLAUDE_CODE_SESSION_ID` is inherited by the commands a session runs, but keeps
  the old id after `/clear` or a resume: ancestry is checked first.
- The kernel process connector delivers fork, exec and exit events to an
  unprivileged process on this kernel.
- An unprivileged process can arm a PSI trigger on `/proc/pressure/memory`
  with a 2 s window, not a 1 s one. The user manager's own `memory.pressure`
  refuses it (permission denied). A trigger set at 50 ms fired within a second
  of a process allocating 300 MB under `MemoryHigh=50M`.
- `pidfd_open` works unprivileged on any process of the user; the descriptor
  becomes readable when the process exits.
- Coordinator loop, first prototype (the Monitor tool on an events file): 11 s
  from the event to the message reaching the session, no token spent between
  events. A Monitor expires after 30 min; re-arming it costs about 0.07 USD and
  handling an event about 0.09 USD, as estimated by Claude Code at API list
  price, which a subscription does not pay.
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
  `BASH_DEFAULT_TIMEOUT_MS` and `BASH_MAX_TIMEOUT_MS`. A foreground call that
  reaches its timeout is moved to the background, not stopped, unless it
  starts with `sleep`.
- `claude agents --json` is the supported way to read session state from
  outside Claude Code; the files of `~/.claude/sessions/` are not documented.
- Bash permission rules are matched after splitting compound commands and
  stripping a fixed list of wrappers and known-safe environment assignments.
  The same program invoked in another form (`/usr/bin/curl`, `sh -c '…'`) is
  not recognised, and a command that cannot be parsed prompts.
- A `PreToolUse` hook can rewrite the Bash command (`updatedInput`), but cannot
  wait past its own timeout; the tool call then proceeds.
- An appended system prompt is reused on resume until the conversation is
  compacted, so a launcher passes it again on every resume.

Coordinator spike, Claude Code 2.1.291, Haiku, October 2026:

- Claude Code's supervisor (`claude daemon`) hosts background sessions
  (`claude --bg`) without a terminal. Started on demand, it detaches but stays
  in the cgroup of whatever started it, with every session it hosts, and
  exits once idle with no client. `claude daemon run` keeps one in the
  foreground: under `orchestrator launch`, its sessions carry the prefix and
  their commands land in job groups, all sessions sharing its one scope.
- `processWrapper` (or `CLAUDE_CODE_PROCESS_WRAPPER`, user settings only) set
  to `orchestrator launch --` gave the supervisor, each background session,
  its terminal host and the standby session the supervisor keeps ready a
  scope of their own, with the prefix. Sessions started from a terminal are
  not covered.
- A background session waiting on a background command it started reads
  `working` in `claude agents --json`, so the supervisor keeps its process.
  Appending an event woke it in 3.5 to 6 s. Its appended system prompt and its
  waiting command both survived `claude respawn`.
- A background session, and a `claude -p` one, sent a message to another
  session with `SendMessage`, without a permission prompt. Claude Code holds a
  message when the receiver's permission mode is stricter than the sender's.
- `claude agents --json` lists interactive and background sessions, but gives
  no process start time; background sessions also write
  `~/.claude/sessions/<pid>.json`.
- Cost as Claude Code estimates it at API list price, which a subscription
  does not pay: a background coordinator, about 0.09 USD to start and
  0.02 USD per event, nothing while idle; a resumed `claude -p` run, about
  0.007 USD per event. Memory: about 280 MB per background session, plus its
  terminal host (87 MB), the supervisor (145 MB) and the standby session the
  supervisor keeps ready (220 to 370 MB).

Coordinator, first version, Claude Code 2.1.291, Haiku and Sonnet 5.5,
October 2026, with scratch directories and sessions:

- With `--setting-sources project`, a run's context held none of the user's
  CLAUDE.md: about 4,000 tokens instead of 12,000. Hooks in its own
  directory's `.claude/settings.json` still ran.
- `--tools` takes `SendMessage` and `ListAgents`. In `dontAsk` mode, a
  `Bash(<command>)` rule ran exactly that command and nothing else; an
  `Edit(//<path>)` rule allowed writing that file only, while a
  `Write(//<path>)` rule is ignored with a warning;
  `blockReadsOutsideWorkingDirectories` denied a `cat` outside.
- A `claude -p` session in `dontAsk` mode messaged another one by the name
  given with `--name`; the receiver quoted the message after its running
  Bash call ended. Claude Code refuses a bare `sleep 30` as a Bash call, and
  a `-p` run waits for a standard input left open.
- End to end, with the real prefix holding a scratch session's Bash call:
  `watch` wrote `admission_wait` 8 s into the wait, as configured, and
  started a run that messaged that session, which quoted the message.
  Reading the state itself, command by command, a Haiku run took 9 to 12
  turns and 33 to 48 s, on a configuration made absurd on purpose (a margin
  of 31 GB). Briefed by `watch`, on the same scenario, tokens summed over
  the run:

  | Run | Model | Turns | Time | Input | From cache | Output |
  | :- | :- | -: | -: | -: | -: | -: |
  | `admission_wait` | Sonnet 5.5 | 3 | 8 s | 10,703 | 9,938 | 697 |
  | `admission_wait` | Haiku | 3 | 18 s | 14,185 | 12,547 | 1,656 |
  | setup, no configuration | Sonnet 5.5 | 3 | 10 s | 7,825 | 7,122 | 1,062 |
  | setup, no configuration | Haiku | 3 | 17 s | 11,690 | 21,734 | 1,328 |

  Sonnet 5.5 chose sensible thresholds (1,000 MB heavy, 1,500 MB margin,
  30 s on 31 GB) and, on the event, left the absurd ones alone and noted
  what it would change.
- Organization instructions set for the account still reach a coordinator,
  whatever `--setting-sources`; one run replied in their language.
- An interactive coordinator under a terminal started `orchestrator
  coordinator next` in the background and went idle; when `watch` queued an
  event, the command ended, the session woke and handled it, and started the
  command again, which marked the event done. While it was open, `watch`
  started no run; after it closed, `watch` started one for the next event.
  Leaving it asks for confirmation, since a background task runs. Its
  transcript held no usage figures to measure.
- Allowed to write the thresholds during an event run, Haiku rewrote them
  after one call (a heavy threshold of 60 MB on 31 GB of memory); an
  interactive one did so unasked. Hence writes at setup only, or asked of
  the user.

## Known gaps

- **Outside the session's group**: Docker containers, anything started through
  `systemd-run --user`, services activated over D-Bus, an `xdg-open` handed to an
  already running browser. A shared service counts for the session that started
  it.
- **Freezing frees no RAM** by itself; with little swap it only stops growth.
- **systemd-oomd**: some distributions arm it on `user@.service` (kill above 50 %
  memory pressure for 20 s on the reference machine). Throttling too hard might
  trigger it: plausible, not verified.
- **The admission wait counts toward the Bash timeout**, which the prefix
  cannot see: only the longest wait bounds it.
- **A heavy call that leaves a process running**, such as a server started in
  the background, keeps its reservation, net of what its group uses, until
  that process ends.
- **Processes Claude Code starts without the prefix** are counted with claude in
  `main/`.
- **Claude Code's own memory cap** (`CLAUDE_CODE_TOOL_MEMORY_LIMIT`) kills a
  session's commands past a size, with a memory cgroup of its own. It must
  stay off under orchestrator: it refuses work and competes with the job
  groups.
- **Background sessions**: Claude Code's supervisor (`claude daemon`) starts
  on demand in the cgroup of whatever started it, and hosts every background
  session there. Unless `processWrapper` routes it through `orchestrator
  launch`, those sessions are not orchestrated.
- **A run's answers**: a coordinator run ends with its reply, so a session
  cannot answer it; the next run checks the effect in the state instead.
  Only an interactive coordinator gets answers.
- **A run outlives a restarted `watch`**: the new `watch` waits for it to
  end, but no longer enforces its time limit.
- **The coordinator's bounds are Claude Code's**: what it may run rests on
  Claude Code's permission rules, not on a sandbox; organization
  instructions reach it too.

## Open questions

- Maximum hook timeout: two readings of the documentation disagree (30 s for
  `PreToolUse`, 600 s by default for command hooks). To retest.
- Learning: the packages of a monorepo share one key, so `pnpm test` run in
  two of them is one command.
- Learning: the expected peak leans low, the smallest of recent calls; does
  the admission margin cover the spread of a command's peak from run to run?
- Admission: the configuration's values are chosen by hand until the
  coordinator writes them. Does a 60 s longest wait leave enough of a 2 min
  Bash timeout to the command itself?
- Claude Code's sessions: keep reading the undocumented session files, or
  move to `claude agents --json`, which starts a process per read and gives no
  process start time to tell a reused pid apart.
- Admission: waiting calls have no order; the first to check once memory
  frees up runs. Reordering them is the coordinator's (see "Levers").
- Long-running servers: how many before the coordinator negotiates.
- Orphans and idle sessions holding resources: reported to the coordinator, or
  released automatically.
- Docker: regulated separately (`docker pause`, `docker update`, attribution by
  compose label), or left out of scope.
- Launching: the short command's name, and whether sessions started by other
  tools (a worktree manager, for instance) go through it. A lead: Claude Code's
  `processWrapper` setting starts every process Claude Code starts itself
  through a launcher, and `orchestrator launch` can be that launcher (see
  "Measured"); sessions started from a terminal would need a `claude` script
  earlier on `PATH`. `orchestrator launch` must then print nothing before it
  replaces itself, which `enter` does not guarantee today.
- Coordinator: a spending policy across runs, from the quota the status line
  reports rather than dollars; whether orphans wake it; whether a run should
  message only sessions orchestrated by `orchestrator launch`.
- `memory.reclaim` on a frozen job: not tested yet.

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
- **A permanent background coordinator** (`claude --bg`): it works (see
  "Measured"), but holds 0.5 to 0.7 GB of memory all the time, on a tool meant
  to spare it.
- **A coordinator resumed from one conversation**: its context grows with
  every event, and a user reopening the conversation would race with `watch`.
  Files keep what it must remember instead.
- **The kernel process connector to see sessions end**: it reports every
  process of the machine, thousands per second during a build. A pidfd on each
  session's claude process reports only what matters.
- **A threshold on available memory**: the kernel cannot report it crossing a
  threshold, so it has to be polled, and low available memory alone does not
  slow anything down. A PSI trigger reports the stall itself; admission, not an
  event, keeps a memory-hungry command from starting when memory is short.
- **Other shell parsers**: tree-sitter-bash needs a C compiler, ran at half
  the speed and leaves unquoting to the caller; yash-syntax parses POSIX shell
  only (it rejects `[[ ]]`) through an asynchronous API; conch-parser has had
  no release since 2019; a parser of our own is a shell grammar to maintain.
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
