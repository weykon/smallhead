//! Project-scoped retrieval as a Monad, with service/process ownership in smallhead.
//! XERJ is an optional installed binary, not a dependency built into the agent.

mod backend;
mod monad;

pub use backend::XerjConfig;
pub use monad::{ProjectLookupTool, ProjectResearchMonad, ProjectResearchTool, RESEARCH_GUIDANCE};

#[cfg(test)]
mod tests;
