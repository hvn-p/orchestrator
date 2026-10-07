# Files and locations

`<runtime>` is `$XDG_RUNTIME_DIR/orchestrator` (else
`/run/user/<uid>/orchestrator`): in memory, in the user's runtime directory,
which is cleared at reboot and removed at the user's final logout unless
lingering is enabled (`loginctl enable-linger`; see pam_systemd(8)).
`<state>` is `$XDG_STATE_HOME/orchestrator` (else
`~/.local/state/orchestrator`): on disk, kept across reboots. `<config>` is
`$XDG_CONFIG_HOME/orchestrator` (else `~/.config/orchestrator`). orchestrator
writes nothing into a repository it works in.

## orchestrator's own

| Path | Written by | Read by | Holds, and how long |
| :- | :- | :- | :- |
| `<config>/config.json` | a human | the prefix, at each Bash call | The admission thresholds. See configuration.md. |
| `<state>/peaks/<hash of repository>/<0-f>.jsonl` | `watch` | the prefix, `peaks` | Learned peaks: a first line naming the repository, then one JSON line per command (`id`, `label`, `peak_mb`, `last_seen`, `recent` as `[peak_mb, alone]` pairs, oldest first). Sixteen files per repository, chosen by the command's hash, each keeping its 125 most recently seen commands: up to 2,000 per repository. Replaced whole on each write. Kept until removed. |
| `<runtime>/events.jsonl` | `watch` | nothing in orchestrator | Events. See events.md. Grows until truncated, or until `<runtime>` is removed. |
| `<runtime>/measurements.jsonl` | `watch` | nothing | One line per measured Bash call, its full command included. Grows until `<runtime>` is removed. |
| `<runtime>/jobs/<session scope>/<job>.json` | the prefix | `watch` | A Bash call's invocation and working directory, until `watch` has measured the job. Records of sessions gone are dropped by the next sweep. |
| `<runtime>/reservations/<job>.json` | the prefix | the prefix | A running heavy call's job group and expected peak. Removed by the next admission check once the group is empty or gone. |
| `<runtime>/admission.lock` | the prefix | the prefix | The lock counting and reserving happen under. |
| `<runtime>/prefix.log` | the prefix | a human | One line per failure the prefix let pass, once the runtime directory exists (`watch` creates it, as does an orchestrated Bash call writing its job record). See events.md. Grows until `<runtime>` is removed. |

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
group once the job has ended, after measuring a Bash call; systemd removes the
scope, with every group in it, when the session ends.

## Claude Code's, read by orchestrator

| Path | Read by | Use |
| :- | :- | :- |
| `<Claude Code config dir>/sessions/<pid>.json` (`$CLAUDE_CONFIG_DIR`, else `~/.claude`) | `watch`, `sessions` | Which sessions are live, their name, id and claude process. The `.key` files next to them hold credentials and are never opened. |
| `/proc/<pid>/stat`, `status`, `cmdline`, and the `CLAUDE_CODE_SESSION_ID` of `environ` | `watch`, `sessions` | Attributing processes to sessions and finding orphans. Only command heads reach events. |
| `/proc/meminfo` | the prefix, `watch`, `sessions` | `MemAvailable`. |
| `/proc/pressure/memory` | `watch` | The memory pressure trigger. |
