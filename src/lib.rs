//! orchestrator keeps the Claude Code sessions running in parallel on one
//! Linux machine working within its finite resources. It schedules their
//! work rather than refusing it: a job may take longer, it is not prevented.
//!
//! Anything that can be decided without judgment is done by code and spends
//! no tokens. Judgment (priorities, exceptions, negotiating with a session)
//! is left to the coordinator, a Claude Code session that reads
//! orchestrator's events. Rust was chosen for a strict compiler, a start
//! time around a millisecond (the shell prefix runs before every command of
//! every session) and a resident `watch` of a few MB.
//!
//! # Components
//!
//! The library serves two binaries, `orchestrator` and
//! `orchestrator-prefix`, installed side by side.
//!
//! - **Launch** (`launch`, `cgroup`): `orchestrator launch` starts a session
//!   in a cgroup of its own and points Claude Code at the shell prefix.
//! - **The shell prefix** (`prefix`, `admission`, `recognise`, `peaks`,
//!   `repository`): every command a session starts goes through it. It
//!   places the command in a job group of its own, and holds a heavy Bash
//!   call back until memory covers it.
//! - **`orchestrator watch`** (`watch`, `jobs`, `pressure`, `exits`,
//!   `attribution`, `events`): the one long-running process. It sleeps until
//!   the kernel reports something, measures each finished Bash call and
//!   learns its peak, reports memory pressure and orphans, and wakes the
//!   coordinator.
//! - **The coordinator** (`coordinator`): a fresh Claude Code session for
//!   each batch of events that need judgment.
//! - **The read commands and the configuration** (`report`, `machine`,
//!   `config`): what a human or a coordinator reads, and the validated
//!   writes of the configuration.
//!
//! A session's cgroup tree:
//!
//! ```text
//! orchestrator-<pid>-<ms>.scope   a delegated systemd user scope
//! ├─ main/                        claude itself
//! ├─ job-bash-<pid>-<ms>/         one Bash call, with every process it starts
//! └─ job-other-<pid>-<ms>/        one hook, status line refresh or MCP server
//! ```
//!
//! The processes talk through files. The prefix writes job records,
//! reservations and waiting calls in the runtime directory, and reads the
//! learned peaks and the configuration. `watch` writes the peaks, the events
//! and the coordinator's queue. A coordinator starts from a briefing code
//! gathers, reads more through the read commands, and acts through the
//! commands it is allowed to run. Runtime data lives in
//! `$XDG_RUNTIME_DIR/orchestrator/` (`runtime`), learned peaks in
//! `$XDG_STATE_HOME/orchestrator/` (`state`), the configuration in
//! `$XDG_CONFIG_HOME/orchestrator/` (`config`).
//!
//! # Principles
//!
//! - **Never block work by failing**: outside an orchestrated session,
//!   without a configuration, or on any cgroup error, the prefix runs the
//!   command unchanged. When `launch` cannot get the session its scope, the
//!   session starts unorchestrated.
//! - **Commands typed outside Claude Code never wait**: they run outside any
//!   orchestrated session. A `!` command typed inside Claude Code is a Bash
//!   call like any other.
//! - **Hooks, the status line and MCP servers never queue**: they get a job
//!   group of their own and start at once.
//! - **Commands are known by what they used, never by name**: a hook or a
//!   shim in `PATH` sees `pnpm typecheck`, not the `tsc` processes it
//!   starts, and `node_modules/.bin` bypasses shims. The kernel puts every
//!   descendant of a command in its job group, whatever its name, language
//!   or depth.
//! - **No full command line in an event**: arguments read from `/proc` can
//!   hold credentials, and the coordinator hands what it reads to a model
//!   (see `events`).
//! - **Claude Code is today's only host, behind a listed boundary**: what
//!   orchestrator relies on in it is a contract of
//!   `docs/claude-code-dependency.md`, and the code relying on one carries
//!   that contract's marker.
//!
//! Decisions and measurements made before the GitHub issues held them are
//! in the design document as it stood when it was removed:
//! <https://github.com/hvn-p/orchestrator/blob/957d2f9f119c7d0fb59c4e13552aeb6ce8a2b1fc/docs/design.md>.

pub mod admission;
pub mod attribution;
pub mod cgroup;
pub mod config;
pub mod coordinator;
pub mod events;
pub mod exits;
pub mod jobs;
pub mod language;
pub mod launch;
pub mod machine;
pub mod memory;
pub mod peaks;
pub mod prefix;
pub mod pressure;
pub mod procfs;
pub mod recognise;
pub mod report;
pub mod repository;
pub mod runtime;
pub mod sessions;
pub mod state;
pub mod watch;
