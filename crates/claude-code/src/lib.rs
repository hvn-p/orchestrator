//! Everything orchestrator relies on in Claude Code, in orchestrator's
//! terms: sessions, the commands the prefix receives, what a call shows the
//! model, starting an agent, messages. No other crate reads or writes a
//! Claude Code format. `claude-code-dependency.md`, next to this crate's
//! manifest, lists each contract.

pub mod agent;
pub mod call;
pub mod invocation;
pub mod launch;
pub mod messages;
pub mod sessions;
