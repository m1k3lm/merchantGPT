//! LLM provider abstraction: streaming chat with native tool calling. Every
//! provider is normalised into `ChatMessage` in and `StreamEvent` out.

pub mod anthropic;
pub mod fake;
pub mod google;
mod http;
pub mod openai;
pub mod sse;

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
pub use tools::{ToolCall, ToolDefinition, ToolResult};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessage {
    System {
        text: String,
    },
    User {
        text: String,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        /// The provider's own content blocks for this turn (Anthropic thinking
        /// blocks, Gemini thought signatures). They must be replayed unchanged
        /// to the same provider; other providers rebuild from text and calls.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_content: Option<ProviderContent>,
    },
    /// All results answering one assistant turn. Kept together because
    /// Anthropic requires them in a single user message.
    ToolResults {
        results: Vec<ToolResult>,
    },
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self::System { text: text.into() }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::User { text: text.into() }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant {
            text: text.into(),
            tool_calls: Vec::new(),
            provider_content: None,
        }
    }

    pub fn assistant_with_calls(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self::Assistant {
            text: text.into(),
            tool_calls,
            provider_content: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderContent {
    pub provider: ProviderId,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    /// A safety system declined; any partial output must not be trusted.
    Refusal,
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    /// A complete tool call. Decoders accumulate streamed argument fragments
    /// internally. Arguments that are not valid JSON arrive as `Value::String`
    /// with the raw text, so the agent can report the error back to the model.
    ToolCall(ToolCall),
    Usage(Usage),
    /// The turn's raw content blocks, emitted just before `Done`.
    ProviderContent(serde_json::Value),
    Done(StopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolChoice {
    #[default]
    Auto,
    /// Tools stay declared (history may reference them) but must not be called.
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: ToolChoice,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("provider error {status}: {message}")]
    Http { status: u16, message: String },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("could not decode provider response: {0}")]
    Decode(String),
    #[error("configuration error: {0}")]
    Config(String),
}

pub type EventStream = BoxStream<'static, Result<StreamEvent, LlmError>>;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    async fn stream(&self, req: ChatRequest) -> Result<EventStream, LlmError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    OpenAi,
    Anthropic,
    Google,
    DeepSeek,
}

impl ProviderId {
    pub const ALL: [ProviderId; 4] = [
        ProviderId::OpenAi,
        ProviderId::Anthropic,
        ProviderId::Google,
        ProviderId::DeepSeek,
    ];

    pub fn id(self) -> &'static str {
        match self {
            ProviderId::OpenAi => "openai",
            ProviderId::Anthropic => "anthropic",
            ProviderId::Google => "google",
            ProviderId::DeepSeek => "deepseek",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.id() == id)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ProviderId::OpenAi => "OpenAI",
            ProviderId::Anthropic => "Anthropic",
            ProviderId::Google => "Google Gemini",
            ProviderId::DeepSeek => "DeepSeek",
        }
    }

    /// Pre-filled in the settings form; the merchant can type any model id.
    pub fn suggested_model(self) -> &'static str {
        match self {
            ProviderId::OpenAi => "gpt-5",
            ProviderId::Anthropic => "claude-opus-5-5",
            ProviderId::Google => "gemini-2.5-pro",
            ProviderId::DeepSeek => "deepseek-chat",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            ProviderId::OpenAi => "https://api.openai.com/v1",
            ProviderId::Anthropic => "https://api.anthropic.com",
            ProviderId::Google => "https://generativelanguage.googleapis.com",
            ProviderId::DeepSeek => "https://api.deepseek.com",
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct ProviderConfig {
    pub provider: ProviderId,
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: String,
}

impl ProviderConfig {
    /// True when no custom base URL points the client at a proxy or gateway.
    pub fn is_official_endpoint(&self) -> bool {
        self.base_url().trim_end_matches('/') == self.provider.default_base_url()
    }

    pub fn base_url(&self) -> String {
        self.base_url
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or(self.provider.default_base_url())
            .trim_end_matches('/')
            .to_string()
    }
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("provider", &self.provider)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish()
    }
}

pub fn build_provider(
    cfg: &ProviderConfig,
    http: reqwest::Client,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    if cfg.api_key.trim().is_empty() {
        return Err(LlmError::Config(format!(
            "no API key set for {}",
            cfg.provider.display_name()
        )));
    }
    match cfg.provider {
        ProviderId::OpenAi | ProviderId::DeepSeek => {
            Ok(Arc::new(openai::OpenAiProvider::new(cfg.clone(), http)))
        }
        ProviderId::Anthropic => Ok(Arc::new(anthropic::AnthropicProvider::new(
            cfg.clone(),
            http,
        ))),
        ProviderId::Google => Ok(Arc::new(google::GoogleProvider::new(cfg.clone(), http))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_message_serialises_with_role_tag() {
        let m = ChatMessage::user("hi");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v, serde_json::json!({"role": "user", "text": "hi"}));
        assert_eq!(serde_json::from_value::<ChatMessage>(v).unwrap(), m);
    }

    #[test]
    fn provider_config_debug_redacts_key() {
        let cfg = ProviderConfig {
            provider: ProviderId::OpenAi,
            api_key: "sk-secret".into(),
            base_url: None,
            model: "m".into(),
        };
        assert!(!format!("{cfg:?}").contains("sk-secret"));
        assert_eq!(cfg.base_url(), "https://api.openai.com/v1");
    }

    #[test]
    fn build_provider_requires_key() {
        let cfg = ProviderConfig {
            provider: ProviderId::DeepSeek,
            api_key: " ".into(),
            base_url: None,
            model: "m".into(),
        };
        assert!(matches!(
            build_provider(&cfg, reqwest::Client::new()),
            Err(LlmError::Config(_))
        ));
    }
}
