# The local API

`orchestrator watch` serves the state over HTTP on a Unix socket, and pushes
the events as they come. Every read of the state goes through it: the read
commands (`sessions`, `peaks`, `admission`, `machine`, `config`),
`config admission`, `setup`, `coordinator` and `coordinator next` ask it, and
stop with an error when no `watch` answers (events.md). A program of any
language can ask it too.

## The socket

`<runtime>/api.sock`, where `<runtime>` is `watch`'s runtime directory
(`$XDG_RUNTIME_DIR/orchestrator`, else `/run/user/<uid>/orchestrator`, or
`watch --runtime-dir`). The commands always ask the one in the default
runtime directory.

- Only its owner can connect (mode `0600`).
- One `watch` serves a runtime directory: a second one stops at start with
  `another orchestrator watch answers at <socket>`. A socket left by a
  `watch` that ended is replaced by the next one.
- A Unix socket's path is limited to about a hundred bytes: a deeper runtime
  directory makes `watch` stop at start (events.md).

```sh
curl --unix-socket "$XDG_RUNTIME_DIR/orchestrator/api.sock" http://localhost/machine
curl -N --unix-socket "$XDG_RUNTIME_DIR/orchestrator/api.sock" http://localhost/events
```

## Requests

HTTP/1.1, `GET` only, one request per connection: each answer closes it.
The host name is ignored. Sizes are in MB.

| Request | Answer, JSON | Same as |
| :- | :- | :- |
| `/sessions`, `/sessions?heads=true` | `available_mb`; `sessions`, largest first, each with `name`, `session_id`, `rss_mb` and `largest` (`pid`, `rss_mb`, `command`); `orphans`, each with `rss_mb`, `processes`, `session_id` and `root` (`pid`, `rss_mb`, `command`) | `orchestrator sessions [--heads]` |
| `/admission` | `thresholds`, either `{"on": {"heavy_mb", "margin_mb", "max_wait_secs", "max_background_wait_secs"}}` or `{"off": "<why>"}`; `available_mb`, `held_mb`, `free_mb`; `waiting`, in the order memory goes to them, each with `job`, `priority` (true when it was given priority), `session`, `waited_secs`, `peak_mb`, `need_mb`, `command`; `reserved`, each with `session`, `held_mb`, `peak_mb`, `current_mb` (null without a memory controller), `command` | `orchestrator admission` |
| `/peaks`, `/peaks?at_least_mb=<MB>&most=<n>` | `at_least_mb`; `repositories`, each with `repository` and `commands`, heaviest first, each with `peak_mb`, `id`, `label` and `recent`, its latest calls oldest first as `[peak_mb, ran_alone]` | `orchestrator peaks` |
| `/machine` | `mem_total_mb`, `mem_available_mb`, `swap_total_mb`, `swap_free_mb`, `cpus`, `pressure`, `delegated` (the controllers, or null outside a systemd user manager), `slice` | `orchestrator machine` |
| `/config` | `path`; `config`, the content of `config.json` or null without the file; `instructions`, the coordinators' `CLAUDE.md` or null without it | `orchestrator config` |

- `heads=true` shows each process by the head of its command line, as events
  do; without it, `command` is the start of the real command line, which can
  hold credentials (commands.md, `orchestrator sessions`).
- `at_least_mb` keeps the commands that heavy, `most` that many of the
  heaviest. A value that is not a number is ignored.

An error answers `{"error": "<why>"}`: status 500 when `watch` cannot read
what was asked (the same message a command prints), 404 for another path,
405 for another method.

## The event stream

`/events` answers `200` with `Content-Type: text/event-stream`, a
server-sent event stream as the HTML Living Standard defines it, and stays
open:

```
data: {"at":1791210633,"kind":"memory_pressure","available_mb":700,…}

: keep-alive

```

- Each event is one `data:` line holding the JSON line `events.jsonl` gets
  (events.md), then a blank line.
- A comment line, `: keep-alive`, comes after 15 seconds without an event.
- The stream carries the events from its connection on; `events.jsonl` keeps
  the ones before.
