# The coordinator

A coordinator is a Claude Code session that orchestrator starts, with a role
of its own, for what needs judgment. It reads the state, messages sessions,
gives waiting calls priority, notes what it did in a journal and, during
setup or when the user asks,
writes the configuration. It never stops, pauses, slows down or kills
anything, and never refuses work itself: it asks, and each session decides.

There are three kinds:

- **The setup conversation**: `orchestrator setup`, or `orchestrator
  coordinator` when there is no `config.json`.
- **Event runs**: `watch` starts one for each batch of events that need
  judgment, once the user has agreed (`wake`).
- **The interactive coordinator**: `orchestrator coordinator`, open for as
  long as the user keeps it.

Each is a fresh session, never resumed: what one knows of the others is in
its journal and the user's instructions. Every coordinator runs under the
session name `orchestrator-coordinator`: messages it sends arrive from
`@orchestrator-coordinator`, and while it runs it shows under that name in
`orchestrator sessions` and in `memory_pressure` events, like any Claude Code
session.

## What every coordinator loads, and not

- Its role, then, for setup, the setup instructions, then the user's
  instructions for coordinators (below), all through its system prompt.
- None of the user's Claude Code settings, hooks, plugins or CLAUDE.md, no
  MCP server, no auto memory.
- No `CLAUDE.md`, `CLAUDE.local.md`, `.claude/rules/` or `AGENTS.md` from its
  working directory or any directory above it, the home directory's
  `.claude/CLAUDE.md` included, which Claude Code would otherwise load as a
  project's instructions.
- Claude Code's project settings from its working directory,
  `<state>/coordinator/.claude/settings.json`, do apply (not
  `settings.local.json`), and so do the settings and instructions an
  organization manages for the account.
- It may read files only in its own directory and the configuration's
  (`<config>`); setup and the interactive coordinator also in the
  directories holding files the instructions import.

## What each kind may do

| | Event run | Setup | Interactive |
| :- | :- | :- | :- |
| Claude Code tools | Bash, Read, SendMessage, ListAgents | Bash, Read, Edit, Write | Bash, Read, Edit, Write, SendMessage, ListAgents |
| Without asking | the read commands, `coordinator note`, `admission priority`, messages | the read commands, `coordinator note`, `admission priority`, `config admission`, `config coordinator`, editing `<config>/CLAUDE.md` | the read commands, `coordinator note`, `admission priority`, `coordinator next`, messages |
| Anything else | denied | asked of the user | asked of the user |
| Model | `coordinator.model` | Claude Code's default | Claude Code's default |
| `max_minutes`, `max_budget_usd` | apply | do not apply | do not apply |

The read commands are `orchestrator sessions --heads`, `orchestrator
admission`, `orchestrator peaks`, `orchestrator machine` and `orchestrator
config`, allowed only exactly as written; `orchestrator coordinator note …`
takes any text, and `orchestrator admission priority` any jobs, or none. They are the `orchestrator` that started the coordinator,
its directory first on the coordinator's `PATH`.

An event run is `claude -p` in Claude Code's `dontAsk` mode: what it was not
given is denied without asking, and `<runtime>/coordinator/last-run.json`
lists those calls under `permission_denials`. It cannot edit a file or write
the configuration: when the thresholds look wrong, it says what it would
change, in its note and its reply. Setup and the interactive coordinator run
in Claude Code's `default` mode, with the user at the terminal.

## One at a time

`<runtime>/coordinator/holder.json` names the process running a coordinator,
by pid and start time. Event runs, `setup` and `coordinator` all take it
first; it frees itself when that process ends. `setup` and `coordinator`
wait for a running one, printing `orchestrator: a coordinator is running
(pid <pid>); waiting for it to end`, then replace themselves with `claude`.
While the setup conversation or an interactive coordinator is open, `watch`
starts no run: events wait in the queue.

## The setup conversation

`orchestrator setup` opens it, and so does `orchestrator coordinator` when
`config.json` does not exist. The first time a coordinator opens in
`<state>/coordinator` at the terminal, Claude Code asks whether to trust that
folder; its preselected answer, `No, exit`, ends the coordinator.

- It writes in the configured `language`. With none set, it starts in the
  system's language (Language, below) and offers to switch in its first
  message.
- It says what orchestrator does and what it will set up, looks at the
  machine from what it was given, then asks one subject at a time, in plain
  words: whether `watch` may wake a coordinator by itself, the model of those
  runs (it proposes `claude-sonnet-5-5`), the user's priorities, and the
  admission thresholds, which it proposes from the machine's facts and
  adjusts to the answers.
- It writes each part once the user agrees, through `orchestrator config
  coordinator` and `orchestrator config admission` (commands.md), never by
  editing `config.json`. When a command refuses a value, it explains why and
  proposes one it accepts.
- The priorities go in the user's instructions for coordinators. It offers
  to create `<config>/CLAUDE.md` when it is missing.
- It notes what it set up in its journal, then says what it wrote and how to
  change it later. It messages no session. `/exit` leaves it.

Setup cannot repair `config.json`; the user fixes it by hand:

- A file that cannot be read stops `setup` and `coordinator` before anything
  starts, with `Error: reading <path>`.
- A hand-written mistake in the `coordinator` section, such as a
  `max_budget_usd` at 0, lets setup start, but every config command it would
  write through refuses the file until the section is fixed.

## Event runs

### What starts one

With `"wake": true` in the `coordinator` section and `watch` running, three
kinds of events go to the queue:

- `memory_pressure` (events.md);
- `admission_wait`, which `watch` writes when admission has held a Bash call
  back `wait_secs` (events.md);
- `oom_kill`, when its session cannot see the kill: a process a Bash call
  left running after it ended, a hook, the status line or an MCP server
  (events.md). A kill during a Bash call, which its result shows, and one in
  `main/` do not.

`orphans` and `job_pressure` events never do. A run starts as soon as an event is queued and no
coordinator is running, and again when a run ends with events pending.

- While `wake` is off, nothing is queued and no `admission_wait` is written;
  events pending from before stay pending. Once `wake` is on again, they go
  with the next queued event.
- While `config.json` cannot be read, or its `coordinator` section holds a
  hand-written mistake, the same: no run starts and nothing is queued, and
  `watch` prints `orchestrator: <why>; no coordinator starts`.
- A call already waiting when `wake` turns on is reported only once another
  call starts or stops waiting, or `watch` restarts.
- `watch` remembers which waiting calls it reported only while it runs: a
  restarted `watch` reports a call still waiting again.

### The queue

`<runtime>/coordinator/queue.json` is a JSON array of the events, oldest
first, each an object with:

- `key`: `memory_pressure`, `admission_wait <job>`, or `oom_kill <job>`;
- `count`: how many events merged into it;
- `first_at`: when the first of them came, in seconds since the epoch;
- `event`: the latest of them, as `events.jsonl` holds it; for `oom_kill`,
  its `killed` is the sum of theirs;
- `status`: `pending`, `in_progress` or `done`;
- `failures`: how many failed runs took it.

The statuses:

- `pending`: waiting for a coordinator. A new event merges into a pending
  one of the same key, which keeps its place. A `memory_pressure` arriving
  while another is in progress is queued separately.
- `in_progress`: taken by a run or by the interactive coordinator.
- `done`: handled. An `admission_wait` whose call no longer waits when a
  coordinator would take it is done without being handled.

What changes a status:

- A run that exits with status 0, was not stopped and whose `is_error` is not
  `true` has handled its events: done.
- A run that ends otherwise gives them back, pending, counting a failure, and
  `watch` waits 60 s before the next run (`orchestrator: the coordinator run
  failed (see <runtime>/coordinator/last-run.err); the next one waits 60 s`).
  An event three failed runs took is closed, done: `orchestrator: <key>
  failed 3 coordinator runs; closed`.
- The interactive coordinator's batch is done when it asks for the next one.
- When the holder ends otherwise (an interactive coordinator closed, a run
  that could not be started, a run left over by a stopped `watch`), its
  events in progress are pending again, with no failure counted.

The 20 latest done events stay in the file; older ones go.

### What a run is given

The prompt carries the time; the events, one JSON object per line with
`count` and `first_at` added; the language to write in; and what `watch`
gathered when it started the run: the configuration, the machine, the
sessions as `orchestrator sessions --heads` shows them, the calls waiting for
memory and the reservations, the 15 heaviest commands learned at or above
`heavy_mb` (of all commands when there are no thresholds), the last 40 lines
of the journal, and where the user's instructions are. It decides from that.

### Messages to sessions

A run messages a session by the name Claude Code records for it, asks for no
answer since it ends with its reply, and does not message itself. Its role
tells it: one message per session per run; not the same request to the same
session within 15 minutes, checked in its journal; ask the sessions the
user's instructions rank lowest first, and leave alone those they protect.

Delivery is Claude Code's:

- A session in a mode that prompts for permissions gets the message between
  two tool calls, or at once when idle.
- A session that skips permission prompts (`bypassPermissions`, and plan
  mode where bypass is available) holds it for its user to approve: an
  interactive session shows a dialog, which drops the message when left
  unanswered past `dialogExpiry`, 5 minutes by default; a `-p` session drops
  it after the same delay.
- A session's `crossSessionInbound` setting overrides both: `accept`
  delivers, whatever its mode; `hold` keeps the message undelivered; `refuse`
  drops it. A `--bare` session has no inbox at all.

### Priority to waiting calls

A run may give waiting calls priority with `orchestrator admission
priority` (commands.md), which no call then passes until it runs or is
refused (configuration.md, "Admitting"). Its role tells it: when several
calls wait, decide from the user's instructions and what the sessions do
whether some matter more than the others; give priority only to those, in
their order of arrival unless it has a reason for another; look again at the
calls given priority before, at each run, and take it back from one that no
longer matters more; otherwise leave the order alone. A call given priority
holds back every call behind it, and a call held back in the foreground is
refused past `max_wait_secs`. A call refused and run again is a new job,
without the priority of the one before.

### Bounds

- **Time**: past `max_minutes`, `watch` sends the run's process group
  SIGTERM, then SIGKILL 10 s later (`orchestrator: the coordinator ran past
  its time limit; stopping it`). The run counts as failed.
- **Spending**: no cap, unless the section holds `max_budget_usd`, written
  by hand and above 0, which Claude Code checks against its own estimate at
  API list price; a subscription counts tokens against its quota instead.
  A run can spend more than the amount before Claude Code stops it. A run
  stopped there exits with status 1, `is_error` true and no
  `reply` (`last-run.json` says `Reached maximum budget`): it counts as
  failed, like any other.

A run runs in its own process group. `watch` stopped with Ctrl-C leaves it
running with no time limit. Its output still goes to `last-run.json` and
`last-run.err`, but no line goes to `runs.jsonl`, which only the `watch` that
started a run writes. The next `watch` waits for it as for any other
coordinator, then gives back the events it held, with no failure counted:
the next coordinator handles them again. Stopping a `watch` started as a
systemd unit (`systemctl --user stop`) ends the run with it.

### When a run cannot start

When `claude` cannot be started (`orchestrator: starting the coordinator:
…`), the events go back to pending with no failure counted, and no retry is
timed: they go with the next queued event, the end of another coordinator,
or a restart of `watch`.

### runs.jsonl

`<runtime>/coordinator/runs.jsonl`, one line per run `watch` started:

```json
{"at":1791300614,"events":1,"secs":7,"turns":3,"input_tokens":3270,"cache_read_tokens":18658,"output_tokens":628,"exit":0,"stopped":false,"is_error":false,"list_price_estimate_usd":0.0230836,"reply":"I messaged …"}
```

| Field | Meaning |
| :- | :- |
| `at` | When the run started, seconds since the epoch. |
| `events` | How many queued events it took. |
| `secs` | How long it lasted. |
| `turns` | Its turns, as Claude Code counts them. |
| `input_tokens` | Input tokens, cache writes included. |
| `cache_read_tokens` | Input tokens read from the prompt cache. |
| `output_tokens` | Output tokens. |
| `exit` | Its exit code, `null` when a signal ended it. |
| `stopped` | `watch` stopped it at its time limit. |
| `is_error` | Claude Code reported the run as failed. |
| `list_price_estimate_usd` | Claude Code's estimate at API list price; not what a subscription counts. |
| `reply` | Its final reply: what it says it did. |

A field Claude Code did not report is `null`. `last-run.json` holds what the
latest run printed (Claude Code's JSON result, `permission_denials`
included), `last-run.err` its standard error.

## The interactive coordinator

`orchestrator coordinator`, when `config.json` exists, opens a coordinator
for the user at the terminal, whether or not `wake` is on.

- It runs `orchestrator coordinator next` in the background. That command
  marks done the events it handed over last time, waits until events are
  pending, takes them, prints them with the state, and ends; Claude Code
  then wakes the coordinator, which handles them and starts the command
  again. Events reach it only while `wake` is on, since `watch` queues
  nothing otherwise.
- `coordinator next` does not check who holds the coordinator: run by hand,
  it marks every event in progress done and takes the pending ones.
- While it is open, `watch` starts no run. When it closes, the events it
  took but did not handle are pending again and `watch` takes over. Leaving
  while `coordinator next` runs, Claude Code shows `Background work is
  running` and offers `Exit and stop tasks` or `Stay`. Leaving stops the
  coordinator, `coordinator next` included: Claude Code's agent view is
  turned off in every coordinator, so none can be moved to the background.
- It may read the state, note, give waiting calls priority, and message
  sessions, which may answer it
  while it is open. Told a priority or a lasting instruction, it proposes the
  change and the file of the instructions it belongs in, and writes it once
  the user agrees, after Claude Code's permission prompt. It changes the
  thresholds only when asked, also after a permission prompt.

## The user's instructions for coordinators

`<config>/CLAUDE.md`, next to `config.json` (`$XDG_CONFIG_HOME/orchestrator`,
else `~/.config/orchestrator`): a CLAUDE.md that every coordinator reads
(setup, event runs, the interactive one) and no other session does. The
user's priorities go there, with anything else coordinators should follow.

- orchestrator reads it each time it starts a coordinator and appends it,
  verbatim, to the coordinator's system prompt, after the role, as taking
  precedence over the role's defaults; the coordinator's tools stay the
  same. An edit applies to the next coordinator. Claude Code itself does not
  load it.
- It may import other files with `@path`, by Claude Code's rules: an absolute
  path, `~/` for the home directory, or a path relative to the importing
  file. A symlinked file is followed, and its relative imports resolve from
  the real file. Any word starting with `@` in prose is an import; one in a
  code span or a fenced block is not. A space in a path is written `\ `;
  unescaped, the path ends there.
- Imports go four hops deep; a file further down is dropped with no word.
  Each file is read once, which also stops a cycle.
- A file that is missing, cannot be read as text, is over 128 KiB, or would
  bring the whole past 256 KiB is left out, and the coordinator reads a line
  saying so in its place. So a word such as `@someone` in prose shows as a
  missing file.
- The coordinator sees each file under a line `Contents of <path>:`, so it
  knows where each instruction lives.

## Language

`language` in the `coordinator` section, a tag such as `fr`, `en` or `pt-BR`,
is the language every coordinator writes in: replies, journal notes and
messages to sessions. The role and every prompt state it, whatever other
instructions say, a language asked in `CLAUDE.md` included: to change it,
change `language`.

- Unset, event runs write in English; setup and the interactive coordinator
  use the system's language, from the first set of `LC_ALL`, `LC_MESSAGES`
  and `LANG` (`fr_FR.UTF-8` gives `fr-FR`), English when none is set or it is
  C or POSIX.
- `orchestrator config coordinator --language <tag>` sets it, as do setup
  and the interactive coordinator when asked. Once set, only editing
  `config.json` by hand removes it.
- What orchestrator itself prints, admission notices included, is in
  English.

## The journal

`<state>/coordinator/journal.md`, written through `orchestrator coordinator
note "<text>"`: one line per line of text, as `YYYY-MM-DD HH:MM UTC  <text>`.
The latest 200 lines are kept, and every coordinator is given the last 40.
Kept across reboots; removing the file makes coordinators forget.

## Turning it off

- `orchestrator config coordinator --wake no`, or removing the `coordinator`
  section: `watch` starts no run and queues nothing. Setup and the
  interactive coordinator stay available on demand.
- Stopping `watch` stops events and new runs. A run already going ends with
  a `watch` stopped as a systemd unit, not with one stopped by Ctrl-C
  (Bounds, above).
