# Commands

## Installing

Requirements:

- Linux with cgroup v2 (`stat -fc %T /sys/fs/cgroup` prints `cgroup2fs`).
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

There is no systemd unit and no installer yet: `watch` is started by hand (see
below).

## orchestrator launch

```sh
orchestrator launch -- claude [claude's arguments]
```

Starts a command, normally `claude`, as an orchestrated session. Arguments
after `--` go to the command unchanged.

- Asks the systemd user manager, over the user bus with `busctl`, for a
  delegated scope `orchestrator-<pid>-<ms since the epoch>.scope` in
  `orchestrator.slice`, waits until it is in it (2 s at most for both), moves
  into the scope's `main/` leaf, enables the `cpu`, `memory` and `pids`
  controllers for the sub-groups, sets `CLAUDE_CODE_SHELL_PREFIX` to the
  `orchestrator-prefix` next to it, then replaces itself with the command
  (same pid, same environment, the command's exit code).
- Prints nothing when it succeeds, except
  `orchestrator: replacing CLAUDE_CODE_SHELL_PREFIX=<previous value>` when one
  was already set to something else.
- On any failure before that (no `busctl`, no user bus, no answer within 2 s,
  a refused scope, a cgroup error, the prefix binary missing), it prints one
  line `orchestrator: <reason>; the session runs unorchestrated` and runs the
  command unchanged.
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
- It then replaces itself with `bash -c '<command line>'`.
- Outside an orchestrated session, or on any error, it runs the command
  unchanged. Its own failures go to `prefix.log` in the runtime directory,
  never to the terminal: its standard error is the command's. Only admission
  notices are written there, on purpose.
- Called with more than one argument, it runs them as a command, unplaced and
  unadmitted, and logs it; with none, it exits successfully.

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
  directory and holds each session's claude process (a pidfd). Five seconds
  after one exits, it looks for orphans. A scan every
  `--orphan-interval-secs` catches the rest. Each orphan group is reported
  once, in an `orphans` event.
- The end of a job of an orchestrated session (inotify on the job groups):
  it reads the job's `memory.peak`; for a Bash call, it appends the peak with
  the command to `measurements.jsonl` and learns it, then removes the empty
  group. A sweep every minute catches what inotify missed.

| Option | Default | What it sets |
| :- | :- | :- |
| `--proc-root <dir>` | `/proc` | Where processes and `meminfo` are read |
| `--sessions-dir <dir>` | `$CLAUDE_CONFIG_DIR/sessions`, else `~/.claude/sessions` | Claude Code's sessions directory |
| `--state-dir <dir>` | `$XDG_STATE_HOME/orchestrator`, else `~/.local/state/orchestrator` | Where learned peaks are kept |
| `--runtime-dir <dir>` | `$XDG_RUNTIME_DIR/orchestrator` | Where events and measurements are written |
| `--stall-ms <ms>` | 200 (1 to 2000) | Memory stall within 2 s that makes a pressure event |
| `--cooldown-secs <s>` | 60 | Minimum time between two memory pressure events |
| `--orphan-interval-secs <s>` | 300 | Time between two orphan scans when no session ends |

Facts that matter when starting it:

- Jobs are measured only when `watch` runs under the user's systemd manager:
  its own cgroup (`cat /proc/self/cgroup` in the shell that starts it) must
  lie below `user@<uid>.service`, as a command started with
  `systemd-run --user` does. Otherwise it prints `orchestrator: no systemd
  user manager above this process; jobs are not collected` and only reports
  events.
- The prefix always uses the default runtime and state directories, read
  from the session's environment. A `watch` given other `--runtime-dir` or
  `--state-dir` values does not find the prefix's job records, and learns
  peaks admission never reads.
- `CLAUDE_CONFIG_DIR` must be the one the sessions run with, or
  `--sessions-dir` must point at their sessions directory.
- Nothing stops a second `watch`: it would append every event twice. Run one
  per user.
- systemd removes a session's job groups when the session ends: a Bash call
  that ended while no `watch` ran may be lost to learning.
- Its messages go to its standard error; see events.md.

## orchestrator sessions

```sh
orchestrator sessions [--proc-root <dir>] [--sessions-dir <dir>]
```

Prints, for a human, the available memory, then each live Claude Code session
(orchestrated or not) with the resident memory of its processes and its
largest process, then the orphaned processes. A process belongs to the live
session whose claude process it descends from, else to the live session named
by its `CLAUDE_CODE_SESSION_ID`. Orphans are processes carrying a
`CLAUDE_CODE_SESSION_ID` whose session is no longer live, folded into their
topmost orphaned ancestor.

```
Available memory: <MB> MB

SESSION                             RSS MB  LARGEST PROCESS
<session name>                       <MB>  <MB> MB  pid <pid>  <command line>

ORPHANS (session gone)
   <MB> MB  <n> process(es)  pid <pid>  session <first 8 of id>  <command line>
```

or `No orphaned process.` The command line shown is the start (70 characters)
of the process's real command line, which can hold credentials. The session
name is the one Claude Code records in its session file.

## orchestrator peaks

```sh
orchestrator peaks [--state-dir <dir>]
```

Prints what `watch` has learned, per repository, heaviest command first, or
`No peak learned yet.`

```
/home/u/project/.git
  PEAK MB  ID        COMMAND         LATEST CALLS, MB (* ALONE)
     2210  1f0c9a2e  pnpm typecheck  2210* 2350 2290*
       12  9b01ff00  git status      12*
```

- The repository key is the git common directory (shared by all worktrees of
  a repository, such as `/home/u/project/.git`), or the directory itself
  outside a repository.
- `PEAK MB` is the command's expected peak, the value admission compares with
  `heavy_mb`.
- `ID` is the start of the command's hash; it tells apart two commands with
  the same label.
- `COMMAND` is the label: the command's words as recognised, a here-document
  shown as `<<…`, cut to 60 characters.
- `LATEST CALLS` lists the peaks of the latest calls holding the command, at
  most five, newest first; `*` marks a call the command ran alone in.

How a command is recognised and its expected peak computed: see
configuration.md, "How admission decides".
