# Claude Code dependency

Claude Code is orchestrator's only host today. This file lists everything
orchestrator relies on in it: a behaviour not listed here is not relied on,
and code must not start relying on one without adding it. Most of these
contracts are not documented by Claude Code; they were measured, and may
change in any release.

Last verified: Claude Code 2.1.296, 2026-10-10.

## Keeping it current

- **Markers**: each place in the code that relies on a contract carries, on
  its own line right above the item, the comment `// claude-code: <id>`, with
  `<id>` a contract's heading below. They all live in the `claude-code`
  crate, which no other crate bypasses. `grep -rn 'claude-code: '` finds
  them.
- **Consistency**: `tests/claude_code_markers.rs`, part of `cargo test`,
  fails when the ids of the markers and the headings below differ, when a
  contract's Code line does not list exactly the files holding its markers,
  or when a marker lies outside `crates/claude-code/`.
- **Merge guard**: a pull request that changes a file holding a marker,
  before or after the change, must change this file too, unless it carries
  the label `claude-code-dependency-unchanged`.
  `scripts/claude-code-guard.sh <base> <head>` runs the check locally.
- **Integration test**: `cargo test --test claude_code -- --ignored` runs a
  real headless session of the installed Claude Code through
  `orchestrator launch` and checks every contract it can see from outside.
  It also starts a coordinator as `watch` would. It is manual: it needs a
  signed-in `claude`, a systemd user manager with cgroup v2, and spends a
  few tens of thousands of tokens, most read from the prompt cache. Never in
  CI.
- **A new Claude Code version**: the project skill `claude-code-compatibility` reads
  the changelog since the version above, runs the integration test, then
  updates the line above or drafts an issue.

Each contract below says what is relied on, where (Code), what breaks if it
changes, and how it is verified: by the integration test (the test function
named after the contract), by measurement, or by Claude Code's documentation
([env-vars](https://code.claude.com/docs/en/env-vars),
[tools-reference](https://code.claude.com/docs/en/tools-reference),
[hooks](https://code.claude.com/docs/en/hooks),
[cli-reference](https://code.claude.com/docs/en/cli-reference),
[permissions](https://code.claude.com/docs/en/permissions),
[permission-modes](https://code.claude.com/docs/en/permission-modes),
[cross-session-messaging](https://code.claude.com/docs/en/cross-session-messaging),
[memory](https://code.claude.com/docs/en/memory),
[headless](https://code.claude.com/docs/en/headless)).

## Contracts

### shell-prefix-variable

Claude Code reads `CLAUDE_CODE_SHELL_PREFIX` from its environment and runs
its value as one executable path, never split into words: a value holding an
argument fails with "not found". Hence `orchestrator-prefix` is a binary of
its own, not a subcommand. Claude Code spawns the prefix itself for a Bash
call or an MCP server, and through `/bin/sh -c '<prefix> <quoted command>'`
for a hook, with the path unquoted: it must hold no space nor character
special to `sh`.

- Code: `crates/claude-code/src/launch.rs` (`PREFIX_VAR`).
- If it changes: renamed or ignored, sessions still run, but every command
  stays in `main/`: nothing is measured, nothing waits.
- Verified: integration test (the probe's Bash call runs in a job group of
  the session). Measured: a value holding an argument fails (2.1.289), hooks
  run through `/bin/sh -c` (2.1.291).

### shell-prefix-argument

The prefix receives the command line as its single argument, shell-quoted,
to be run by a shell. For a Bash call it is the whole invocation Claude Code
assembled, setup included (see `bash-call-signature`). The prefix runs it
with `bash -c`, which assumes Claude Code's shell is bash, its default when
`$SHELL` is bash; `CLAUDE_CODE_SHELL` can make it zsh.

- Code: `crates/claude-code/src/invocation.rs` (`command`, `shell`).
- If it changes: with more than one argument, the prefix runs them as a
  command, unplaced and unadmitted, and logs it to `prefix.log`; with none,
  it exits successfully. Orchestration is lost, never the command. With a
  single argument no longer meant for a shell, commands break.
- Verified: integration test (the probe, a hook of several commands and an
  MCP server with arguments all run through the prefix and complete).
  Documentation: env-vars, `CLAUDE_CODE_SHELL_PREFIX`.

### shell-prefix-coverage

These go through the prefix: Bash calls, `!` commands typed in Claude Code,
shell-form command hooks, the status line, and stdio MCP server starts. These
do not: exec-form hooks (`args` set), PowerShell hooks, and helpers Claude
Code starts itself, such as clipboard tools; they stay in `main/`.

- Code: `crates/claude-code/src/invocation.rs` (`Kind`).
- If it changes: whatever leaves the prefix is counted with claude in
  `main/`; if Bash calls leave it, admission and learning stop.
- Verified: integration test (a Bash call, a hook and an MCP server each run
  in a job group). `!` commands and the status line: measured in an
  interactive session on 2.1.289. Exceptions: documentation (env-vars,
  hooks "Exec form and shell form").

### bash-call-signature

The invocation of a Bash call sources the session's shell snapshot
(`…/shell-snapshots/snapshot-<shell>-….sh`) and ends by recording the working
directory with `pwd -P >| <file>`. Hooks, the status line and MCP servers do
neither. Either sign marks a Bash call: a call may run before its snapshot
exists.

- Code: `crates/claude-code/src/invocation.rs` (`kind`).
- If it changes: Bash calls are taken for hooks: they run at once in
  `job-other-*` groups, unmeasured and never held back.
- Verified: integration test (the probe's call runs in a `job-bash-*` group
  and its record is a Bash call; the hook and the MCP server run in
  `job-other-*` groups).

### bash-call-eval

The command Claude wrote is the argument of the last top-level `eval` of the
invocation: single-quoted, or bare when it is one word, followed by
`< /dev/null` unless it reads its own input. Setup lines before it may span
several lines, and turn `extglob` off.

- Code: `crates/claude-code/src/invocation.rs` (`written`, `parser_options`).
- If it changes: no command is recognised: nothing is learned, nothing
  waits. Unit tests hold copies of a real invocation ("as Claude Code
  <version> hands it to the prefix"), to update with the contract.
- Verified: integration test (the probe's record yields `bash <probe>`).

### bash-call-cwd

Claude Code starts the prefix of a Bash call in the directory the command
runs in: the session's working directory, which follows a `cd` within the
project from one call to the next.

- Code: `crates/claude-code/src/invocation.rs` (`working_dir`).
- If it changes: peaks are learned and looked up in the wrong repository.
- Verified: integration test (the record's directory is the session's).
  Documentation: tools-reference, the Bash tool's working directory.

### bash-call-shell

Claude Code starts the prefix of a Bash call as its own child, and the call
lasts as long as that process: the prefix replaces itself with the call's
shell, keeping its pid, which names the job. A process the call leaves
running, detached with `&` or `nohup`, outlives it in the job group. So a
memory kill while the shell lives shows in the call's result, and one after
it shows nowhere: only the latter wakes the coordinator.

- Code: `crates/claude-code/src/invocation.rs` (`hold_call`).
- If it changes: with the prefix started through a shell of its own, the
  pid in the job name is not the call's shell; `oom_kill` events then say
  the call was not running when it was, and wake the coordinator for kills
  the session saw.
- Verified: integration test (the probe's parent, the call's shell, is the
  pid in the job's name, and its parent is the session's claude process; a
  process the probe kills with `SIGKILL`, as the OOM killer does, shows in
  the call's result: bash's `Killed` line and its exit status 137).

### bash-call-output

What a Bash call writes to its standard error reaches Claude with the call's
result. Admission's notices go there; the prefix's own failures must not.

- Code: `crates/claude-code/src/call.rs` (`notices`).
- If it changes: Claude no longer learns why a call waited or was refused.
- Verified: integration test (a line the probe writes to its standard error
  is in the session's output).

### bash-call-timeout

A Bash call times out after 2 minutes by default, 10 at most, tunable with
`BASH_DEFAULT_TIMEOUT_MS` and `BASH_MAX_TIMEOUT_MS`; Claude often asks for
more on a command it expects to be long. The admission wait counts toward
it, and the prefix cannot see it. A foreground call that reaches its timeout
is moved to the background, not stopped, unless it starts with `sleep`: its
result then says so and holds none of the output, only the task's id and its
output file. `max_wait_secs` stays under the default, so that a refusal
reaches Claude in the call's result.

- Code: `crates/claude-code/src/call.rs` (`DEFAULT_TIMEOUT_SECS`).
- If it changes: with a shorter default, a call refused in the foreground
  has moved to the background first, and Claude learns of the refusal only
  from the output file and the notice of its end.
- Verified: documentation (env-vars; tools-reference, "Timeout and output
  limits", "Foreground commands that move to the background"). Measured on
  2.1.296 (#16): four calls held past their timeout moved to the
  background; each time Claude read the output file, did not run the call
  again, and answered once told it had ended. The integration test cannot
  see it.

### bash-call-background

A Bash call Claude starts with `run_in_background` returns at once with its
task's id and output file. The command runs with no time limit in an
interactive session, and for 30 minutes in one that runs unattended (10 in a
`claude -p` run whose prompt is text); the session is told when it ends,
with its exit code. In auto mode, a classifier judges each call from the
user's messages, Claude's tool calls and CLAUDE.md, never from tool results:
what makes a background relaunch legitimate has to show in the command. A
refused call tells Claude to run it again in the background with
`ORCHESTRATOR_BACKGROUND=wait-for-memory`, whose value states the purpose.

- Code: `crates/claude-code/src/call.rs` (`BACKGROUND_LIMIT_SECS`,
  `in_background`).
- If it changes: a refused call can no longer wait longer without holding
  the conversation; with a shorter limit, a call waiting in the background is
  stopped before `max_background_wait_secs`; a classifier that denies the
  relaunch leaves the call refused.
- Verified: documentation (tools-reference, "Background commands" and "Time
  limit for background commands"; permission-modes, what the classifier
  sees). Measured on 2.1.296 (#16): four sessions refused in the foreground
  relaunched in the background with the variable on their own; in auto mode,
  the classifier denied 4 relaunches of 37 with
  `ORCHESTRATOR_BACKGROUND=1` and none of 25 with
  `ORCHESTRATOR_BACKGROUND=wait-for-memory`. The integration test cannot see
  it.

### session-file

Each live session has a file `<config dir>/sessions/<pid>.json` (see
`config-dir`), `<pid>` being its claude process, which every command of the
session descends from and which lives as long as the session. The file is
written when the session starts, rewritten (closed after writing, or moved
in place) when its status changes, and may outlive a crashed process. The
`<pid>.<hash>.key` files next to it hold credentials and are never opened.

- Code: `crates/claude-code/src/sessions.rs` (`read_sessions`, `CHANGES`).
- If it changes: `watch` no longer sees sessions start or end; `sessions`
  and memory events attribute nothing to a session.
- Verified: integration test (the probe finds the file of an ancestor
  process, the session's claude process). Undocumented; the supported
  interface is `claude agents --json`, which starts a process per read and
  gives no start time.

### session-file-fields

A session file is a JSON object with `pid` (a number), `sessionId` and
`name` (strings), and optionally `cwd`, `status` and `procStart`. `procStart`
is the claude process's start time in clock ticks since boot, field 22 of
`/proc/<pid>/stat`, as a decimal string: a file whose `procStart` does not
match its process belongs to a dead session whose pid was reused. Other
fields are ignored.

- Code: `crates/claude-code/src/sessions.rs` (`ClaudeSession`,
  `ClaudeSession::is_process`).
- If it changes: a missing or renamed field makes the file unreadable; a
  `procStart` with another meaning makes every session look dead.
- Verified: integration test (the file parses, its `pid`, `sessionId`, `cwd`
  and `procStart` match the session).

### session-id-variable

Every command a session starts inherits `CLAUDE_CODE_SESSION_ID`, set to the
session's `sessionId`. It keeps the value it had when the process started:
after `/clear` or a resume, the session's id changes and the process's does
not.

- Code: `crates/claude-code/src/sessions.rs` (`ID_VAR`).
- If it changes: a process reparented away from claude (`nohup`, `setsid`)
  can no longer be attributed to its session, nor reported as an orphan.
- Verified: integration test (the Bash call, the hook and the MCP server
  carry the session's id). Documentation: env-vars,
  `CLAUDE_CODE_SESSION_ID`.

### config-dir

`CLAUDE_CONFIG_DIR`, when set, replaces `~/.claude` as Claude Code's
configuration directory, sessions directory included. orchestrator reads it
from its own environment, so `watch` needs the value the sessions run with.

- Code: `crates/claude-code/src/sessions.rs` (`default_dir`).
- If it changes: `watch` and `sessions` read an empty or stale sessions
  directory.
- Verified: documentation (env-vars, `CLAUDE_CONFIG_DIR`); that the sessions
  directory follows it is inferred, not tested. The integration test checks
  the resolved directory only.

### coordinator-session

A coordinator is `claude` started with these options, in print mode for a
run `watch` starts, interactively for the setup conversation and the
interactive coordinator, and Claude Code honours them: `--name` names the
session; `--append-system-prompt-file` appends a file to the default system
prompt, in print mode and interactively; `--setting-sources project` leaves
out the user's settings, hooks, plugins and CLAUDE.md, and the project's
`settings.local.json` and `CLAUDE.local.md`, though not the organization's
instructions; `--settings` takes inline JSON (`autoMemoryEnabled`,
`permissions.blockReadsOutsideWorkingDirectories`, and `claudeMdExcludes`,
whose `/**` leaves out every `CLAUDE.md`, `.claude/rules/` file and
`AGENTS.md` that Claude Code would load from the working directory and the
directories above it, the home directory's `.claude/CLAUDE.md` included,
but not an organization's managed CLAUDE.md, and `disableAgentView`, which
turns off `/background`, `--bg` and moving the session to the background on
leaving for that session only, without stopping a running supervisor or
its background sessions, and leaves background Bash commands and messages
working);
`--strict-mcp-config` without `--mcp-config` starts no MCP server; the
CLAUDE.md of a directory given with `--add-dir` is not loaded unless
`CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD` is set, which orchestrator
removes, since it hands the user's instructions for coordinators over
itself, imports resolved; `--` ends the options before the prompt. In print mode (`-p`), `--model` picks
the model and takes a full model id (`claude-sonnet-5-5`, the default) as
well as an alias, `--max-budget-usd`, passed only when the user configures
one, stops the run once its estimated spend at list price reaches the
amount, `--no-session-persistence` keeps no transcript, and a standard input
left open is waited for.

- Code: `crates/claude-code/src/agent.rs` (`command`, `args`).
- If it changes: a coordinator starts without its role, with the user's
  settings and hooks, or without its bounds; an unknown option makes every
  run fail at once, which `runs.jsonl` and `last-run.err` show.
- Verified: integration test (the reply quotes the role's first line and the
  session's name, and not the marker of a CLAUDE.md above the coordinator's
  directory). Measured on 2.1.291: with `--setting-sources project`, a run's
  context was 4,000 tokens instead of 12,000 and held none of the user's
  CLAUDE.md; `--model claude-sonnet-5-5` ran that model. Measured on
  2.1.292: without `claudeMdExcludes`, a run loaded a `CLAUDE.md` and a
  `.claude/rules/` file from a parent directory (an `InstructionsLoaded` hook
  logged them as project files), an `AGENTS.md` from a parent directory, and,
  with its working directory under the home directory, `~/.claude/CLAUDE.md`
  as a project file; with it, none of them. The integration test's marker
  check fails without it and passes with it. `CLAUDE.local.md` and
  `.claude/settings.local.json` were not loaded either way. Measured on
  2.1.292 with `disableAgentView`, in a scratch interactive coordinator: its
  background `coordinator next` ran, and its end on a queued event woke the
  session; `SendMessage` reached a scratch `-p` peer; `/exit` offered only
  `Exit and stop tasks` and `Stay`; a supervisor started beforehand kept its
  pid and its background session throughout. `Move to background and exit`
  never showed on this machine, with or without the setting, so its removal
  rests on the documentation. Documentation: cli-reference, memory,
  agent-view ("Turn off agent view"), settings-reference.

### coordinator-permissions

`--tools` takes the names of built-in tools, `SendMessage` and `ListAgents`
among them, and leaves the others out. `--allowedTools` takes rules:
`Bash(<command>)` matches that command exactly, `Bash(<command> *)` that
command with any arguments, and `Edit(//<path>)` every file-editing tool,
Write included, on that one absolute path (a `Write(...)` rule is ignored,
with a warning). With `--permission-mode dontAsk`, any call no rule allows
is denied without asking; with `default`, it is asked.
`--add-dir` makes a directory readable; with
`blockReadsOutsideWorkingDirectories`, read-only commands such as `cat` are
denied outside the working directories.

- Code: `crates/claude-code/src/agent.rs` (`args`).
- If it changes: a coordinator cannot read the state, note in its journal
  or give waiting calls priority, or, worse, may run what it was not given.
- Verified: integration test (`orchestrator machine`, the journal note, and
  `orchestrator admission priority` with a job then without one run; a
  `touch` and a read outside, by `cat` or by Read, are denied).
  Measured on 2.1.291: in
  the setup conversation, the `Edit(//<path>)` rule on a file let the Write
  tool create it without asking; in `default` mode, an edit in an
  `--add-dir` directory asked first. Documentation: permissions,
  permission-modes.

### cross-session-message

`SendMessage` delivers a message to a session of this machine addressed by
its name, which is the `name` of its session file (see `session-file`), set
by `--name` when it is given. It needs no permission. A sender in a mode
that prompts for permissions (`default`, `dontAsk`, `auto`, `acceptEdits`)
has its messages delivered to a receiver in such a mode, and held for
approval by a receiver that skips permission prompts. A `claude -p` session
has an inbox too. The receiver reads the message between tool calls, or in a
new turn when idle. `ListAgents` lists the reachable sessions, its first line
naming the session itself. Since 2.1.293, in a session without `SendMessage`,
a notice that no session can be messaged comes before that line.

- Code: `crates/claude-code/src/agent.rs` (`args`),
  `crates/claude-code/src/messages.rs` (`address`).
- If it changes: the coordinator's messages do not arrive, arrive held, or
  go to another session of the same name.
- Verified: integration test (`ListAgents` runs and names the session; the
  test never sends). Measured on 2.1.291: a `claude -p` session in `dontAsk`
  mode messaged another one, started with `--name`, which quoted the message
  after its Bash call ended. Documentation: cross-session-messaging.

### print-json-result

`claude -p --output-format json` prints one JSON object when the run ends:
`result` holds the final reply, `num_turns` the turns, `usage` the tokens
summed over the run (`input_tokens`, `cache_creation_input_tokens`,
`cache_read_input_tokens`, `output_tokens`), `is_error` whether it failed,
`permission_denials` the calls denied, and `total_cost_usd` the estimated
spend at API list price, which a subscription does not pay.

- Code: `crates/claude-code/src/agent.rs` (`RunResult::parse`).
- If it changes: `runs.jsonl` and `orchestrator setup` lose the tokens and
  the reply of each run. A run that exits successfully still counts as
  having handled its events; one whose `is_error` reads true does not.
- Verified: integration test (the fields are present). Documentation:
  headless (`result`, `total_cost_usd`, `permission_denials`); the other
  fields measured on 2.1.291.

### background-command-wake

In an interactive session, a Bash call started with `run_in_background` runs
with no time limit, and when it ends, an idle session starts a turn with its
output.

- Code: `crates/claude-code/src/agent.rs` (`args`).
- If it changes: an interactive coordinator no longer wakes for events; they
  wait in the queue until it closes and `watch` takes them.
- Verified: measured on 2.1.291 with a background session (claude --bg),
  woken 3.5 to 6 s after its command ended; on 2.1.292 in a scratch
  interactive coordinator with `disableAgentView`. Documentation:
  tools-reference, "Time limit for background commands". The integration
  test cannot see it.
