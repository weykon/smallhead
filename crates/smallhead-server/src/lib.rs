//! smallhead-server — optional local A2A bridge + headless agent runner.
//!
//! The wire protocol is a symmetric, peer-to-peer message/task model ported from
//! big-head's `src/a2a.rs`, so one smallhead process can both *receive* messages
//! from another agent (the [`a2a::A2aServer`]) and *send* them ([`a2a::A2aClient`]).
//! See `docs/a2a-protocol.md` for the full wire contract.

pub mod a2a;
pub mod driver;
mod journal;

pub use a2a::{A2aClient, A2aMessage, A2aPermissions, A2aServer, A2aTaskReply};
pub use driver::{run_turn, TurnOutcome};
