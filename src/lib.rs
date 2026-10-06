//! orchestrator: schedules the work of the Claude Code sessions running in
//! parallel on one machine, so development keeps going. See docs/design.md.
//!
//! The library serves the two binaries: `orchestrator`, and
//! `orchestrator-prefix`, the shell prefix Claude Code runs every command
//! through.

pub mod attribution;
pub mod cgroup;
pub mod events;
pub mod exits;
pub mod jobs;
pub mod launch;
pub mod memory;
pub mod peaks;
pub mod prefix;
pub mod pressure;
pub mod procfs;
pub mod recognise;
pub mod repository;
pub mod runtime;
pub mod sessions;
pub mod state;
pub mod watch;
