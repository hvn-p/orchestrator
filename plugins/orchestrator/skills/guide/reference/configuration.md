# Configuration

## config.json

`$XDG_CONFIG_HOME/orchestrator/config.json`, by default
`~/.config/orchestrator/config.json` (a relative `XDG_CONFIG_HOME` is
ignored), resolved from the environment Claude Code was started with. Without
it, nothing waits. Only the prefix reads it, at every Bash call of an
orchestrated session: a change applies from the next Bash call, with nothing
to restart. It is written by hand.

For example, for a machine with about 30 GB of RAM:

```json
{
  "admission": {
    "heavy_mb": 1024,
    "margin_mb": 2048,
    "max_wait_secs": 60
  }
}
```

| Field | Meaning |
| :- | :- |
| `admission.heavy_mb` | A Bash call whose expected peak reaches this, in MB, waits for memory. At 0, every call with a learned command is heavy. |
| `admission.margin_mb` | Free memory kept on top of the expected peak, in MB. At 0, a heavy call starts as soon as free memory covers its expected peak. |
| `admission.max_wait_secs` | The longest a call waits; then it runs anyway. At 0, no call waits and no notice is written, but heavy calls still reserve their expected peak. |

- `admission` is the only section; its three fields are all required.
- A file that cannot be read whole (invalid JSON, a missing or unknown field,
  a misspelt section) turns admission off, and each Bash call logs the error
  to `prefix.log` in the runtime directory. A typo never makes calls wait.
- Removing the file, or its `admission` section, turns admission off.
- MB here, as everywhere in orchestrator, means 1024 × 1024 bytes.

## Choosing values

Facts to weigh:

- **The wait counts toward the Bash call's timeout**: 2 min by default
  (`BASH_DEFAULT_TIMEOUT_MS`); Claude can ask for more, up to the larger of
  `BASH_MAX_TIMEOUT_MS` (10 min by default) and the default. The prefix cannot
  see it, so `max_wait_secs` has to leave the command its own time. A
  foreground call that reaches its timeout is moved to the background by
  Claude Code, not stopped, unless it starts with `sleep` or background tasks
  are turned off (`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`).
- **What the margin absorbs**: a heavy call is admitted on the kernel's
  `MemAvailable` at that moment, net of the reservations of heavy calls
  already running. Memory that anything else takes afterwards (other
  sessions, light calls, other programs) is covered only by `margin_mb`.
- **A need the machine can never meet**: a call needs its expected peak plus
  `margin_mb` free. When that exceeds what the machine can free, the call
  waits `max_wait_secs` every time, then runs with a "memory is still short"
  notice.
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
  `cd -`), or into a directory that does not exist, the commands are not
  learned and never make a call wait. A call that cannot be parsed teaches
  nothing and never waits.

### Learning

- `watch` learns per repository (the git common directory of the directory
  the command runs in, all worktrees together; outside a repository, the
  directory) and per command.
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
  both. Waiting calls have no order: the first to check once memory frees up
  runs.
- A waiting call checks again as soon as a job holding a reservation changes,
  and every second otherwise. After `max_wait_secs`, it runs anyway, with its
  reservation.
- A heavy call that leaves a process running, such as a server started in the
  background, keeps its reservation, net of what its group uses, until that
  process ends.
- An admission lock held for more than a second, or any other error, lets the
  call run at once; the error goes to `prefix.log`.

## Environment variables

| Variable | Read by | Effect |
| :- | :- | :- |
| `XDG_CONFIG_HOME` | the prefix | Where `config.json` is (absolute paths only). |
| `XDG_STATE_HOME` | the prefix, `watch`, `peaks` | Where learned peaks are (absolute paths only). |
| `XDG_RUNTIME_DIR` | the prefix, `watch` | The runtime directory, else `/run/user/<uid>`. |
| `HOME` | all | The fallback for the above; resolves `cd` and `cd ~` when recognising commands. |
| `CLAUDE_CONFIG_DIR` | `watch`, `sessions` | Claude Code's configuration directory, holding `sessions/`. |
| `CLAUDE_CODE_SHELL_PREFIX` | Claude Code | Set by `launch` to the prefix's path. |
| `CLAUDE_CODE_SHELL` | Claude Code | The shell Claude Code builds its commands for (bash or zsh). The prefix runs them with bash whatever it is. |
| `CLAUDE_CODE_SESSION_ID` | `watch`, `sessions` | Set by Claude Code in every command it starts; attributes a process reparented away from claude, and marks orphans. |
| `BASH_DEFAULT_TIMEOUT_MS`, `BASH_MAX_TIMEOUT_MS` | Claude Code | The Bash timeout the admission wait counts toward. |
| `CLAUDE_CODE_TOOL_MEMORY_LIMIT` | Claude Code | On Linux, a size such as `4G` caps the memory of all of a session's Bash, PowerShell and Monitor commands together, through a memory cgroup of Claude Code's own: past it, the kernel kills a command, and nothing in its result names the cap. `CLAUDE_CODE_TOOL_MEMORY_CGROUP_EXCLUDE` lists the other kinds of processes Claude Code exempts from that cap. orchestrator never kills a command. |

The prefix reads these from the environment Claude Code was started with.
