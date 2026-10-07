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
  `busctl`, which ships with systemd. `sessions` and `watch` only read `/proc`
  and Claude Code's session files.
- A Rust toolchain, Rust 1.88 or later.

From a clone of <https://github.com/hvn-p/orchestrator>:

```sh
cargo install --path .
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
  bash whatever shell Claude Code picked: Claude Code uses `CLAUDE_CODE_SHELL`
  when it names a bash or zsh binary, else `$SHELL` when it is bash or zsh,
  else the first zsh, then bash, it finds. When that is zsh, commands still
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

| Option | Default | What it sets |
| :- | :- | :- |
| `--proc-root <dir>` | `/proc` | The proc file system to read: processes, `meminfo`, `pressure/memory`, and `watch`'s own cgroup (`self/cgroup`), which decides whether jobs are collected |
| `--sessions-dir <dir>` | `$CLAUDE_CONFIG_DIR/sessions`, else `~/.claude/sessions` | Claude Code's sessions directory |
| `--state-dir <dir>` | `$XDG_STATE_HOME/orchestrator`, else `~/.local/state/orchestrator` | Where learned peaks are kept |
| `--runtime-dir <dir>` | `$XDG_RUNTIME_DIR/orchestrator`, else `/run/user/<uid>/orchestrator` | Where events and measurements are written and job records read |
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
  the manager's environment, not the shell's: a `CLAUDE_CONFIG_DIR` the
  sessions use has to be passed with `--setenv=CLAUDE_CONFIG_DIR`. Its
  messages go to the user journal (`journalctl --user -u
  orchestrator-watch`); `systemctl --user stop orchestrator-watch` stops it.

Facts that matter when starting it:

- The prefix always uses the default runtime and state directories, read
  from the session's environment. A `watch` given other `--runtime-dir` or
  `--state-dir` values does not find the prefix's job records, and learns
  peaks admission never reads.
- `CLAUDE_CONFIG_DIR` must be the one the sessions run with, or
  `--sessions-dir` must point at their sessions directory. When that
  directory does not exist yet as `watch` starts, session ends are found only
  by the periodic scan until `watch` restarts.
- Nothing stops a second `watch`: it would append every event twice. Run one
  per user.
- systemd removes a session's job groups when the session ends: a Bash call
  that ended while no `watch` ran may be lost to learning. Admission keeps
  working meanwhile, from the peaks already learned.
- Its messages go to its standard error, and it stops at start on a few
  errors; see events.md.

## orchestrator sessions

```sh
orchestrator sessions [--proc-root <dir>] [--sessions-dir <dir>]
```

Prints, for a human, the available memory, then each live Claude Code session
(orchestrated or not) with the resident memory of its processes and its
largest process, then the orphaned processes, largest first in both lists. A
process belongs to the live session whose claude process it descends from,
else to the live session named by its `CLAUDE_CODE_SESSION_ID`. Orphans are
processes carrying a `CLAUDE_CODE_SESSION_ID` whose session is no longer live,
folded into their topmost orphaned ancestor. The options and their defaults
are `watch`'s.

```
Available memory: <MB> MB

SESSION                             RSS MB  LARGEST PROCESS
<session name>                       <MB>  <MB> MB  pid <pid>  <command line>

ORPHANS (session gone)
   <MB> MB  <n> process(es)  pid <pid>  session <first 8 of id>  <command line>
```

or `No orphaned process.` The command line shown is the start (70 characters)
of the process's real command line, which can hold credentials. The session
name is the one Claude Code records in its session file. It exits with
`Error: …` and status 1 when `<proc root>/meminfo` cannot be read.

## orchestrator peaks

```sh
orchestrator peaks [--state-dir <dir>]
```

Prints what `watch` has learned, per repository, heaviest command first, or
`No peak learned yet.` `--state-dir` defaults to
`$XDG_STATE_HOME/orchestrator`, else `~/.local/state/orchestrator`, as for
`watch`.

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
