# Files and locations

`<runtime>` is `$XDG_RUNTIME_DIR/orchestrator` (else
`/run/user/<uid>/orchestrator`): in memory, cleared at reboot. `<state>` is
`$XDG_STATE_HOME/orchestrator` (else `~/.local/state/orchestrator`): on disk,
kept across reboots. `<config>` is `$XDG_CONFIG_HOME/orchestrator` (else
`~/.config/orchestrator`). orchestrator writes nothing into a repository it
works in.

## orchestrator's own

| Path | Written by | Read by | Holds, and how long |
| :- | :- | :- | :- |
| `<config>/config.json` | a human | the prefix, at each Bash call | The admission thresholds. See configuration.md. |
| `<state>/peaks/<hash of repository>/<0-f>.jsonl` | `watch` | the prefix, `peaks` | Learned peaks: a first line naming the repository, then one JSON line per command (`id`, `label`, `peak_mb`, `last_seen`, `recent` as `[peak_mb, alone]` pairs, oldest first). Up to 2,000 most recently seen commands per repository, in sixteen files chosen by the command's hash. Replaced whole on each write. Kept until removed. |
| `<runtime>/events.jsonl` | `watch` | nothing yet | Events. See events.md. Grows until reboot or truncation. |
| `<runtime>/measurements.jsonl` | `watch` | nothing | One line per measured Bash call, its full command included. Grows until reboot. |
| `<runtime>/jobs/<session scope>/<job>.json` | the prefix | `watch` | A Bash call's invocation and working directory, until `watch` has measured the job. Records of sessions gone are dropped by the next sweep. |
| `<runtime>/reservations/<job>.json` | the prefix | the prefix | A running heavy call's job group and expected peak. Removed by the next admission check once the group is empty or gone. |
| `<runtime>/admission.lock` | the prefix | the prefix | The lock counting and reserving happen under. |
| `<runtime>/prefix.log` | the prefix | a human | One line per failure the prefix let pass. See events.md. Grows until reboot. |

Forgetting learned peaks: remove `<state>/peaks/` or one repository's
directory under it. Admission then knows nothing of those commands, and
`watch` learns them again.

## The cgroup tree

```
/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service/orchestrator.slice/
└─ orchestrator-<pid>-<ms>.scope   one session, started by `orchestrator launch`
   ├─ main/                        claude itself, and what bypasses the prefix
   ├─ job-bash-<pid>-<ms>/         one Bash call or `!` command, with all it starts
   └─ job-other-<pid>-<ms>/        one hook, status line refresh or MCP server
```

`<pid>` in a scope's name is the launched claude's pid, in a job's name the
prefix's. Each job group holds the kernel's accounting of the job, such as
`memory.current`, `memory.peak` and `cgroup.events`. `watch` removes a job's
group once it has measured it; systemd removes the scope, with every group in
it, when the session ends.

## Claude Code's, read by orchestrator

| Path | Read by | Use |
| :- | :- | :- |
| `<Claude Code config dir>/sessions/<pid>.json` (`$CLAUDE_CONFIG_DIR`, else `~/.claude`) | `watch`, `sessions` | Which sessions are live, their name, id and claude process. The `.key` files next to them hold credentials and are never opened. |
| `/proc/<pid>/stat`, `status`, `cmdline`, and the `CLAUDE_CODE_SESSION_ID` of `environ` | `watch`, `sessions` | Attributing processes to sessions and finding orphans. Only command heads reach events. |
| `/proc/meminfo` | the prefix, `watch`, `sessions` | `MemAvailable`. |
| `/proc/pressure/memory` | `watch` | The memory pressure trigger. |
