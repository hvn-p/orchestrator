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
| `<config>/config.json` | `orchestrator config admission` and `config coordinator` (which the setup conversation uses), or a human | the prefix at each Bash call, `watch` at each event, `setup`, `coordinator`, `admission`, `config`, coordinators | The admission thresholds and the coordinator section. See configuration.md. |
| `<config>/CLAUDE.md` | a human, or a coordinator the user asked | every coordinator, as it starts | The coordinators' instructions, with the files it imports. See coordinator.md. |
| `<state>/peaks/<hash of repository>/<0-f>.jsonl` | `watch` | the prefix, `peaks` | Learned peaks: a first line naming the repository, then one JSON line per command (`id`, `label`, `peak_mb`, `last_seen`, `recent` as `[peak_mb, alone]` pairs, oldest first). Sixteen files per repository, chosen by the command's hash, each keeping its 125 most recently seen commands: up to 2,000 per repository. Replaced whole on each write. Kept until removed. |
| `<runtime>/events.jsonl` | `watch` | nothing in orchestrator | Events. See events.md. Grows until truncated, or until `<runtime>` is removed. |
| `<runtime>/measurements.jsonl` | `watch` | nothing | One line per measured Bash call, its full command included. Grows until `<runtime>` is removed. |
| `<runtime>/jobs/<session scope>/<job>.json` | the prefix | `watch` | A Bash call's invocation and working directory, until `watch` has measured the job. Records of sessions gone are dropped by the next sweep. |
| `<runtime>/reservations/<job>.json` | the prefix | the prefix | A running heavy call's job group and expected peak. Removed by the next admission check once the group is empty or gone. |
| `<runtime>/admission.lock` | the prefix | the prefix | The lock counting and reserving happen under. |
| `<runtime>/waiting/<job>.json` | the prefix | `watch`, `admission`, coordinators | A heavy call waiting for memory: its job group, label, expected peak, the memory it needs, and since when. Removed when it runs. One left by a killed prefix is skipped by readers, and removed by `watch` while the coordinator wakes. |
| `<state>/coordinator/` | orchestrator | coordinators | The coordinators' working directory. Claude Code's project settings in its `.claude/settings.json` apply to every coordinator; `.claude/settings.local.json` does not. |
| `<state>/coordinator/journal.md` | `orchestrator coordinator note` | every coordinator | The coordinators' journal, one dated line per line of a note's text, the latest 200 kept. Kept across reboots. |
| `<runtime>/coordinator/holder.json` | `watch`, `setup`, `coordinator` | the same | The process running a coordinator, pid and start time. Stale once that process has ended. |
| `<runtime>/coordinator/queue.json` | `watch`, `setup`, `coordinator`, `coordinator next` | the same | The queued events and their status; the 20 latest done kept. See coordinator.md. |
| `<runtime>/coordinator/lock` | the same | the same | The lock the holder and the queue change under. |
| `<runtime>/coordinator/role.md` | `watch`, `setup`, `coordinator` | the coordinator starting | Its role, plus the setup instructions for setup, plus the coordinators' instructions with their imports. Rewritten at each start. |
| `<runtime>/coordinator/runs.jsonl` | `watch` | a human | One line per coordinator run. See coordinator.md. Grows until `<runtime>` is removed. |
| `<runtime>/coordinator/last-run.json`, `last-run.err` | `watch` | a human | What the latest run printed on its standard output and error. |
| `<runtime>/api.sock` | `watch` | the read commands, `config admission`, `setup`, `coordinator`, any client | The local API's socket, only its owner can connect (api.md). Replaced by the next `watch` when the one that made it has ended. |
| `<runtime>/prefix.log` | the prefix | a human | One line per failure the prefix let pass, once the runtime directory exists (`watch` creates it, as does an orchestrated Bash call writing its job record). See events.md. Grows until `<runtime>` is removed. |

`watch --runtime-dir` and `--state-dir` move the `<runtime>/coordinator/`
and `<state>/coordinator/` files of the runs it starts, the journal its runs
are briefed with included. `setup`, `coordinator` and `coordinator note`, a
run's own notes included, always use the default directories.

Forgetting learned peaks: remove `<state>/peaks/` or one repository's
directory under it. Admission then knows nothing of those commands, and
`watch` learns them again.

Forgetting what coordinators remember: remove
`<state>/coordinator/journal.md`.

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
| `<Claude Code config dir>/sessions/<pid>.json` (`$CLAUDE_CONFIG_DIR`, else `~/.claude`) | `watch`, `sessions`, `admission` | Which sessions are live, their name, id and claude process. The `.key` files next to them hold credentials and are never opened. |
| `/proc/<pid>/stat`, `status`, `cmdline`, and the `CLAUDE_CODE_SESSION_ID` of `environ` | `watch`, `sessions` | Attributing processes to sessions and finding orphans. Only command heads reach events. |
| `/proc/meminfo` | the prefix, `watch`, `sessions` | `MemAvailable`. |
| `/proc/pressure/memory` | `watch` | The memory pressure trigger. |
