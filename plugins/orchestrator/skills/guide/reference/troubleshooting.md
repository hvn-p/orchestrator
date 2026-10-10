# Troubleshooting

Observe first. Most questions are settled by where a command runs, by
orchestrator's files (files.md) and by the messages it printed (events.md).

## A command says watch is not running

`Error: orchestrator watch is not running: start it with `orchestrator watch`
(no answer at <socket>)`: the read commands, `config admission`, `setup` and
`coordinator` ask `watch`, and none answers at the socket they name.

- No `watch` runs: start one (commands.md, "Starting watch").
- A `watch` runs with another runtime directory: started with
  `--runtime-dir`, or with another `XDG_RUNTIME_DIR` than the shell asking,
  as a user service with the manager's environment can be. `ls
  <runtime>/api.sock` in each tells; the commands ask the one in their
  default runtime directory.
- The `watch` that served it has ended, leaving its socket: the next `watch`
  replaces it.

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
- After `orchestrator: setting up <scope>: …; the session runs
  unorchestrated`, the session stays in the scope itself or in its `main/`
  leaf, without the prefix: its calls run there too, unorchestrated.
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
  Claude Code's background service starts them, not `orchestrator launch`.
  Whether their commands are orchestrated depends on where that service
  itself was started. A session started with `orchestrator launch -- claude`
  is orchestrated.
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
- `watch` runs with another runtime directory than the prefix's default
  (`--runtime-dir`, or another `XDG_RUNTIME_DIR`): it never finds the job
  records.
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
  (`cd "$dir"`, `cd -`), or its directory did not exist when the call ended
  (the `cd` into it failed).
- `watch` runs with another state directory than the prefix's default
  (`--state-dir`, or another `XDG_STATE_HOME`): it learns there, where
  admission never reads. `orchestrator peaks` shows what that `watch`
  learned.
- It was learned under another repository: peaks are per repository (its git
  common directory) and per exact command (configuration.md, "Recognising a
  command").

## Nothing waits

- No `config.json`, or no `admission` section: admission is off, and nothing
  is logged. The prefix looks for the file from the environment Claude Code
  was started with (`XDG_CONFIG_HOME`, else `HOME`), which may differ from a
  terminal's.
- A `config.json` that cannot be read: `prefix.log` holds
  `reading …/config.json: …` for each Bash call. An `admission` section
  written before `max_background_wait_secs` existed lacks that field: set the
  section again with `orchestrator config admission` (commands.md).
- No command of the call has an expected peak at or above `heavy_mb`
  (`orchestrator peaks`). The expected peak is the smallest of the latest
  calls back to the latest one the command ran alone in (configuration.md,
  "Learning"): a heavy call shared with other commands does not raise it
  above its latest run alone, nor above a lighter call since.
- The command follows a `cd` whose target only running tells (`cd "$dir"`,
  `cd -`): it is never looked up.
- The call cannot be parsed as shell: none of its commands is looked up.
- `max_wait_secs` is 0: a heavy call that does not fit is refused at once,
  without waiting.
- Memory was free: a heavy call that finds enough starts at once, silently.
- The command is not a Bash call of an orchestrated session.
- An error let it through: see `prefix.log`.

## A call waits too often or too long, or is refused

- The second notice says `refused`: its expected peak plus `margin_mb`
  exceeded what the machine freed within its longest wait. Run in the
  background with `ORCHESTRATOR_BACKGROUND=wait-for-memory` before the
  command, it waits `max_background_wait_secs` without holding the
  conversation.
- In auto mode, Claude Code's classifier may deny that background run; Claude
  then stops and says so. Telling Claude that the run is wanted can let it
  through.
- The waiting notice says "net of … already running": other heavy calls hold
  reservations until their job groups empty, including a server one of them
  left running in the background.
- A light command seen only beside heavy ones carries their peak: a filter
  such as `tail -1`, used only after a build, is as heavy as the build. Its
  first lighter call, once measured, lowers it (configuration.md,
  "Learning"). Removing its repository's directory under `<state>/peaks/`
  (files.md) forgets every command of that repository at once.
- The call reached its Bash timeout before admission decided: Claude asked
  for a timeout under `max_wait_secs`, or `BASH_DEFAULT_TIMEOUT_MS` is lower.
  Claude Code moved it to the background, where it goes on waiting; its
  notices are in the output file, not in the call's result
  (configuration.md, "Choosing values").

## A command behaves as in bash, not zsh

The prefix runs every command with `bash -c`, even when Claude Code uses zsh
(through `CLAUDE_CODE_SHELL`, or a zsh `$SHELL`). zsh syntax then fails in an
orchestrated session. Claude Code uses bash when `CLAUDE_CODE_SHELL` names a
working bash binary (commands.md, "orchestrator-prefix").

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

- `watch` reads another sessions directory than the sessions write, and
  `sessions` shows what `watch` reads: `CLAUDE_CONFIG_DIR` differs, or pass
  `watch --sessions-dir`. A `watch`
  started as a service with `systemd-run --user` does not get the shell's
  `CLAUDE_CONFIG_DIR` (commands.md, "Starting watch"). With a wrong or
  missing sessions directory, the processes of live sessions also show up as
  orphans, in `sessions` and in `orphans` events.
- The sessions directory did not exist when `watch` started
  (`no kernel signal for session ends`): orphans then come only from the
  periodic scan, until `watch` restarts.
- A process started outside Claude Code carries no
  `CLAUDE_CODE_SESSION_ID`, so it is never an orphan.
- An orphan group is reported once per run of `watch`, for as long as it
  lives.

## The same orphans reported again

- `watch` remembers which orphan groups it reported only while it runs. A
  restarted `watch` reports every orphan group still alive, in the scan it
  makes as it starts.
- A group is known by its topmost process, its pid and start time. When that
  process exits and others of the group live on, they are reported again,
  under their new topmost process.

## No coordinator run starts

`orchestrator config` shows the section; `<runtime>/coordinator/queue.json`
the queued events and their status; `runs.jsonl` the runs (coordinator.md).

- No `coordinator` section, or `"wake": false`: `watch` starts no run and
  queues nothing, by design. Events that came while `wake` was off are not
  queued later; a call already waiting when it turns on is reported once
  another call starts or stops waiting, or `watch` restarts.
- `config.json` cannot be read, or its `coordinator` section holds a
  hand-written mistake (a `max_budget_usd` at or below 0, for one): `watch`
  prints `orchestrator: <error>; no coordinator starts`.
- `watch` is not running, or reads another `config.json` (its
  `XDG_CONFIG_HOME` or `HOME` differs from the sessions').
- A coordinator holds, and events wait for it (`holder.json` names its pid):
  the setup conversation or an interactive coordinator is open, or a run
  left by a `watch` stopped with Ctrl-C still runs.
- A run just failed: the next one waits 60 s (`the coordinator run failed`).
- `claude` is not on `watch`'s `PATH`: `orchestrator: starting the
  coordinator: starting claude: …`. A `watch` started as a service has the
  user manager's `PATH` (commands.md, "Starting watch"). No retry is timed:
  the events wait for the next queued event, the end of another coordinator,
  or a restart of `watch`.
- No event needed one: `orphans` events never start a run. An
  `admission_wait` is written only for a call held back `wait_secs`; a call
  that started earlier makes none, and a `wait_secs` at or above
  `max_wait_secs` reports only calls waiting in the background. An
  `admission_wait` whose call already ran when a coordinator would
  take it is closed without a run.

## A coordinator run fails

`runs.jsonl` gives `exit`, `stopped`, `is_error` and `reply`;
`<runtime>/coordinator/last-run.err` and `last-run.json` hold what the latest
run printed.

- Not signed in: the run exits 1 with `is_error` true, and `reply` holds
  Claude Code's message, such as `Not logged in · Please run /login`.
- A model the account cannot use: `model` is checked only for its form.
- `stopped: true`: it reached `max_minutes` and `watch` stopped it.
- A `max_budget_usd` reached: exit 1, `is_error` true, no `reply`, and
  `last-run.json` says `Reached maximum budget`.
- After a failure its events are pending again and the next run waits 60 s;
  an event three failed runs took is closed.

## A session did not get the coordinator's message

The run's `reply` and the journal say whom it messaged. Delivery is Claude
Code's (coordinator.md, "Messages to sessions").

- The session skips permission prompts (`bypassPermissions`, or plan mode
  where bypass is available): Claude Code holds the message for its user to
  approve. An interactive session shows a dialog that drops the message when
  left unanswered past `dialogExpiry`, 5 minutes by default; a `-p` session
  drops it after the same delay. A `crossSessionInbound` set to `accept` in
  the session's settings delivers it whatever the mode.
- The session's `crossSessionInbound` setting is `hold`, which keeps the
  message undelivered, or `refuse`, which drops it.
- The session was started with `--bare`: it has no inbox.
- The session ended, or several live sessions share its name.
- A session reads a message between two tool calls: a long Bash call delays
  it.
- The journal shows the same request less than 15 minutes earlier: the role
  tells the coordinator not to ask again.

## The coordinator writes in another language

- `language` in the `coordinator` section decides, over a language asked
  in the user's `CLAUDE.md`. Unset, event runs write in English, setup and
  the interactive coordinator in the language of `LC_ALL`, `LC_MESSAGES` or
  `LANG`, the first set, and in English when that is C or POSIX.
- `orchestrator config coordinator --language <tag>` changes it, as does
  telling setup or the interactive coordinator. Removing it, to follow the
  system's language again, takes editing `config.json` by hand.
- orchestrator's own output, admission notices included, is always in
  English.

## The coordinator ignores an instruction

- The file must be `<config>/CLAUDE.md`: `orchestrator config` names it when
  it exists.
- It is read when a coordinator starts: an open interactive coordinator keeps
  the version it started with.
- An import missing, unreadable as text or too large is not included, and
  leaves a line saying so. One written in a code span or a fenced block, or
  more than four imports down, is not included either, with no line. An
  unescaped space ends the path: write it `\ `. A relative import resolves
  from the real file of a symlink.
- Any word starting with `@` in prose is read as an import, and shows as a
  missing file.
- `<runtime>/coordinator/role.md` holds the role and the instructions the
  latest coordinator was given.
- A run cannot do what its tools do not allow (coordinator.md, "What each
  kind may do"), whatever the instructions say.

## The interactive coordinator gets no events

- `watch` is not running, or works with other directories (another
  `config.json`, or another runtime directory through `--runtime-dir` or
  `XDG_RUNTIME_DIR`): only `watch` queues events, in its runtime directory.
- `watch` queues events only with `"wake": true`: with it off, nothing
  reaches the interactive coordinator either.
- It must keep `orchestrator coordinator next` running in the background
  (Claude Code shows a shell still running); ask it to start it again.
- `setup` and `coordinator` print `orchestrator: a coordinator is running
  (pid <pid>); waiting for it to end` while another holds. An event run ends
  within `max_minutes` while the `watch` that started it runs; one left by a
  `watch` stopped with Ctrl-C has no limit; a setup conversation or another
  interactive coordinator holds until it is closed.

## setup, coordinator or config stops at once, or refuses every value

The setup conversation cannot repair `config.json` in either case: fix or
remove it by hand (configuration.md).

- `Error: reading <path>`, then `Caused by:` and the reason: `config.json`
  exists but cannot be read whole, whether the file itself (permissions, a
  directory) or its content (invalid JSON, a missing or unknown field, a
  misspelt section; the cause names it, as in missing field `model`).
  `setup`, `coordinator` and every `config` command stop there and leave it
  alone.
- Every `config` command, and so setup, refuses with the same `Error: <why>`
  whatever value it is given: a hand-written value in the `coordinator`
  section makes no sense, such as `max_budget_usd (0) must be above 0`.
  `max_budget_usd` has no option; only an edit by hand fixes it.

## Hooks fail through the prefix

Claude Code runs a hook as `/bin/sh -c '<prefix path> <quoted command>'`,
with the path unquoted: an install path holding a space or a character
special to `sh` breaks every hook. Install where the path is plain.

## After a Claude Code update

orchestrator relies on Claude Code through the contracts of
[crates/claude-code/claude-code-dependency.md](https://github.com/hvn-p/orchestrator/blob/main/crates/claude-code/claude-code-dependency.md),
whose "Last verified" line names the version they were checked on. A broken
contract costs orchestration, not the command: commands no longer in job
groups, Bash calls in `job-other-*`, no admission notice reaching Claude,
nothing learned, sessions or orphans not seen. Compare `claude --version`
with that version. From a clone of the repository,

```sh
cargo test --test claude_code -- --ignored --nocapture
```

starts a real headless session of the installed Claude Code through the
clone's own build of `orchestrator launch`, not an installed `orchestrator`,
then a coordinator as `watch` would, and reports each contract as `ok` or
`BROKEN`. It needs a signed-in `claude` and a systemd user manager with
cgroup v2, and spends some tens of thousands of tokens, most read from the
prompt cache.

## Turning orchestrator off

- For one session: start it with `claude` alone.
- Admission only: remove `config.json` or its `admission` section.
- Coordinator runs: `orchestrator config coordinator --wake no`, or remove
  the `coordinator` section.
- Learning, events and coordinator runs: stop `watch`.
