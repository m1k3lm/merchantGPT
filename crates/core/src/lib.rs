//! Chat core: the agent loop, confirmation gating, prompt building and the
//! backend that the UI drives with commands. Platform- and UI-agnostic.

pub mod agent;
pub mod backend;
pub mod prompt;

pub use agent::{AgentConfig, AgentEvent, Decision, Responder};
pub use backend::{Backend, BackendEvent, Command};
pub use connectors::Credentials;
