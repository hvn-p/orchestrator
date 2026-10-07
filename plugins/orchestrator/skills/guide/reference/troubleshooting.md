# Troubleshooting

Observe first. Most questions are settled by where a command runs, by
orchestrator's files (files.md) and by the messages it printed (events.md).

## Is this session orchestrated?

Run these as Bash calls of the session in question:

```sh
cat /proc/self/cgroup
echo "$CLAUDE_CODE_SHELL_PREFIX"
```

- Orchestrated: the cgroup ends in
  `/orchestrator.slice/orchestrator-<pid>-<ms>.scope/job-bash-<pid>-<ms>`, and
  the variable is the path of `orchestrator-prefix`.
- The cgroup has no `orchestrator.slice`: the session was not started through
  `orchestrator launch`, or `launch` fell back. Its fallback prints one line
  when the session starts, `orchestrator: <reason>; the session runs
  unorchestrated` (reasons in events.md). Common ones: `busctl` missing, no
  user bus in that login, cgroup v1, or `orchestrator-prefix` not next to
  `orchestrator`.
- The cgroup ends in `orchestrator-<pid>-<ms>.scope` with nothing after it:
  `launch` gave up after 2 s and started the session unorchestrated, then
  systemd moved it into the scope anyway. A session launched again gets its
  `main/` leaf and the prefix.
- The scope is there but the call runs in `main/`: the call did not go
  through the prefix, or the prefix could not create its job group. Check the
  variable (something may have replaced it in the session's environment), and
  `prefix.log` for `creating job-bash-…`.
- A Bash call lands in `job-other-*`: the prefix no longer recognises Claude
  Code's Bash invocation, which points at a Claude Code change (see
  "After a Claude Code update").

`systemctl --user list-units 'orchestrator-*.scope'` lists the live session
scopes.

Not orchestrated:

- Sessions started with `claude` alone, including by a tool that runs
  `claude` itself.
- Exec-form hooks (`args` set), PowerShell hooks and helpers Claude Code
  starts itself: they bypass the prefix and are counted with claude in
  `main/`.
- Background sessions: those of `claude --bg` and of agent view, Claude
  Code's screen to dispatch and watch background sessions (`claude agents`).
  Claude Code's background service runs them, started in the cgroup of
  whatever started it; they do not go through `orchestrator launch`.
- What runs outside the session's cgroup even when the session started it:
  Docker containers, units started with `systemd-run --user`, services
  activated over D-Bus.

## Nothing is learned

`orchestrator peaks` lacks a command that ran. `measurements.jsonl` tells
which half failed: no line for the call means it was not measured, a line
means it was measured but not learned.

Not measured:

- `watch` was not running when the call ended, or ran outside the user's
  systemd manager (`no systemd user manager above this process`; see
  commands.md, "Starting watch"). systemd removes a session's job groups when
  it ends, so a missed call stays missed.
- `watch` runs with `--runtime-dir` or `--state-dir` other than the defaults
  the prefix uses.
- The session is not orchestrated (above), or the call is not a Bash call:
  hooks, the status line and MCP servers are never measured.
- The job group has no `memory.peak`: Linux older than 5.19, or no `memory`
  controller in the session's scope. Run
  `ls /sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)` as a Bash call:
  the listing must hold `memory.peak`.
- The prefix could not write the job record: `prefix.log` holds
  `writing …/jobs/…`.
- The call has not ended: it is measured once its job group is empty, so a
  server it started in the background holds it open.

Measured, not learned:

- The call could not be parsed as shell.
- The command ran after a `cd` whose target only running tells
  (`cd "$dir"`, `cd -`), or into a directory that does not exist.
- It was learned under another repository: peaks are per repository (its git
  common directory) and per exact command (configuration.md, "Recognising a
  command").

## Nothing waits

- No `config.json`, no `admission` section, or a file that cannot be read:
  `prefix.log` then holds `reading …/config.json: …` for each Bash call. The
  prefix looks for the file from the environment Claude Code was started with
  (`XDG_CONFIG_HOME`, else `HOME`), which may differ from a terminal's.
- No command of the call has an expected peak at or above `heavy_mb`
  (`orchestrator peaks`). The expected peak is the smallest of the latest
  calls back to the latest one the command ran alone in (configuration.md,
  "Learning"): a heavy call shared with other commands does not raise it
  above its latest run alone, nor above a lighter call since.
- `max_wait_secs` is 0: heavy calls reserve memory but never wait.
- Memory was free: a heavy call that finds enough starts at once, silently.
- The command is not a Bash call of an orchestrated session.
- An error let it through: see `prefix.log`.

## A call waits too often or too long

- The second notice says "memory is still short": its expected peak plus
  `margin_mb` exceeds what the machine freed within `max_wait_secs`.
- The waiting notice says "net of … already running": other heavy calls hold
  reservations until their job groups empty, including a server one of them
  left running in the background.
- The command got lighter: its expected peak follows its latest five calls.
  Removing its repository's directory under `<state>/peaks/` (files.md)
  forgets it at once.
- The call reached its Bash timeout: the wait counts toward it
  (configuration.md, "Choosing values").

## A command behaves as in bash, not zsh

The prefix runs every command with `bash -c`, even when Claude Code uses zsh
(through `CLAUDE_CODE_SHELL`, or a zsh `$SHELL`). zsh syntax then fails in an
orchestrated session. Claude Code builds its commands for bash when
`CLAUDE_CODE_SHELL` names a bash binary.

## No memory_pressure event

- `watch` is not running, or writes to another runtime directory.
- The trigger could not be set up (`no kernel signal for memory pressure`
  when `watch` started), for instance on Linux older than 6.4: no pressure
  event will come.
- The trigger broke (`the memory pressure trigger broke`): no pressure event
  until `watch` restarts.
- No task stalled on memory for `--stall-ms` within 2 s, or an event fired
  less than `--cooldown-secs` ago.

## Sessions or orphans not seen

- `watch` or `sessions` reads another sessions directory than the sessions
  write: `CLAUDE_CONFIG_DIR` differs, or pass `--sessions-dir`. A `watch`
  started as a service with `systemd-run --user` does not get the shell's
  `CLAUDE_CONFIG_DIR` (commands.md, "Starting watch").
- The sessions directory did not exist when `watch` started
  (`no kernel signal for session ends`): orphans then come only from the
  periodic scan, until `watch` restarts.
- A process started outside Claude Code carries no
  `CLAUDE_CODE_SESSION_ID`, so it is never an orphan.
- An orphan group is reported once per run of `watch`, for as long as it
  lives.

## The same orphans reported again

`watch` remembers which orphan groups it reported only while it runs. A
restarted `watch` reports every orphan group still alive, in the scan it
makes as it starts.

## Hooks fail through the prefix

Claude Code runs a hook as `/bin/sh -c '<prefix path> <quoted command>'`,
with the path unquoted: an install path holding a space or a character
special to `sh` breaks every hook. Install where the path is plain.

## After a Claude Code update

orchestrator relies on Claude Code through the contracts of
[docs/claude-code-dependency.md](https://github.com/hvn-p/orchestrator/blob/main/docs/claude-code-dependency.md),
whose "Last verified" line names the version they were checked on. A broken
contract costs orchestration, not the command: commands no longer in job
groups, Bash calls in `job-other-*`, no admission notice reaching Claude,
nothing learned, sessions or orphans not seen. Compare `claude --version`
with that version. From a clone of the repository,

```sh
cargo test --test claude_code -- --ignored --nocapture
```

starts a real headless session through `orchestrator launch` and reports each
contract as `ok` or `BROKEN`. It needs a signed-in `claude` and a systemd user
manager with cgroup v2, and spends about a cent of tokens.

## Turning orchestrator off

- For one session: start it with `claude` alone.
- Admission only: remove `config.json` or its `admission` section.
- Learning and events: stop `watch`.
