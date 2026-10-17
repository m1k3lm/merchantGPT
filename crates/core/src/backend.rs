//! Owns the session state (LLM provider, connected shop, tools, history) and
//! executes UI commands one at a time. Cancellation and confirmations bypass
//! the command queue: the UI holds the turn's `CancellationToken` and answers
//! confirmation cards through their `Responder` directly.

use std::sync::Arc;

use connectors::{
    Connector, ConnectorError, ConnectorKind, ConnectorRegistry, Credentials, HttpTransport,
};
use llm::{ChatMessage, LlmError, LlmProvider, ProviderConfig};
use tools::{CancellationToken, ToolContext, ToolRegistry};

use crate::agent::{Agent, AgentConfig, AgentEvent, AuditSink, NoAudit, UiGate};
use crate::prompt::{system_prompt, PromptContext};

#[derive(Debug)]
pub enum Command {
    SetLlm(ProviderConfig),
    ConnectShop {
        url: String,
        kind: ConnectorKind,
        credentials: Credentials,
    },
    Send {
        text: String,
        cancel: CancellationToken,
    },
    NewConversation,
}

#[derive(Debug, Clone)]
pub enum BackendEvent {
    Agent(AgentEvent),
    /// Sent once after every `Send`, whether it succeeded or not.
    TurnEnded {
        error: Option<String>,
    },
    Notice(String),
    Error(String),
    ShopConnected {
        url: String,
        kind: ConnectorKind,
    },
    LlmConfigured {
        provider: llm::ProviderId,
        model: String,
    },
    ConversationReset,
}

pub type BackendSink = Arc<dyn Fn(BackendEvent) + Send + Sync>;
pub type ProviderFactory =
    Arc<dyn Fn(&ProviderConfig) -> Result<Arc<dyn LlmProvider>, LlmError> + Send + Sync>;

struct ActiveShop {
    url: String,
    connector: Arc<dyn Connector>,
}

pub struct Backend {
    sink: BackendSink,
    provider_factory: ProviderFactory,
    transport: Arc<dyn HttpTransport>,
    audit: Arc<dyn AuditSink>,
    agent_defaults: AgentConfig,
    llm: Option<(ProviderConfig, Arc<dyn LlmProvider>)>,
    shop: Option<ActiveShop>,
    tools: ToolRegistry,
    history: Vec<ChatMessage>,
}

impl Backend {
    pub fn new(
        sink: BackendSink,
        provider_factory: ProviderFactory,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            sink,
            provider_factory,
            transport,
            audit: Arc::new(NoAudit),
            agent_defaults: AgentConfig::new(""),
            llm: None,
            shop: None,
            tools: ToolRegistry::new(),
            history: Vec::new(),
        }
    }

    pub fn with_agent_defaults(mut self, cfg: AgentConfig) -> Self {
        self.agent_defaults = cfg;
        self
    }

    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = audit;
        self
    }

    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    fn emit(&self, e: BackendEvent) {
        (self.sink)(e)
    }

    pub async fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::SetLlm(cfg) => self.set_llm(cfg),
            Command::ConnectShop {
                url,
                kind,
                credentials,
            } => self.connect_shop(&url, kind, &credentials).await,
            Command::Send { text, cancel } => {
                let error = self.send(text, cancel).await.err();
                self.emit(BackendEvent::TurnEnded { error });
            }
            Command::NewConversation => {
                self.history.clear();
                self.emit(BackendEvent::ConversationReset);
            }
        }
    }

    fn set_llm(&mut self, cfg: ProviderConfig) {
        match (self.provider_factory)(&cfg) {
            Ok(provider) => {
                self.emit(BackendEvent::LlmConfigured {
                    provider: cfg.provider,
                    model: cfg.model.clone(),
                });
                self.llm = Some((cfg, provider));
            }
            Err(e) => self.emit(BackendEvent::Error(e.to_string())),
        }
    }

    async fn connect_shop(&mut self, url: &str, kind: ConnectorKind, creds: &Credentials) {
        let url = match normalize_shop_url(url) {
            Ok(u) => u,
            Err(e) => return self.emit(BackendEvent::Error(e)),
        };
        let connector = match ConnectorRegistry::build(kind, &url, creds, self.transport.clone()) {
            Ok(c) => c,
            Err(e) => return self.emit(BackendEvent::Error(e.to_string())),
        };
        match connector.verify().await {
            Ok(()) => {}
            Err(ConnectorError::NotImplemented(what)) => self.emit(BackendEvent::Notice(format!(
                "Connected, but {what} is not implemented yet, so shop tools will report errors."
            ))),
            Err(e) => {
                return self.emit(BackendEvent::Error(format!(
                    "Could not connect to {url}: {e}"
                )))
            }
        }
        self.tools = ToolRegistry::new();
        self.tools.extend(connector.tools());
        self.shop = Some(ActiveShop {
            url: url.clone(),
            connector,
        });
        self.emit(BackendEvent::ShopConnected { url, kind });
    }

    fn prompt_context(&self) -> PromptContext {
        PromptContext {
            today: Some(chrono::Local::now().date_naive()),
            shop_name: None,
            shop_url: self.shop.as_ref().map(|s| s.url.clone()),
            connectors: self
                .shop
                .iter()
                .map(|s| s.connector.kind().display_name())
                .collect(),
            skills: Vec::new(),
            memory: Vec::new(),
        }
    }

    async fn send(&mut self, text: String, cancel: CancellationToken) -> Result<(), String> {
        let Some((cfg, provider)) = self.llm.clone() else {
            return Err("Configure an AI provider in Settings first.".into());
        };
        self.history.push(ChatMessage::user(text));
        let sink = self.sink.clone();
        let agent_sink: crate::agent::EventSink = Arc::new(move |e| sink(BackendEvent::Agent(e)));
        let config = AgentConfig {
            model: cfg.model.clone(),
            ..self.agent_defaults.clone()
        };
        let gate = UiGate::new(agent_sink.clone());
        let agent = Agent {
            provider: provider.as_ref(),
            tools: &self.tools,
            gate: &gate,
            audit: self.audit.as_ref(),
            sink: agent_sink,
            config: &config,
        };
        let ctx = ToolContext {
            shop_id: self.shop.as_ref().map(|s| s.url.clone()),
            unattended: false,
            cancel,
        };
        let prompt = system_prompt(&self.prompt_context());
        agent
            .run_turn(&prompt, &mut self.history, &ctx)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Accepts what merchants paste ("shop.com", "https://shop.com/") and returns
/// a canonical https base URL without a trailing slash.
pub fn normalize_shop_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter the shop URL.".into());
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = reqwest::Url::parse(&with_scheme).map_err(|e| format!("Invalid shop URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("Invalid shop URL: {trimmed}"));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use connectors::transport::FakeTransport;
    use connectors::Platform;
    use llm::fake::FakeProvider;
    use llm::ProviderId;
    use std::sync::Mutex;

    struct Fixture {
        backend: Backend,
        events: Arc<Mutex<Vec<BackendEvent>>>,
        provider: Arc<FakeProvider>,
    }

    fn fixture() -> Fixture {
        let events = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider::new());
        let ev = events.clone();
        let p = provider.clone();
        let backend = Backend::new(
            Arc::new(move |e| ev.lock().unwrap().push(e)),
            Arc::new(move |_| Ok(p.clone() as Arc<dyn LlmProvider>)),
            Arc::new(FakeTransport::new()),
        );
        Fixture {
            backend,
            events,
            provider,
        }
    }

    fn llm_cfg() -> ProviderConfig {
        ProviderConfig {
            provider: ProviderId::OpenAi,
            api_key: "k".into(),
            base_url: None,
            model: "m".into(),
        }
    }

    #[tokio::test]
    async fn send_without_llm_reports_error_and_ends_turn() {
        let mut f = fixture();
        f.backend
            .handle(Command::Send {
                text: "hi".into(),
                cancel: CancellationToken::new(),
            })
            .await;
        let events = f.events.lock().unwrap();
        assert!(matches!(
            events.last(),
            Some(BackendEvent::TurnEnded { error: Some(e) }) if e.contains("Settings")
        ));
        assert!(f.backend.history().is_empty());
    }

    #[tokio::test]
    async fn send_streams_reply_and_keeps_history() {
        let mut f = fixture();
        f.provider.push_text("Hello merchant");
        f.backend.handle(Command::SetLlm(llm_cfg())).await;
        f.backend
            .handle(Command::Send {
                text: "hi".into(),
                cancel: CancellationToken::new(),
            })
            .await;
        assert_eq!(f.backend.history().len(), 2);
        let events = f.events.lock().unwrap();
        assert!(events.iter().any(
            |e| matches!(e, BackendEvent::Agent(AgentEvent::TextDelta(t)) if t == "Hello merchant")
        ));
        assert!(matches!(
            events.last(),
            Some(BackendEvent::TurnEnded { error: None })
        ));
        let req = &f.provider.requests()[0];
        assert!(
            matches!(&req.messages[0], ChatMessage::System { text } if text.contains("+ button"))
        );
        assert_eq!(req.model, "m");
    }

    #[tokio::test]
    async fn connecting_a_stub_shop_registers_tools_with_a_notice() {
        let mut f = fixture();
        let mut creds = Credentials::new();
        creds.insert("access_token".into(), "t".into());
        f.backend
            .handle(Command::ConnectShop {
                url: "myshop.test/".into(),
                kind: ConnectorKind::Shop(Platform::Shopify),
                credentials: creds,
            })
            .await;
        assert_eq!(f.backend.tools().len(), 8);
        let events = f.events.lock().unwrap();
        assert!(matches!(&events[0], BackendEvent::Notice(n) if n.contains("not implemented")));
        assert!(
            matches!(&events[1], BackendEvent::ShopConnected { url, .. } if url == "https://myshop.test")
        );
    }

    #[tokio::test]
    async fn new_conversation_clears_history() {
        let mut f = fixture();
        f.provider.push_text("x");
        f.backend.handle(Command::SetLlm(llm_cfg())).await;
        f.backend
            .handle(Command::Send {
                text: "hi".into(),
                cancel: CancellationToken::new(),
            })
            .await;
        f.backend.handle(Command::NewConversation).await;
        assert!(f.backend.history().is_empty());
    }

    #[test]
    fn shop_urls_are_normalised() {
        assert_eq!(
            normalize_shop_url(" shop.com/ ").unwrap(),
            "https://shop.com"
        );
        assert_eq!(
            normalize_shop_url("http://localhost:8080/store/").unwrap(),
            "http://localhost:8080/store"
        );
        assert!(normalize_shop_url("").is_err());
        assert!(normalize_shop_url("ftp://x").is_err());
    }
}
