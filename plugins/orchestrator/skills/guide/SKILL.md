---
name: guide
description: >-
  Use when the user asks about orchestrator, the tool that keeps the Claude
  Code sessions running in parallel on one Linux machine within its memory:
  installing it; starting a session through `orchestrator launch`; running
  `orchestrator watch`; writing its `config.json`; what a line of
  `events.jsonl` or `measurements.jsonl`, or an admission notice
  ("orchestrator: waiting for memory before running …"), means; what
  `orchestrator peaks` or `orchestrator sessions` shows; why a session or a
  command is not orchestrated, nothing waits or nothing is learned; whether
  orchestrator works with the installed Claude Code. Also on "how do I set up
  orchestrator", "why is this Bash call waiting for memory", "orchestrator
  learns no peak", "what does this memory_pressure event mean", "comment
  installer orchestrator", "pourquoi cette commande attend de la mémoire", "ma
  session n'est pas orchestrée", "que veut dire cet événement". Answers from
  orchestrator's reference, checked against the machine.
---

# orchestrator guide

orchestrator keeps the Claude Code sessions running in parallel on one Linux
machine within its memory. It never refuses work: it delays it. This guide
describes the `main` branch of <https://github.com/hvn-p/orchestrator>.

## What orchestrator does

- `orchestrator launch`, the shell prefix `orchestrator-prefix`,
  `orchestrator watch` (memory pressure and orphan events, the memory peak of
  every Bash call, learned per repository and command), admission (a Bash
  call learned as memory-hungry waits for memory before it runs),
  `orchestrator sessions` and `orchestrator peaks`.
- Anything this guide does not describe, orchestrator does not do. Say so
  plainly; never infer a feature from elsewhere.

## How it works

1. `orchestrator launch -- claude …` puts the session in a cgroup of its own
   (a delegated systemd user scope in `orchestrator.slice`), claude in its
   `main/` leaf, and sets `CLAUDE_CODE_SHELL_PREFIX` to `orchestrator-prefix`.
   A session started with `claude` alone is not orchestrated.
2. Claude Code runs every Bash call, `!` command, shell-form hook, status line
   refresh and stdio MCP server start through the prefix. The prefix moves the
   command into a job group of its own (`job-bash-*` for Bash calls and `!`
   commands, `job-other-*` for the rest), then runs it with `bash -c`, whatever
   shell Claude Code uses. Output, exit code and signals stay the command's.
3. `orchestrator watch` measures the memory peak of each finished Bash call,
   learns it per repository and command, and appends events for memory
   pressure and orphaned processes.
4. With a configuration, a Bash call whose commands were learned as heavy
   waits until free memory covers its expected peak plus a margin, for a
   bounded time, and says so in its output. Hooks, the status line and MCP
   servers never wait. Admission reads the peaks already learned, so it works
   while `watch` is stopped, and it counts the heavy calls of all the user's
   orchestrated sessions together.
5. The command always runs. The prefix places a Bash call in its job group,
   writes its job record, then admits it, each step needing the ones before;
   `watch` measures and learns it from the group and the record. What a
   failure loses:
   - no scope, or no job group: not measured, not learned, never admitted;
   - no job record: neither admitted nor measured, so not learned;
   - no configuration: not admitted, still measured and learned;
   - a call that cannot be parsed: neither admitted nor learned, still
     measured;
   - an admission error: the call runs at once, unreserved, still measured
     and learned.

## How to answer

- Answer from the references below and from the machine, not from memory.
  Observe before diagnosing: the cgroup of a Bash call
  (`cat /proc/self/cgroup` run as a Bash call), the files, `--help`.
- When `orchestrator --help` lists a subcommand this guide does not know, or
  lacks one it describes, the installed build differs from this guide: trust
  the binary's `--help` and say so. `claude plugin update
  orchestrator@orchestrator` fetches the latest guide (applied after a
  restart of Claude Code).
- Command lines can hold credentials. Events and `peaks` show only command
  heads or text Claude wrote; `orchestrator sessions` shows the start of real
  command lines, and `/proc/<pid>/cmdline` or `environ` all of them. Quote
  those only as far as the question needs.
- `config.json` changes what every orchestrated session's next Bash call does:
  show the values and ask before writing it. Same for Claude Code settings.

## References

Read the one the question needs:

- [reference/commands.md](reference/commands.md): installing, then each
  command (`launch`, the prefix, `watch`, `sessions`, `peaks`): options,
  behaviour, output.
- [reference/configuration.md](reference/configuration.md): `config.json` and
  its fields, choosing values, the environment variables that matter.
- [reference/events.md](reference/events.md): `events.jsonl`,
  `measurements.jsonl`, admission notices, and the messages each command and
  the prefix print.
- [reference/files.md](reference/files.md): every file and directory
  orchestrator reads or writes, who writes it, how long it lives.
- [reference/troubleshooting.md](reference/troubleshooting.md): from a symptom
  to its checks and causes.

Claude Code compatibility: orchestrator relies on Claude Code only through the
contracts listed in
[docs/claude-code-dependency.md](https://github.com/hvn-p/orchestrator/blob/main/docs/claude-code-dependency.md)
(under `docs/` in a clone of the repository), whose "Last verified" line names
the Claude Code version they were checked on. Read the version there, never
from memory.
