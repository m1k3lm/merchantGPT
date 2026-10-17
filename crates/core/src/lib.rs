//! Chat core: the agent loop, confirmation gating, prompt building and the
//! backend that the UI drives with commands. Platform- and UI-agnostic.

pub mod agent;
pub mod backend;
pub mod config;
pub mod memory;
pub mod prompt;
pub mod redact;
pub mod scheduler;

pub use agent::{AgentConfig, AgentEvent, Decision, Responder};
pub use backend::{
    AuditSummary, Backend, BackendEvent, Command, ConversationSummary, GoogleBuiltIn,
    GoogleSignInState, LlmSettingSummary, ShopSummary, SkillSummary, TaskSummary,
};
pub use config::Config;
pub use connectors::Credentials;
