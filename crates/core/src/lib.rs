//! Chat core: the agent loop, confirmation gating, prompt building and the
//! backend that the UI drives with commands. Platform- and UI-agnostic.

pub mod agent;
pub mod backend;
pub mod config;
pub mod memory;
pub mod prompt;

pub use agent::{AgentConfig, AgentEvent, Decision, Responder};
pub use backend::{
    Backend, BackendEvent, Command, ConversationSummary, LlmSettingSummary, ShopSummary,
    SkillSummary,
};
pub use config::Config;
pub use connectors::Credentials;
