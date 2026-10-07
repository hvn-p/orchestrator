# Events, measurements, notices and messages

Sizes are in MB (1024 × 1024 bytes); times `at` are seconds since the Unix
epoch. The example lines below show the exact shape; their values are made
up.

## events.jsonl

`$XDG_RUNTIME_DIR/orchestrator/events.jsonl` (or `watch --runtime-dir`), one
JSON object per line, appended by `watch`. The file is opened per event, so a
reader may truncate it between two. Nothing in orchestrator reads it or acts
on an event: events are for a human, or a tool of the user's own that reads
the file.

A process is never named by its full command line, which can hold
credentials: an event gives its pid, its `comm` (the kernel's name for the
process, at most 15 characters) and the head of its command line. The head is
the program and at most two words after it, each reduced to its last path
component. It stops at the first option, and at a word that is a URL, holds a
character other than letters, digits and `._@+-` (such as `=`, `:` or
quotes), or is longer than 32 characters. It is empty when the program name
itself fails that test.

### memory_pressure

```json
{"at":1791356865,"kind":"memory_pressure","available_mb":812,"stall_ms":200,"largest":{"session":"api refactor","session_id":"6f1c2d9e-3b4a-4c5d-8e7f-0a1b2c3d4e5f","rss_mb":6120,"process_pid":41237,"process_rss_mb":3890,"process_comm":"node","process_command":"node tsc"},"next":[{"session":"docs","rss_mb":1450}]}
```

Written when some task stalled on memory for at least `--stall-ms` within a
2 s window (a PSI trigger), at most once per `--cooldown-secs`.

| Field | Meaning |
| :- | :- |
| `available_mb` | The kernel's `MemAvailable` when the event was written. |
| `stall_ms` | The threshold that fired, `--stall-ms`; not a measured duration. |
| `largest` | The live Claude Code session using the most resident memory, or `null` when none is live. |
| `largest.session`, `largest.session_id` | Its name and id, as Claude Code records them. |
| `largest.rss_mb` | The resident memory of all its processes. |
| `largest.process_pid`, `process_rss_mb`, `process_comm`, `process_command` | Its largest process: pid, resident memory, `comm`, command head. |
| `next` | Up to two next sessions by memory: `session`, `rss_mb`. |

It reports; it acts on nothing.

### orphans

```json
{"at":1791356870,"kind":"orphans","orphans":[{"pid":52011,"session_id":"0a9b8c7d-6e5f-4a3b-9c2d-1e0f2a3b4c5d","rss_mb":640,"processes":3,"comm":"npm","command":"npm run dev"}]}
```

Processes left behind by a Claude Code session that is no longer live: they
carry its `CLAUDE_CODE_SESSION_ID` and no live session owns them. Each group
is folded into its topmost orphaned process.

| Field | Meaning |
| :- | :- |
| `pid`, `comm`, `command` | The group's topmost process: pid, `comm`, command head. |
| `session_id` | The session it came from, from its `CLAUDE_CODE_SESSION_ID`. |
| `rss_mb`, `processes` | The group's resident memory and number of processes. |

Written when `watch` starts, five seconds after a session's claude process
exits, and by the scan every `--orphan-interval-secs`, for each group this run
of `watch` has not reported yet. A restarted `watch` reports again the groups
still alive. orchestrator does not stop them. A process detached with `nohup`
or `setsid` from a session that has since run `/clear` or been resumed keeps
the old id: it is reported as an orphan though its session lives.

## measurements.jsonl

```json
{"at":1791356901,"session":"orchestrator-41000-1791356000000.scope","job":"job-bash-41390-1791356880123","peak_mb":2210,"command":"source /home/u/.claude/shell-snapshots/snapshot-bash-….sh 2>/dev/null || true && … && eval 'pnpm typecheck' < /dev/null && pwd -P >| /tmp/claude-…-cwd","cwd":"/home/u/project"}
```

One line per Bash call `watch` measured, next to `events.jsonl`. `session` is
the session's scope, `job` the call's job group, `peak_mb` the group's
`memory.peak` (children included), `command` the whole invocation Claude Code
handed the prefix, and `cwd` the directory it ran in. Nothing reads this file;
admission reads the learned peaks. It holds the full text of the commands
Claude wrote, and lives until reboot.

## Admission notices

A Bash call held back writes two lines to its standard error, which Claude
Code returns with the call's output. A call that starts at once writes
nothing.

```
orchestrator: waiting for memory before running `pnpm typecheck`. This call is expected to peak at 2210 MB; with the 2048 MB margin it needs 4258 MB free, and 3100 MB are free, net of 1800 MB kept for 1 heavy command already running. It starts as soon as memory frees up, after 60 s at most.
orchestrator: running `pnpm typecheck` after waiting 12.4 s for memory.
```

- The command is named by its label (see `orchestrator peaks`): the call's
  first heavy command that has run alone, else its first heavy command, or
  `this command` when it has no label.
- "net of … already running" appears only when running heavy calls hold
  reservations.
- When the call waited `max_wait_secs` and memory is still short, the second
  line reads instead:

  ```
  orchestrator: running `pnpm typecheck` after waiting 60.0 s, the longest admission waits; memory is still short: 3900 MB free, 4258 MB needed.
  ```

The call was delayed, not refused: its own output and exit code follow,
unchanged. There is nothing to retry.

## Messages

### launch, on its standard error

- `orchestrator: replacing CLAUDE_CODE_SHELL_PREFIX=<value>`: a shell prefix
  was already set; this session uses orchestrator's instead.
- `orchestrator: <reason>; the session runs unorchestrated`: the session
  started, outside orchestration. The reasons:
  - `<dir>/orchestrator-prefix not found`, or `locating orchestrator: …`;
  - `asking the systemd user manager for orchestrator-….scope: ` followed by
    `running busctl: …`, `busctl got no answer within 2s`,
    `busctl failed (exit status: 1): <busctl's reason>`, or
    `unexpected reply from busctl: "<reply>"`;
  - `waiting for <job> to move this process into <unit>: ` followed by
    `still elsewhere after 2s` or `reading own cgroup: …`;
  - `setting up <scope>: …`.
- `Error: running <command>`, then `Caused by:` and the reason: the command
  itself could not be started. `launch` exits with status 1.

### The prefix, in prefix.log

`$XDG_RUNTIME_DIR/orchestrator/prefix.log`, one line per failure:
`<ms since the epoch> <error>`. Lines are written only once the runtime
directory exists. The command ran anyway; the line says which step it lost:

- `reading …/config.json: …`, `neither XDG_CONFIG_HOME nor HOME is set`, or
  `neither XDG_STATE_HOME nor HOME is set`: no admission.
- `reading own cgroup: …`, or `creating job-bash-…: …` (or `job-other-…`):
  the command ran where Claude Code started it, in `main/`, neither measured
  nor admitted.
- `creating …/jobs/…: …` or `writing …/jobs/…/job-bash-….json: …`: the job
  record could not be written; the call ran in its job group, neither
  admitted nor learned.
- `…/admission.lock stayed locked for 1s`, or another admission error
  (`opening …`, `locking …`, `creating …/reservations`, `reading …`,
  `writing …/reservations/…`, `no MemAvailable line in …`,
  `reading the working directory`): the call ran at once, unreserved.
- `expected one argument, got <n>: running them unorchestrated`: Claude Code
  passed other than one argument; the command ran unplaced and unadmitted.

On its standard error, the prefix writes only admission notices and, when it
cannot start bash (or, in the last case above, the command),
`orchestrator-prefix: running bash: <error>` (or `running the command`), with
exit status 127.

### watch, on its standard error

- `orchestrator: no systemd user manager above this process; jobs are not collected`:
  nothing is measured nor learned; events still are. See commands.md,
  "Starting watch".
- `orchestrator: no kernel signal for memory pressure, sweeps only: <error>`:
  no `memory_pressure` event at all (there is no sweep for pressure). The
  kernel must expose `/proc/pressure/memory` and, before Linux 6.4, refuses
  the trigger to an unprivileged process.
- `orchestrator: no kernel signal for session ends, sweeps only: <error>`:
  orphans are found only by the periodic scan. Most often the sessions
  directory did not exist when `watch` started; a restart of `watch` once a
  session has run sets the signal up.
- `orchestrator: no kernel signal for job ends, sweeps only: <error>`, or
  `orchestrator: job tracking stopped, sweeping instead: <error>`: jobs are
  swept every 2 seconds instead.
- `orchestrator: the memory pressure trigger broke; no more pressure events`:
  a restart of `watch` sets it up again.
- `orchestrator: skipping <file>: <error>`: a Claude Code session file could
  not be parsed, maybe while being rewritten; the next read tries again.
  `sessions` prints it too.
- `orchestrator: learning a peak: <error>`, `orchestrator: waiting: <error>`,
  `orchestrator: <error>`: an error `watch` outlives; it keeps running.

`watch` stops at start, printing `Error: …` (with its cause under
`Caused by:` when there is one) and exiting with status 1, when its runtime directory cannot be created,
`<proc root>/meminfo` cannot be read, or a default directory cannot be
resolved (`neither CLAUDE_CONFIG_DIR nor HOME is set`,
`neither XDG_STATE_HOME nor HOME is set`).
