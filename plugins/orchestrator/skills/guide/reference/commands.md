# Commands

## Installing

Requirements:

- Linux with cgroup v2 (`stat -fc %T /sys/fs/cgroup` prints `cgroup2fs`).
- For measuring Bash calls: Linux 5.19 or later, whose cgroups have
  `memory.peak`, and the `memory` controller in the session's scope. `launch`
  enables `cpu`, `memory` and `pids` there, among those the user's systemd
  manager provides.
- For `memory_pressure` events: Linux 6.4 or later, the first to let an
  unprivileged process set the pressure trigger `watch` uses, and
  `/proc/pressure/memory` present.
- For `launch`: a systemd user manager that delegates to user scopes, and
  `busctl`, which ships with systemd. `sessions` and `watch` need neither
  `busctl` nor a scope.
- A Rust toolchain, Rust 1.88 or later for the dependency versions the
  repository locks.
- For coordinators: a signed-in `claude` on the `PATH` of what starts them,
  `watch` for event runs, the shell for `setup` and `coordinator`.

From a clone of <https://github.com/hvn-p/orchestrator>:

```sh
cargo install --locked --path .
```

This installs two binaries side by side in Cargo's bin directory
(`~/.cargo/bin` by default): `orchestrator` and `orchestrator-prefix`. `launch`
finds the prefix next to its own executable, so both must stay together.
Claude Code runs the prefix's path unquoted for hooks (through `/bin/sh -c`):
the path must hold no space nor character special to `sh`.
`cargo uninstall orchestrator` removes both.

orchestrator ships no systemd unit: `watch` is started by hand (see "Starting
watch" below).

`orchestrator --version` prints the version; `orchestrator help <command>` or
`orchestrator <command> --help` lists a command's options.

## orchestrator launch

```sh
orchestrator launch [--] claude [claude's arguments]
```

Starts a command, normally `claude`, as an orchestrated session. Every
argument after the command goes to it unchanged, options included; `--` is
optional.

- Asks the systemd user manager, over the user bus with `busctl`, for a
  delegated scope `orchestrator-<pid>-<ms since the epoch>.scope` in
  `orchestrator.slice`, waits until it is in it (2 s at most for both), moves
  into the scope's `main/` leaf, enables the `cpu`, `memory` and `pids`
  controllers the scope has for its sub-groups, sets
  `CLAUDE_CODE_SHELL_PREFIX` to the `orchestrator-prefix` next to it, then
  replaces itself with the command (same pid, same environment, the command's
  exit code).
- Prints nothing when it succeeds, except
  `orchestrator: replacing CLAUDE_CODE_SHELL_PREFIX=<previous value>` when one
  was already set to something else.
- On any failure before that (no `busctl`, no user bus, no answer within 2 s,
  a refused scope, a cgroup error, the prefix binary missing), it prints one
  line `orchestrator: <reason>; the session runs unorchestrated` and runs the
  command unchanged.
- When systemd grants the scope after those 2 s, it still moves the process
  into it: the session then runs in the scope itself, with no `main/` leaf
  and no prefix, unorchestrated.
- When the command cannot be started (not on `PATH`, for instance), it prints
  `Error: running <command>` and the cause, and exits with status 1.
- The scope is collected by systemd once its last process ends, with every
  job group in it.
- A launch from inside an orchestrated session gets a new scope of its own.

## orchestrator-prefix

Not run by hand: Claude Code runs it as `orchestrator-prefix '<command line>'`
because `launch` set `CLAUDE_CODE_SHELL_PREFIX`.

- Inside an orchestrated session, it creates a job group in the session's
  scope and moves into it: `job-bash-<pid>-<ms>` for a Bash call or a `!`
  command, `job-other-<pid>-<ms>` for a hook, the status line or an MCP server.
  A Bash call is told apart by its invocation, which sources Claude Code's
  shell snapshot or records its working directory with `pwd -P >|`.
- For a Bash call, it writes a job record (the invocation and the working
  directory) for `watch`, then goes through admission (see
  configuration.md).
- It then replaces itself with `bash -c '<command line>'`. The command runs in
  bash whatever shell Claude Code picked. Claude Code's documentation
  (environment variables, `CLAUDE_CODE_SHELL`) says it uses the bash or zsh
  binary `CLAUDE_CODE_SHELL` names when that works, else `$SHELL` when it
  points to bash or zsh, else the first working zsh, then bash, found on the
  `PATH` and in standard install locations. When that is zsh, commands still
  run in bash under orchestrator.
- Outside an orchestrated session, or on any error, it runs the command
  unchanged. Its own failures go to `prefix.log` in the runtime directory,
  not to the terminal: its standard error is the command's. Only admission
  notices are written there, on purpose. A failure is not logged at all while
  the runtime directory does not exist: `watch` creates it when it starts, as
  does an orchestrated Bash call when it writes its job record.
- When bash itself cannot be started, it prints
  `orchestrator-prefix: running bash: <error>` and exits with status 127.
- Called with other than one argument, it logs
  `expected one argument, got <n>: running them unorchestrated`, then runs the
  arguments as a command, unplaced and unadmitted (printing
  `orchestrator-prefix: running the command: <error>` and exiting with status
  127 if it cannot); with none, it exits successfully.

Not through the prefix, so counted with claude in `main/`: exec-form hooks
(`args` set), PowerShell hooks, and helpers Claude Code starts itself, such as
clipboard tools.

## orchestrator watch

```sh
orchestrator watch [options]
```

Runs in the foreground until stopped, sleeping until the kernel reports
something:

- Memory pressure, through a PSI trigger on `/proc/pressure/memory`: when some
  task stalls on memory for `--stall-ms` within a 2 s window, it ranks
  sessions by memory and appends a `memory_pressure` event, at most once per
  `--cooldown-secs`.
- The end of a Claude Code session: it watches Claude Code's sessions
  directory and is told when each session's claude process exits. Five
  seconds later, it looks for orphans. It also looks when it starts, and every
  `--orphan-interval-secs`. Each orphan group is reported once per run of
  `watch`, in an `orphans` event: a restarted `watch` reports the groups
  still alive again.
- The end of a job of an orchestrated session: it is told when a job's group
  empties, reads the job's `memory.peak`, and, for a Bash call, appends the
  peak with the command to `measurements.jsonl` and learns it. It then removes
  the empty group, whatever the job. A sweep every minute catches what it was
  not told.
- With `"wake": true` in the configuration's `coordinator` section: a Bash
  call that admission has held back `wait_secs` makes an `admission_wait`
  event, and `memory_pressure` and `admission_wait` events are queued for a
  coordinator run, which `watch` starts as soon as no coordinator is running
  (coordinator.md). It reads the configuration at each event, so a change
  needs no restart.
- It serves the state and the events over a local API, at
  `<runtime>/api.sock` (api.md). The read commands, `config admission`,
  `setup` and `coordinator` ask it: without a `watch`, they stop with an
  error.

| Option | Default | What it sets |
| :- | :- | :- |
| `--proc-root <dir>` | `/proc` | The proc file system to read: processes, `meminfo`, `pressure/memory`, and `watch`'s own cgroup (`self/cgroup`), which decides whether jobs are collected |
| `--sessions-dir <dir>` | `$CLAUDE_CONFIG_DIR/sessions`, else `~/.claude/sessions` | Claude Code's sessions directory |
| `--state-dir <dir>` | `$XDG_STATE_HOME/orchestrator`, else `~/.local/state/orchestrator` | Where learned peaks are kept, and the coordinator's working directory and journal for the runs it starts |
| `--runtime-dir <dir>` | `$XDG_RUNTIME_DIR/orchestrator`, else `/run/user/<uid>/orchestrator` | Where events and measurements are written, job records read and the API's socket listens, and the coordinator's queue, holder, role and runs |
| `--stall-ms <ms>` | 200 (1 to 2000) | Memory stall within 2 s that makes a pressure event |
| `--cooldown-secs <s>` | 60 | Minimum time between two memory pressure events |
| `--orphan-interval-secs <s>` | 300 (0 counts as 1) | Time between two orphan scans when no session ends |

### Starting watch

Jobs are measured only when `watch` runs under the user's systemd manager:
its own cgroup (`cat /proc/self/cgroup` in the shell that starts it) must lie
below `user@<uid>.service`. Otherwise it prints `orchestrator: no systemd
user manager above this process; jobs are not collected` and only reports
events.

- A terminal opened from a desktop session often lies there already. A login
  shell over SSH does not: it runs in the login's `session-<n>.scope`.
- `systemd-run --user --scope orchestrator watch` runs it below the user
  manager from any shell, in the foreground, with the shell's environment.
- `systemd-run --user --unit=orchestrator-watch orchestrator watch` runs it
  in the background, as a service of the user manager. A service starts with
  the manager's environment, not the shell's (`systemctl --user
  show-environment` prints it). When the sessions run with a
  `CLAUDE_CONFIG_DIR`, `XDG_STATE_HOME` or `XDG_RUNTIME_DIR` other than the
  manager's, pass each one, as in `--setenv=CLAUDE_CONFIG_DIR` (a name alone
  takes the shell's value). Otherwise `watch` reads another sessions
  directory, or works in other directories than the prefix, with no
  message. Coordinator runs need `claude` on that environment's `PATH`
  (`--setenv=PATH`). Its messages go to the user journal (`journalctl --user -u
  orchestrator-watch`); `systemctl --user stop orchestrator-watch` stops it.

Facts that matter when starting it:

- The prefix always uses the default runtime and state directories, read
  from the session's environment. A `watch` given other `--runtime-dir` or
  `--state-dir` values, or started with another `XDG_RUNTIME_DIR` or
  `XDG_STATE_HOME`, does not find the prefix's job records, or learns peaks
  admission never reads. The commands ask the `watch` serving the default
  runtime directory: one started with another does not answer them. `setup`, `coordinator` and `coordinator note`, a
  run's own notes included, also keep to the default directories: they do
  not see that `watch`'s queue and holder, and a run's notes go to the
  default journal, not the one its next runs are briefed with.
- `CLAUDE_CONFIG_DIR` must be the one the sessions run with, or
  `--sessions-dir` must point at their sessions directory. When that
  directory does not exist yet as `watch` starts, session ends are found only
  by the periodic scan until `watch` restarts.
- One `watch` serves a runtime directory: a second one stops at start with
  `Error: another orchestrator watch answers at <runtime>/api.sock`.
- systemd removes a session's job groups when the session ends: a Bash call
  that ended while no `watch` ran may be lost to learning. Admission keeps
  working meanwhile, from the peaks already learned.
- Its messages go to its standard error, and it stops at start on a few
  errors; see events.md.

## orchestrator sessions

```sh
orchestrator sessions [--heads]
```

Prints, for a human, the available memory, then each live Claude Code session
(orchestrated or not) with the resident memory of its processes and its
largest process, then the orphaned processes, largest first in both lists. A
process belongs to the live session whose claude process it descends from,
else to the live session named by its `CLAUDE_CODE_SESSION_ID`. Orphans are
processes carrying a `CLAUDE_CODE_SESSION_ID` whose session is no longer live,
folded into their topmost orphaned ancestor. It shows what `watch` reads,
with `watch`'s proc root and sessions directory.

```
Available memory: <MB> MB

SESSION                             RSS MB  LARGEST PROCESS
<session name>                       <MB>  <MB> MB  pid <pid>  <command line>

ORPHANS (session gone)
   <MB> MB  <n> process(es)  pid <pid>  session <first 8 of id>  <command line>
```

or `No orphaned process.` The command line shown is the start (70 characters)
of the process's real command line, which can hold credentials. With
`--heads`, it is the head of the command line instead, as events show it
(events.md), never its arguments: what coordinators read. The session
name is the one Claude Code records in its session file. With a wrong or
missing sessions directory, it shows no session, and the processes of live
sessions as orphans. It exits with `Error: …` and status 1 when no `watch`
answers, or when `watch` cannot read the available memory, the processes or
the sessions directory; see events.md.

## orchestrator peaks

```sh
orchestrator peaks
```

Prints what `watch` has learned, per repository, heaviest command first, or
`No peak learned yet.`, from `watch`'s state directory. It exits with
`Error: …` and status 1 when no `watch` answers or `watch` cannot read the
peaks; see events.md.

```
/home/u/project/.git
  PEAK MB  ID        COMMAND         LATEST CALLS, MB (* ALONE)
     2210  1f0c9a2e  pnpm typecheck  2210* 2350 2290*
       12  9b01ff00  git status      12*
```

- The repository key is the git common directory (shared by all worktrees of
  a repository, such as `/home/u/project/.git`), or the directory itself
  outside a repository.
- `PEAK MB` is the command's expected peak. A call's expected peak, which
  admission compares with `heavy_mb`, is the largest among its commands'.
- `ID` is the start of the command's hash; it tells apart two commands with
  the same label.
- `COMMAND` is the label: the command's words as recognised, a here-document
  or here-string shown as `<<…`, cut to 60 characters.
- `LATEST CALLS` lists the peaks of the latest calls holding the command, at
  most five, newest first; `*` marks a call the command ran alone in.

How a command is recognised and its expected peak computed: see
configuration.md, "How admission decides".

## orchestrator admission

```sh
orchestrator admission
```

Prints the admission thresholds, the memory free for admission, the Bash
calls waiting for memory and the heavy calls running with a reservation:

```
Admission: a call expected to peak at 1024 MB or more waits until free memory covers its peak plus 2048 MB, 60 s at most.
Available memory: 5200 MB; running heavy calls still hold 1800 MB of it: 3400 MB free for admission.

WAITING FOR MEMORY
  SESSION                                   WAITED  PEAK MB  NEEDS MB  COMMAND
  api refactor (6f1c2d9e)                     14 s     2210      4258  pnpm typecheck

RESERVED BY RUNNING HEAVY CALLS
  SESSION                                  HOLDS MB  PEAK MB  USES MB  COMMAND
  docs (0a9b8c7d)                              1800     3000     1200  pnpm build
```

- Without thresholds, the first line reads `Admission: off, no admission
  section in <path>.`, or gives the reason the file cannot be read.
- `No call waits for memory.` and `No heavy call runs with a reservation.`
  replace the empty lists.
- A session shows by its name and the start of its id, else by its scope.
  `HOLDS MB` is what the reservation still holds, its expected peak minus
  `USES MB`, what the job group uses now (`?` without a memory controller).
- A waiting call is the one the prefix records while it waits. Reading takes
  no lock: what it shows may be a check behind.

It shows what `watch` reads in its runtime directory and configuration, and
exits with `Error: …` when no `watch` answers or `watch` cannot read
`/proc/meminfo`.

## orchestrator machine

```sh
orchestrator machine
```

Prints what admission thresholds are chosen from, as `watch` reads it, or
exits with `Error: …` when no `watch` answers:

```
Memory: 31264 MB total, 5578 MB available
Swap: 4095 MB total, 16 MB free
CPUs: 12
Memory pressure reports (PSI): available
Controllers the systemd user manager delegates: cpu memory pids
orchestrator.slice: present
```

- `CPUs` counts the CPUs `watch` may run on.
- The PSI line reads `missing, so watch reports no memory pressure` without
  `/proc/pressure/memory`.
- The controllers line adds `(missing <controllers>: sessions cannot be
  measured or slowed down by it)` when one of `cpu`, `memory`, `pids` is
  not delegated, and reads `unknown, this process runs outside it` when
  `watch` runs outside the user's systemd manager.
- `orchestrator.slice: absent, no session launched since boot` until a first
  `launch`.

## orchestrator config

```sh
orchestrator config
orchestrator config admission --heavy-mb <MB> --margin-mb <MB> --max-wait-secs <s>
orchestrator config coordinator [--wake <yes|no>] [--model <model>] [--max-minutes <n>] [--wait-secs <s>] [--language <tag>]
```

`orchestrator config` prints the path of `config.json` and its content, or
`No configuration at <path>.`, then `The coordinators' instructions: <path>`
when that file exists, as `watch` reads them. `config admission` checks the
values against the machine's memory as `watch` reads it. Both exit with
`Error: …` when no `watch` answers; `config coordinator` does not need one.

The two subcommands write one section of `config.json` (configuration.md),
keeping the rest, once the values make sense; otherwise they print
`Error: <why>`, exit with status 1 and leave the file as it was. Both check
the whole `coordinator` section, hand-written values included, so a mistake
there makes `config admission` refuse too. A `config.json` that cannot be
read stops every `config` command with `Error: reading <path>` and its cause,
leaving the file alone. On success they print `Wrote <path>` and the whole
configuration. The file is replaced at once, so the prefix
never reads half of it.

`config admission` takes all three options:

- `--heavy-mb`: above 0, at most the machine's total memory.
- `--margin-mb`: under the machine's total memory.
- `--max-wait-secs`: at most 600.
- With the coordinator waking (`"wake": true`), its `wait_secs` must stay
  under `--max-wait-secs`.

`config coordinator` takes at least one option and starts from these values
when the section does not exist: `wake` no, `model` `claude-sonnet-5-5`,
`max_minutes` 5, `wait_secs` 20, no `language`.

- `--wake`: `yes` or `no`, also `y`/`n`, `true`/`false`, `t`/`f`,
  `on`/`off`, `1`/`0`, in any case.
- `--model`: a model name or alias, made of letters, digits, `.`, `-`, `_`,
  `[` and `]`, at most 100 characters. Whether the account can use it is not
  checked.
- `--max-minutes`: 1 to 60.
- `--wait-secs`: 1 to 600, and under admission's `max_wait_secs` while
  `wake` is on.
- `--language`: a tag of two or three lowercase letters, then up to three
  subtags of 2 to 8 letters or digits joined by `-`: `fr`, `en`, `pt-BR`.
  No option removes it once set: that takes editing `config.json` by hand.

`max_budget_usd` has no option; it is written by hand, above 0.

## orchestrator setup

```sh
orchestrator setup
```

Opens the setup conversation with a coordinator, at the terminal: it asks
and writes the configuration through the commands above (coordinator.md).
Run it again to review the configuration.

## orchestrator coordinator

```sh
orchestrator coordinator
orchestrator coordinator next
orchestrator coordinator note <text>…
```

- Without a subcommand: the setup conversation when `config.json` does not
  exist, else the interactive coordinator (coordinator.md). Both wait for a
  coordinator already running.
- `next`: what the interactive coordinator runs in the background. It marks
  every event in progress done, which closes the batch it gave last time,
  waits until events are pending, takes them (`in_progress`), prints them,
  one JSON object per line under `## Events`, then the state under
  `## State now`, and ends. It does not check which coordinator holds: run
  by hand, it closes the events a run is handling and takes events meant for
  the coordinator that holds.
- `note`: adds its arguments, joined by spaces, to the coordinator's journal,
  one dated line per line of text (coordinator.md).
