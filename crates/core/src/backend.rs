//! Owns the session state (LLM provider, active shop, tools, conversation)
//! and executes UI commands one at a time, persisting everything to the store.
//! Cancellation and confirmations bypass the command queue: the UI holds the
//! turn's `CancellationToken` and answers cards through their `Responder`.

use std::sync::{Arc, Mutex};

use connectors::{
    Connector, ConnectorError, ConnectorKind, ConnectorRegistry, Credentials, HttpTransport,
};
use llm::{ChatMessage, LlmError, LlmProvider, ProviderConfig, ProviderId, Usage};
use skills::{ScriptLimits, Skill};
use store::Store;
use tools::{CancellationToken, Tool, ToolContext, ToolRegistry};

use crate::agent::{Agent, AgentConfig, AgentEvent, AuditEntry, AuditSink, AuditStage, UiGate};
use crate::prompt::{system_prompt, PromptContext};

#[derive(Debug)]
pub enum Command {
    /// Restores the active LLM, the most recent shop and its last conversation.
    Init,
    SetLlm(ProviderConfig),
    ConnectShop {
        url: String,
        kind: ConnectorKind,
        credentials: Credentials,
    },
    SelectShop(i64),
    RemoveShop(i64),
    Send {
        text: String,
        cancel: CancellationToken,
    },
    NewConversation,
    OpenConversation(i64),
    RenameConversation {
        id: i64,
        title: String,
    },
    DeleteConversation(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShopSummary {
    pub id: i64,
    pub url: String,
    pub name: Option<String>,
    pub kind: Option<ConnectorKind>,
}

impl ShopSummary {
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            self.url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_string()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: i64,
    pub title: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmSettingSummary {
    pub provider: ProviderId,
    pub model: String,
    pub base_url: Option<String>,
    pub active: bool,
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
    LlmConfigured {
        provider: ProviderId,
        model: String,
    },
    /// Saved provider settings, without API keys, to pre-fill the form.
    LlmSettings(Vec<LlmSettingSummary>),
    /// Skills available as `/name` commands.
    Skills(Vec<SkillSummary>),
    Shops {
        shops: Vec<ShopSummary>,
        active: Option<i64>,
    },
    Conversations {
        items: Vec<ConversationSummary>,
        active: Option<i64>,
    },
    /// Replace the transcript. `id` is `None` for a fresh, unsaved chat.
    ConversationOpened {
        id: Option<i64>,
        messages: Vec<ChatMessage>,
        usage: Usage,
    },
}

pub type BackendSink = Arc<dyn Fn(BackendEvent) + Send + Sync>;
pub type ProviderFactory =
    Arc<dyn Fn(&ProviderConfig) -> Result<Arc<dyn LlmProvider>, LlmError> + Send + Sync>;

struct ActiveShop {
    id: i64,
    url: String,
    name: Option<String>,
    connector: Option<Arc<dyn Connector>>,
}

pub struct Backend {
    sink: BackendSink,
    provider_factory: ProviderFactory,
    transport: Arc<dyn HttpTransport>,
    store: Store,
    agent_defaults: AgentConfig,
    llm: Option<(ProviderConfig, Arc<dyn LlmProvider>)>,
    shop: Option<ActiveShop>,
    tools: ToolRegistry,
    conversation: Option<i64>,
    history: Vec<ChatMessage>,
    skills: Arc<Vec<Skill>>,
    skill_errors: Vec<String>,
}

/// Notes beyond this many are left out of the system prompt (newest kept).
const MAX_PROMPT_NOTES: usize = 50;

/// Writes mutation audit entries for the current shop and conversation.
struct StoreAudit {
    store: Store,
    shop_id: Option<i64>,
    conversation_id: Option<i64>,
}

impl AuditSink for StoreAudit {
    fn record(&self, entry: AuditEntry) {
        let (stage, detail) = match entry.stage {
            AuditStage::Denied(reason) => ("denied", reason),
            AuditStage::Approved => ("approved", None),
            AuditStage::Succeeded(summary) => ("succeeded", Some(summary)),
            AuditStage::Failed(error) => ("failed", Some(error)),
        };
        // An audit failure must not abort the merchant's action; it is logged
        // to stderr instead because the store is the only other sink.
        if let Err(e) = self.store.record_audit(&store::NewAudit {
            shop_id: self.shop_id,
            conversation_id: self.conversation_id,
            tool: entry.tool,
            arguments: entry.arguments.to_string(),
            stage: stage.into(),
            detail,
        }) {
            eprintln!("audit write failed: {e}");
        }
    }
}

type Res<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn conversation_title(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let mut title: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        title.push('…');
    }
    if title.is_empty() {
        "New chat".into()
    } else {
        title
    }
}

impl Backend {
    pub fn new(
        sink: BackendSink,
        provider_factory: ProviderFactory,
        transport: Arc<dyn HttpTransport>,
        store: Store,
    ) -> Self {
        let mut backend = Self {
            sink,
            provider_factory,
            transport,
            store,
            agent_defaults: AgentConfig::new(""),
            llm: None,
            shop: None,
            tools: ToolRegistry::new(),
            conversation: None,
            history: Vec::new(),
            skills: Arc::new(Vec::new()),
            skill_errors: Vec::new(),
        };
        backend.rebuild_tools();
        backend
    }

    pub fn with_skills(mut self, skills: Vec<Skill>, errors: Vec<String>) -> Self {
        self.set_skills(skills, errors);
        self
    }

    pub fn set_skills(&mut self, skills: Vec<Skill>, errors: Vec<String>) {
        self.skills = Arc::new(skills);
        self.skill_errors = errors;
        self.rebuild_tools();
    }

    pub fn with_agent_defaults(mut self, cfg: AgentConfig) -> Self {
        self.agent_defaults = cfg;
        self
    }

    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    pub fn active_shop_id(&self) -> Option<i64> {
        self.shop.as_ref().map(|s| s.id)
    }

    pub fn conversation_id(&self) -> Option<i64> {
        self.conversation
    }

    fn emit(&self, e: BackendEvent) {
        (self.sink)(e)
    }

    pub async fn handle(&mut self, cmd: Command) {
        let result = match cmd {
            Command::Init => self.init(),
            Command::SetLlm(cfg) => self
                .with_saved_key(cfg)
                .and_then(|cfg| self.set_llm(cfg, true)),
            Command::ConnectShop {
                url,
                kind,
                credentials,
            } => self.connect_shop(&url, kind, &credentials).await,
            Command::SelectShop(id) => self.select_shop(id),
            Command::RemoveShop(id) => self.remove_shop(id),
            Command::Send { text, cancel } => {
                let error = self.send(text, cancel).await.err();
                self.emit(BackendEvent::TurnEnded { error });
                Ok(())
            }
            Command::NewConversation => {
                self.new_conversation();
                self.emit_conversations()
            }
            Command::OpenConversation(id) => self.open_conversation(id),
            Command::RenameConversation { id, title } => {
                let title = title.trim();
                if title.is_empty() {
                    Ok(())
                } else {
                    self.store
                        .rename_conversation(id, title)
                        .map_err(err)
                        .and_then(|_| self.emit_conversations())
                }
            }
            Command::DeleteConversation(id) => self.delete_conversation(id),
        };
        if let Err(e) = result {
            self.emit(BackendEvent::Error(e));
        }
    }

    fn init(&mut self) -> Res<()> {
        self.emit(BackendEvent::Skills(
            self.skills
                .iter()
                .map(|s| SkillSummary {
                    name: s.name.clone(),
                    description: s.description.clone(),
                })
                .collect(),
        ));
        for e in &self.skill_errors {
            self.emit(BackendEvent::Notice(format!("Skill skipped: {e}")));
        }
        if let Some(row) = self.store.active_llm().map_err(err)? {
            match ProviderId::parse(&row.provider) {
                Some(provider) => {
                    let cfg = ProviderConfig {
                        provider,
                        api_key: row.api_key,
                        base_url: row.base_url,
                        model: row.model,
                    };
                    if let Err(e) = self.set_llm(cfg, false) {
                        self.emit(BackendEvent::Error(e));
                    }
                }
                None => self.emit(BackendEvent::Notice(format!(
                    "Ignoring saved settings for unknown provider {:?}.",
                    row.provider
                ))),
            }
        }
        self.emit_llm_settings()?;
        match self.store.list_shops().map_err(err)?.first() {
            Some(shop) => self.select_shop(shop.id),
            None => {
                self.emit_shops()?;
                self.open_latest_conversation()
            }
        }
    }

    fn set_llm(&mut self, cfg: ProviderConfig, persist: bool) -> Res<()> {
        let provider = (self.provider_factory)(&cfg).map_err(err)?;
        if persist {
            self.store
                .save_llm_settings(&store::LlmSettingsRow {
                    provider: cfg.provider.id().into(),
                    api_key: cfg.api_key.clone(),
                    base_url: cfg.base_url.clone(),
                    model: cfg.model.clone(),
                    active: true,
                })
                .map_err(err)?;
            self.emit_llm_settings()?;
        }
        self.emit(BackendEvent::LlmConfigured {
            provider: cfg.provider,
            model: cfg.model.clone(),
        });
        self.llm = Some((cfg, provider));
        Ok(())
    }

    /// A blank key in the form means "keep the saved one for this provider".
    fn with_saved_key(&self, mut cfg: ProviderConfig) -> Res<ProviderConfig> {
        if cfg.api_key.trim().is_empty() {
            cfg.api_key = self
                .store
                .llm_settings()
                .map_err(err)?
                .into_iter()
                .find(|r| r.provider == cfg.provider.id())
                .map(|r| r.api_key)
                .ok_or_else(|| format!("Enter an API key for {}.", cfg.provider.display_name()))?;
        }
        Ok(cfg)
    }

    fn emit_llm_settings(&self) -> Res<()> {
        let rows = self.store.llm_settings().map_err(err)?;
        let summaries = rows
            .into_iter()
            .filter_map(|r| {
                Some(LlmSettingSummary {
                    provider: ProviderId::parse(&r.provider)?,
                    model: r.model,
                    base_url: r.base_url,
                    active: r.active,
                })
            })
            .collect();
        self.emit(BackendEvent::LlmSettings(summaries));
        Ok(())
    }

    async fn connect_shop(
        &mut self,
        url: &str,
        kind: ConnectorKind,
        creds: &Credentials,
    ) -> Res<()> {
        let url = normalize_shop_url(url)?;
        let connector =
            ConnectorRegistry::build(kind, &url, creds, self.transport.clone()).map_err(err)?;
        match connector.verify().await {
            Ok(()) => {}
            Err(ConnectorError::NotImplemented(what)) => self.emit(BackendEvent::Notice(format!(
                "Connected, but {what} is not implemented yet, so shop tools will report errors."
            ))),
            Err(e) => return Err(format!("Could not connect to {url}: {e}")),
        }
        let row = self.store.upsert_shop(&url, None).map_err(err)?;
        self.store
            .save_credentials(row.id, &kind.id(), creds)
            .map_err(err)?;
        self.emit(BackendEvent::Notice(format!(
            "Connected {} at {url}.",
            kind.display_name()
        )));
        self.select_shop(row.id)
    }

    fn shop_kind(&self, shop_id: i64) -> Res<Option<(ConnectorKind, Credentials)>> {
        Ok(self
            .store
            .credentials(shop_id)
            .map_err(err)?
            .into_iter()
            .find_map(|row| {
                let kind = ConnectorKind::parse(&row.kind)?;
                matches!(kind, ConnectorKind::Shop(_)).then_some((kind, row.credentials))
            }))
    }

    fn select_shop(&mut self, id: i64) -> Res<()> {
        let row = self
            .store
            .shop(id)
            .map_err(err)?
            .ok_or_else(|| format!("shop {id} no longer exists"))?;
        let connector = match self.shop_kind(id)? {
            Some((kind, creds)) => {
                match ConnectorRegistry::build(kind, &row.url, &creds, self.transport.clone()) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        self.emit(BackendEvent::Error(format!(
                            "Saved credentials for {} are unusable: {e}",
                            row.url
                        )));
                        None
                    }
                }
            }
            None => None,
        };
        self.store.touch_shop(id).map_err(err)?;
        self.shop = Some(ActiveShop {
            id,
            url: row.url,
            name: row.name,
            connector,
        });
        self.rebuild_tools();
        self.emit_shops()?;
        self.open_latest_conversation()
    }

    fn remove_shop(&mut self, id: i64) -> Res<()> {
        self.store.delete_shop(id).map_err(err)?;
        if self.active_shop_id() == Some(id) {
            self.shop = None;
            self.rebuild_tools();
            if let Some(next) = self.store.list_shops().map_err(err)?.first() {
                return self.select_shop(next.id);
            }
            self.emit_shops()?;
            return self.open_latest_conversation();
        }
        self.emit_shops()
    }

    fn rebuild_tools(&mut self) {
        let mut tools = ToolRegistry::new();
        if let Some(c) = self.shop.as_ref().and_then(|s| s.connector.as_ref()) {
            tools.extend(c.tools());
        }
        tools.extend(self.extra_tools());
        self.tools = tools;
    }

    fn extra_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools = skills::skill_tools(self.skills.clone(), ScriptLimits::default());
        tools.extend(crate::memory::memory_tools(
            self.store.clone(),
            self.active_shop_id(),
        ));
        tools
    }

    fn emit_shops(&self) -> Res<()> {
        let shops = self
            .store
            .list_shops()
            .map_err(err)?
            .into_iter()
            .map(|s| {
                let kind = self.shop_kind(s.id).ok().flatten().map(|(k, _)| k);
                ShopSummary {
                    id: s.id,
                    url: s.url,
                    name: s.name,
                    kind,
                }
            })
            .collect();
        self.emit(BackendEvent::Shops {
            shops,
            active: self.active_shop_id(),
        });
        Ok(())
    }

    fn emit_conversations(&self) -> Res<()> {
        let items = self
            .store
            .list_conversations(self.active_shop_id())
            .map_err(err)?
            .into_iter()
            .map(|c| ConversationSummary {
                id: c.id,
                title: c.title,
                updated_at: c.updated_at,
            })
            .collect();
        self.emit(BackendEvent::Conversations {
            items,
            active: self.conversation,
        });
        Ok(())
    }

    fn new_conversation(&mut self) {
        self.conversation = None;
        self.history.clear();
        self.emit(BackendEvent::ConversationOpened {
            id: None,
            messages: Vec::new(),
            usage: Usage::default(),
        });
    }

    fn open_latest_conversation(&mut self) -> Res<()> {
        match self
            .store
            .list_conversations(self.active_shop_id())
            .map_err(err)?
            .first()
        {
            Some(c) => self.open_conversation(c.id),
            None => {
                self.new_conversation();
                self.emit_conversations()
            }
        }
    }

    fn open_conversation(&mut self, id: i64) -> Res<()> {
        let row = self
            .store
            .conversation(id)
            .map_err(err)?
            .ok_or_else(|| format!("conversation {id} no longer exists"))?;
        if row.shop_id != self.active_shop_id() {
            return Err("that conversation belongs to another shop".into());
        }
        let mut skipped = 0;
        let messages: Vec<ChatMessage> = self
            .store
            .messages(id)
            .map_err(err)?
            .iter()
            .filter_map(|m| {
                let parsed = serde_json::from_str(m).ok();
                skipped += usize::from(parsed.is_none());
                parsed
            })
            .collect();
        if skipped > 0 {
            self.emit(BackendEvent::Notice(format!(
                "{skipped} unreadable message(s) were skipped."
            )));
        }
        self.conversation = Some(id);
        self.history = messages.clone();
        self.emit(BackendEvent::ConversationOpened {
            id: Some(id),
            messages,
            usage: Usage {
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
            },
        });
        self.emit_conversations()
    }

    fn delete_conversation(&mut self, id: i64) -> Res<()> {
        self.store.delete_conversation(id).map_err(err)?;
        if self.conversation == Some(id) {
            self.new_conversation();
        }
        self.emit_conversations()
    }

    fn prompt_context(&self) -> PromptContext {
        let notes = self
            .store
            .memory_notes(self.active_shop_id())
            .unwrap_or_default();
        let skip = notes.len().saturating_sub(MAX_PROMPT_NOTES);
        PromptContext {
            today: Some(chrono::Local::now().date_naive()),
            shop_name: self.shop.as_ref().and_then(|s| s.name.clone()),
            shop_url: self.shop.as_ref().map(|s| s.url.clone()),
            connectors: self
                .shop
                .iter()
                .filter_map(|s| s.connector.as_ref())
                .map(|c| c.kind().display_name())
                .collect(),
            skills: self
                .skills
                .iter()
                .map(|s| (s.name.clone(), s.description.clone()))
                .collect(),
            memory: notes.into_iter().skip(skip).map(|n| n.text).collect(),
        }
    }

    async fn send(&mut self, text: String, cancel: CancellationToken) -> Res<()> {
        let Some((cfg, provider)) = self.llm.clone() else {
            return Err("Configure an AI provider in Settings first.".into());
        };
        let conversation = match self.conversation {
            Some(id) => id,
            None => {
                let id = self
                    .store
                    .create_conversation(self.active_shop_id(), &conversation_title(&text))
                    .map_err(err)?;
                self.conversation = Some(id);
                id
            }
        };
        let first_new = self.history.len();
        let text = match skills::expand_command(&text, &self.skills) {
            Some(expanded) => format!("{}\n\n{expanded}", text.trim()),
            None => text,
        };
        self.history.push(ChatMessage::user(text));

        let usage = Arc::new(Mutex::new(Usage::default()));
        let agent_sink: crate::agent::EventSink = {
            let sink = self.sink.clone();
            let usage = usage.clone();
            Arc::new(move |e| {
                if let AgentEvent::Usage(u) = &e {
                    let mut total = usage.lock().unwrap();
                    total.input_tokens += u.input_tokens;
                    total.output_tokens += u.output_tokens;
                }
                sink(BackendEvent::Agent(e))
            })
        };
        let config = AgentConfig {
            model: cfg.model.clone(),
            ..self.agent_defaults.clone()
        };
        let gate = UiGate::new(agent_sink.clone());
        let audit = StoreAudit {
            store: self.store.clone(),
            shop_id: self.active_shop_id(),
            conversation_id: Some(conversation),
        };
        let ctx = ToolContext {
            shop_id: self.active_shop_id().map(|id| id.to_string()),
            unattended: false,
            cancel,
        };
        let prompt = system_prompt(&self.prompt_context());
        let agent = Agent {
            provider: provider.as_ref(),
            tools: &self.tools,
            gate: &gate,
            audit: &audit,
            sink: agent_sink,
            config: &config,
        };
        let outcome = agent.run_turn(&prompt, &mut self.history, &ctx).await;

        let new: Vec<String> = self.history[first_new..]
            .iter()
            .filter_map(|m| serde_json::to_string(m).ok())
            .collect();
        let persisted = self
            .store
            .append_messages(conversation, &new)
            .and_then(|_| {
                let u = *usage.lock().unwrap();
                self.store
                    .add_usage(conversation, u.input_tokens, u.output_tokens)
            })
            .map_err(err);
        self.emit_conversations()?;
        persisted?;
        outcome.map(|_| ()).map_err(err)
    }
}

/// Accepts what merchants paste ("shop.com", "https://shop.com/") and returns
/// a canonical base URL without a trailing slash, defaulting to https.
pub fn normalize_shop_url(input: &str) -> Res<String> {
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
    use serde_json::json;

    struct Fixture {
        backend: Backend,
        events: Arc<Mutex<Vec<BackendEvent>>>,
        provider: Arc<FakeProvider>,
        store: Store,
    }

    fn fixture_with(store: Store) -> Fixture {
        let events = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider::new());
        let ev = events.clone();
        let p = provider.clone();
        let backend = Backend::new(
            Arc::new(move |e| {
                if let BackendEvent::Agent(AgentEvent::ConfirmationRequested {
                    responder, ..
                }) = &e
                {
                    responder.respond(crate::Decision::Approve);
                }
                ev.lock().unwrap().push(e)
            }),
            Arc::new(move |_| Ok(p.clone() as Arc<dyn LlmProvider>)),
            Arc::new(FakeTransport::new()),
            store.clone(),
        );
        Fixture {
            backend,
            events,
            provider,
            store,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(Store::open_in_memory().unwrap())
    }

    impl Fixture {
        fn events(&self) -> Vec<BackendEvent> {
            self.events.lock().unwrap().clone()
        }

        async fn send(&mut self, text: &str) {
            self.backend
                .handle(Command::Send {
                    text: text.into(),
                    cancel: CancellationToken::new(),
                })
                .await;
        }

        async fn connect(&mut self, url: &str, platform: Platform) {
            let key = ConnectorRegistry::credential_schema(ConnectorKind::Shop(platform))
                .fields()
                .iter()
                .map(|f| (f.key.to_string(), "x".to_string()))
                .collect();
            self.backend
                .handle(Command::ConnectShop {
                    url: url.into(),
                    kind: ConnectorKind::Shop(platform),
                    credentials: key,
                })
                .await;
        }

        async fn configure(&mut self) {
            self.backend.handle(Command::SetLlm(llm_cfg())).await;
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
        f.send("hi").await;
        assert!(matches!(
            f.events().last(),
            Some(BackendEvent::TurnEnded { error: Some(e) }) if e.contains("Settings")
        ));
        assert!(f.backend.history().is_empty());
    }

    #[tokio::test]
    async fn send_streams_reply_and_persists_conversation() {
        let mut f = fixture();
        f.provider.push_text("Hello merchant");
        f.configure().await;
        f.send("hi there").await;
        assert_eq!(f.backend.history().len(), 2);
        let events = f.events();
        assert!(events.iter().any(
            |e| matches!(e, BackendEvent::Agent(AgentEvent::TextDelta(t)) if t == "Hello merchant")
        ));
        assert!(matches!(
            events.last(),
            Some(BackendEvent::TurnEnded { error: None })
        ));
        let id = f.backend.conversation_id().unwrap();
        assert_eq!(f.store.messages(id).unwrap().len(), 2);
        assert_eq!(f.store.conversation(id).unwrap().unwrap().title, "hi there");
    }

    #[tokio::test]
    async fn conversation_can_be_reopened_after_new_chat() {
        let mut f = fixture();
        f.provider.push_text("one");
        f.configure().await;
        f.send("first").await;
        let id = f.backend.conversation_id().unwrap();
        f.backend.handle(Command::NewConversation).await;
        assert!(f.backend.history().is_empty());
        f.backend.handle(Command::OpenConversation(id)).await;
        assert_eq!(f.backend.history().len(), 2);
        assert!(f.events().iter().any(|e| matches!(
            e,
            BackendEvent::ConversationOpened { id: Some(i), messages, .. } if *i == id && messages.len() == 2
        )));
    }

    #[tokio::test]
    async fn init_restores_llm_shop_and_last_conversation() {
        let store = Store::open_in_memory().unwrap();
        let mut first = fixture_with(store.clone());
        first.provider.push_text("hey");
        first.configure().await;
        first.connect("shop.test", Platform::Shopify).await;
        first.send("hello").await;

        let mut second = fixture_with(store);
        second.backend.handle(Command::Init).await;
        let events = second.events();
        assert!(events
            .iter()
            .any(|e| matches!(e, BackendEvent::LlmConfigured { model, .. } if model == "m")));
        assert!(events.iter().any(|e| matches!(
            e,
            BackendEvent::Shops { shops, active: Some(_) } if shops.len() == 1
                && shops[0].kind == Some(ConnectorKind::Shop(Platform::Shopify))
        )));
        assert_eq!(second.backend.history().len(), 2);
        assert_eq!(second.backend.tools().len(), 8 + 3, "shop + memory tools");
    }

    #[tokio::test]
    async fn blank_key_reuses_the_saved_one() {
        let mut f = fixture();
        f.configure().await;
        let mut cfg = llm_cfg();
        cfg.api_key = String::new();
        cfg.model = "m2".into();
        f.backend.handle(Command::SetLlm(cfg)).await;
        let row = f.store.active_llm().unwrap().unwrap();
        assert_eq!((row.api_key.as_str(), row.model.as_str()), ("k", "m2"));

        let mut other = llm_cfg();
        other.provider = ProviderId::DeepSeek;
        other.api_key = String::new();
        f.backend.handle(Command::SetLlm(other)).await;
        assert!(matches!(f.events().last(), Some(BackendEvent::Error(e)) if e.contains("API key")));
    }

    #[tokio::test]
    async fn llm_settings_are_persisted_without_exposing_keys_in_events() {
        let mut f = fixture();
        f.configure().await;
        assert_eq!(f.store.active_llm().unwrap().unwrap().api_key, "k");
        let summaries = f.events().into_iter().find_map(|e| match e {
            BackendEvent::LlmSettings(s) => Some(s),
            _ => None,
        });
        assert_eq!(summaries.unwrap()[0].model, "m");
    }

    #[tokio::test]
    async fn audit_row_written_for_mutation() {
        let mut f = fixture();
        f.configure().await;
        f.connect("shop.test", Platform::Magento).await;
        f.provider
            .push_tool_call(
                "c1",
                "shop_update_product",
                json!({"id": "7", "regular_price": "9.90"}),
            )
            .push_text("Tried");
        f.send("set price").await;
        let shop = f.backend.active_shop_id();
        let mut log = f.store.audit_log(shop, 10).unwrap();
        log.reverse();
        let stages: Vec<_> = log.iter().map(|a| a.stage.as_str()).collect();
        assert_eq!(stages, ["approved", "failed"]);
        assert_eq!(log[0].tool, "shop_update_product");
        assert_eq!(log[0].arguments, r#"{"id":"7","regular_price":"9.90"}"#);
        assert_eq!(log[0].conversation_id, f.backend.conversation_id());
        assert!(log[1]
            .detail
            .as_deref()
            .unwrap()
            .contains("not implemented"));
    }

    #[tokio::test]
    async fn conversations_are_scoped_to_shop() {
        let mut f = fixture();
        f.configure().await;
        f.provider.push_text("a").push_text("b");
        f.connect("a.test", Platform::Shopify).await;
        let shop_a = f.backend.active_shop_id().unwrap();
        f.send("about A").await;
        let conv_a = f.backend.conversation_id().unwrap();
        f.connect("b.test", Platform::Magento).await;
        assert_eq!(f.backend.conversation_id(), None, "new shop starts fresh");
        f.send("about B").await;

        assert_eq!(f.store.list_conversations(Some(shop_a)).unwrap().len(), 1);
        f.backend.handle(Command::SelectShop(shop_a)).await;
        assert_eq!(f.backend.conversation_id(), Some(conv_a));
        let b_conv = f.store.list_conversations(Some(shop_a + 1)).unwrap()[0].id;
        f.events.lock().unwrap().clear();
        f.backend.handle(Command::OpenConversation(b_conv)).await;
        assert!(
            matches!(f.events().last(), Some(BackendEvent::Error(e)) if e.contains("another shop"))
        );
    }

    #[tokio::test]
    async fn switching_shop_rebuilds_tool_registry_and_prompt() {
        let mut f = fixture();
        f.configure().await;
        f.connect("a.test", Platform::Shopify).await;
        let shop_a = f.backend.active_shop_id().unwrap();
        f.connect("b.test", Platform::Magento).await;
        f.backend.handle(Command::SelectShop(shop_a)).await;
        f.provider
            .push_tool_call("c", "shop_list_orders", json!({}))
            .push_text("ok");
        f.send("orders").await;
        let req = &f.provider.requests()[0];
        assert!(
            matches!(&req.messages[0], ChatMessage::System { text } if text.contains("https://a.test"))
        );
        let result = f
            .backend
            .history()
            .iter()
            .find_map(|m| match m {
                ChatMessage::ToolResults { results } => Some(results[0].content.clone()),
                _ => None,
            })
            .unwrap();
        assert!(result.contains("Shopify"), "{result}");
    }

    #[tokio::test]
    async fn removing_active_shop_falls_back_to_next() {
        let mut f = fixture();
        f.connect("a.test", Platform::Shopify).await;
        let a = f.backend.active_shop_id().unwrap();
        f.connect("b.test", Platform::Shopify).await;
        let b = f.backend.active_shop_id().unwrap();
        f.backend.handle(Command::RemoveShop(b)).await;
        assert_eq!(f.backend.active_shop_id(), Some(a));
        f.backend.handle(Command::RemoveShop(a)).await;
        assert_eq!(f.backend.active_shop_id(), None);
        assert!(f.backend.tools().get("shop_list_orders").is_none());
    }

    #[tokio::test]
    async fn rename_and_delete_conversation() {
        let mut f = fixture();
        f.provider.push_text("x");
        f.configure().await;
        f.send("hi").await;
        let id = f.backend.conversation_id().unwrap();
        f.backend
            .handle(Command::RenameConversation {
                id,
                title: "Renamed".into(),
            })
            .await;
        assert_eq!(f.store.conversation(id).unwrap().unwrap().title, "Renamed");
        f.backend.handle(Command::DeleteConversation(id)).await;
        assert_eq!(f.backend.conversation_id(), None);
        assert!(f.backend.history().is_empty());
    }

    #[tokio::test]
    async fn memory_notes_appear_in_system_prompt() {
        let mut f = fixture();
        f.configure().await;
        f.connect("a.test", Platform::Shopify).await;
        f.provider
            .push_tool_call("m", "memory_save", json!({"text": "Ships on Mondays"}))
            .push_text("Noted")
            .push_text("Sure");
        f.send("remember we ship on Mondays").await;
        f.send("when do we ship?").await;
        let last = f.provider.requests().pop().unwrap();
        let ChatMessage::System { text } = &last.messages[0] else {
            panic!();
        };
        assert!(text.contains("- Ships on Mondays"), "{text}");
        let audit = f.store.audit_log(f.backend.active_shop_id(), 10).unwrap();
        assert!(audit
            .iter()
            .any(|a| a.tool == "memory_save" && a.stage == "approved"));
    }

    #[tokio::test]
    async fn skills_are_listed_and_slash_commands_expand() {
        let skill = Skill {
            name: "daily-sales".into(),
            description: "Morning briefing".into(),
            body: "Call shop_sales_report for yesterday.".into(),
            dir: Default::default(),
            scripts: vec![],
        };
        let mut f = fixture();
        f.backend
            .set_skills(vec![skill], vec!["bad/SKILL.md: broken".into()]);
        f.backend.handle(Command::Init).await;
        assert!(f
            .events()
            .iter()
            .any(|e| matches!(e, BackendEvent::Skills(s) if s[0].name == "daily-sales")));
        assert!(f
            .events()
            .iter()
            .any(|e| matches!(e, BackendEvent::Notice(n) if n.contains("broken"))));
        assert!(f.backend.tools().get("load_skill").is_some());
        assert!(
            f.backend.tools().get("run_skill_script").is_none(),
            "no scripts, no runner"
        );

        f.provider.push_text("Briefing");
        f.configure().await;
        f.send("/daily-sales please").await;
        let req = &f.provider.requests()[0];
        let ChatMessage::System { text } = &req.messages[0] else {
            panic!();
        };
        assert!(text.contains("- daily-sales: Morning briefing"));
        let ChatMessage::User { text } = &req.messages[1] else {
            panic!();
        };
        assert!(text.starts_with("/daily-sales please"));
        assert!(text.contains("Call shop_sales_report for yesterday."));
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

    #[test]
    fn titles_are_first_line_truncated() {
        assert_eq!(conversation_title("Hello\nworld"), "Hello");
        assert_eq!(conversation_title("  "), "New chat");
        assert_eq!(conversation_title(&"a".repeat(70)).chars().count(), 61);
    }
}
