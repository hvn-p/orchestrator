# Configuration

## config.json

`$XDG_CONFIG_HOME/orchestrator/config.json`, by default
`~/.config/orchestrator/config.json` (a relative `XDG_CONFIG_HOME` is
ignored). Without it, nothing waits. Only the prefix reads it, at every Bash
call of an orchestrated session: a change applies from the next Bash call,
with nothing to restart. It is written by hand.

The README's example, for a machine with about 30 GB of RAM:

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
| `admission.heavy_mb` | A Bash call whose expected peak reaches this, in MB, waits for memory. |
| `admission.margin_mb` | Free memory kept on top of the expected peak, in MB. |
| `admission.max_wait_secs` | The longest a call waits; then it runs anyway. |

- `admission` is the only section; its three fields are all required.
- A file that cannot be read whole (invalid JSON, a missing or unknown field,
  a misspelt section) turns admission off, and each Bash call logs the error
  to `prefix.log` in the runtime directory. A typo never makes calls wait.
- Removing the file, or its `admission` section, turns admission off.
- MB here, as everywhere in orchestrator, means 1024 × 1024 bytes.

## Choosing values

Facts to weigh:

- **The wait counts toward the Bash call's timeout**: 2 min by default,
  10 min at most, set by Claude Code's `BASH_DEFAULT_TIMEOUT_MS` and
  `BASH_MAX_TIMEOUT_MS`. The prefix cannot see it, so `max_wait_secs` has to
  leave the command its own time. A foreground call that reaches its timeout
  is moved to the background by Claude Code, not stopped.
- **Available memory drifts**: `MemAvailable` moved by up to 350 MB within a
  few seconds while other sessions worked, on the reference machine. The
  margin has to exceed that drift.
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
Claude Code hands the prefix. It is parsed as shell (brush-parser), the way
Claude Code matches its Bash permission rules:

- The call is split into simple commands at `&&`, `||`, `;`, `|`, `|&`, `&`
  and newlines.
- Removed: the wrappers `timeout`, `time`, `nice`, `nohup`, `stdbuf`,
  `command` and `builtin` with their options, leading environment
  assignments, redirections, and `cd`, whose target still decides the
  directory of the commands after it. So
  `cd app && timeout 300 pnpm exec vitest run X 2>&1 | grep FAIL` yields
  `pnpm exec vitest run X` and `grep FAIL`.
- The bodies of brace groups, subshells, loops and conditionals are commands
  of the call; a `cd` inside a subshell stays there. A command substitution
  stays part of its word. A here-document or here-string stays with its
  command, as its input.
- What remains is kept word for word: `vitest run` and
  `vitest run one.test.ts` are two commands.
- After a `cd` whose target only running the call would tell (`cd "$dir"`,
  `cd -`), or into a directory that does not exist, the commands are not
  learned. A call that cannot be parsed teaches nothing and never waits.

### Learning

- `watch` learns per repository (its git common directory, all worktrees
  together; outside a repository, the directory) and per command.
- Each command of a measured call gets the call's peak, marked "alone" when
  it was the call's only command. The latest five calls are kept.
- A command's expected peak is the smallest peak among its latest calls,
  walking back from the latest and stopping at the latest call it ran alone
  in. A filter such as `tail -1` seen only beside a heavy command carries that
  command's peak; once seen in a light call, it is light.
- One measured call is enough: from then on the command has an expected peak.
- A repository keeps its 2,000 most recently seen commands.

### Admitting

- A call's expected peak is the largest of its commands', never their sum. A
  call with no learned command, or under `heavy_mb`, starts at once and
  reserves nothing.
- A heavy call waits until free memory covers its expected peak plus
  `margin_mb`. Free memory is the kernel's `MemAvailable` minus the
  reservations of heavy calls already running: from its admission until its
  job group empties, a heavy call reserves its expected peak minus what its
  group already uses.
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
| `CLAUDE_CODE_SESSION_ID` | `watch`, `sessions` | Set by Claude Code in every command it starts; attributes a process reparented away from claude, and marks orphans. |
| `BASH_DEFAULT_TIMEOUT_MS`, `BASH_MAX_TIMEOUT_MS` | Claude Code | The Bash timeout the admission wait counts toward. |
| `CLAUDE_CODE_TOOL_MEMORY_LIMIT` | Claude Code | Claude Code's own memory cap, a cgroup of its own that kills commands past a size. orchestrator's design wants it off: it refuses work and competes with the job groups. |
