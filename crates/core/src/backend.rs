//! Owns the session state (LLM provider, active shop, tools, conversation)
//! and executes UI commands one at a time, persisting everything to the store.
//! Cancellation and confirmations bypass the command queue: the UI holds the
//! turn's `CancellationToken` and answers cards through their `Responder`.

use std::sync::{Arc, Mutex};

use connectors::google_ads::{AdsAccount, AdsAuth, AdsClient};
use connectors::{
    Connector, ConnectorError, ConnectorKind, ConnectorRegistry, Credentials, HttpTransport,
};
use llm::{ChatMessage, LlmError, LlmProvider, ProviderConfig, ProviderId, Usage};
use skills::{ScriptLimits, Skill};
use store::Store;
use tools::{CancellationToken, Tool, ToolContext, ToolRegistry};

use chrono::NaiveDateTime;

use crate::agent::{
    Agent, AgentConfig, AgentEvent, AuditEntry, AuditSink, AuditStage, AutoDeny, UiGate,
};
use crate::prompt::{system_prompt, PromptContext};
use crate::redact::Redactor;
use crate::scheduler::Schedule;

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
    /// Runs every enabled task whose schedule is due now (sent by a timer).
    RunDueTasks,
    CreateTask {
        name: String,
        prompt: String,
        schedule: String,
    },
    SetTaskEnabled {
        id: i64,
        enabled: bool,
    },
    DeleteTask(i64),
    RunTaskNow(i64),
    /// Loads the active shop's audit log (newest first).
    LoadAudit,
    /// Saves the app's Google OAuth client and developer token; blank secrets
    /// keep the saved ones.
    SaveGoogleAdsApp {
        client_id: String,
        client_secret: String,
        developer_token: String,
    },
    /// Opens Google's consent page and waits for the merchant to sign in.
    GoogleSignIn,
    CancelGoogleSignIn,
    /// Posted by the sign-in task: the refresh token, or why it failed.
    GoogleSignInFinished(Result<Secret, String>),
    /// Links the chosen Google Ads account to the active shop.
    ConnectGoogleAds {
        customer_id: String,
    },
    /// Unlinks a non-shop connector (e.g. Google Ads) from the active shop.
    DisconnectConnector(ConnectorKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditSummary {
    pub created_at: String,
    pub tool: String,
    pub arguments: String,
    pub stage: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSummary {
    pub id: i64,
    pub name: String,
    pub prompt: String,
    pub schedule: String,
    pub enabled: bool,
    pub last_run_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShopSummary {
    pub id: i64,
    pub url: String,
    pub name: Option<String>,
    pub kind: Option<ConnectorKind>,
    /// Every connector linked to the shop, the shop platform included.
    pub connectors: Vec<ConnectorKind>,
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
    /// Non-secret provider options (e.g. Bedrock region, AWS key id).
    pub options: std::collections::BTreeMap<String, String>,
    /// Whether secret options (e.g. an AWS session token) are saved.
    pub has_secret_options: bool,
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
    /// Scheduled tasks of the active shop.
    Tasks(Vec<TaskSummary>),
    TaskFinished {
        name: String,
        error: Option<String>,
    },
    Audit(Vec<AuditSummary>),
    /// Whether the Google Ads app credentials are set up (secrets never sent).
    GoogleAdsApp {
        configured: bool,
        client_id: String,
    },
    GoogleSignIn(GoogleSignInState),
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
    /// The shop platform plus any linked services (e.g. Google Ads).
    connectors: Vec<Arc<dyn Connector>>,
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
    /// Lets background work (Google sign-in) post its result into the queue.
    commands: Option<tokio::sync::mpsc::UnboundedSender<Command>>,
    open_browser: Arc<dyn Fn(&str) + Send + Sync>,
    google_sign_in: Option<CancellationToken>,
    google_pending: Option<GooglePending>,
}

/// A finished Google sign-in waiting for the merchant to pick an account.
struct GooglePending {
    app: GoogleApp,
    refresh_token: String,
    accounts: Vec<AdsAccount>,
}

/// The app's Google OAuth client and developer token, set up once.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct GoogleApp {
    client_id: String,
    client_secret: String,
    developer_token: String,
}

const GOOGLE_APP_KEY: &str = "google_ads_app";
const GOOGLE_SIGN_IN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// A secret carried through a `Command`; its Debug output never shows the value.
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GoogleSignInState {
    #[default]
    Idle,
    /// The consent page is open in the browser.
    Waiting,
    Accounts(Vec<AdsAccount>),
    Failed(String),
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
            commands: None,
            open_browser: Arc::new(|_| {}),
            google_sign_in: None,
            google_pending: None,
        };
        backend.rebuild_tools();
        backend
    }

    pub fn with_command_sender(mut self, tx: tokio::sync::mpsc::UnboundedSender<Command>) -> Self {
        self.commands = Some(tx);
        self
    }

    /// How to show a URL to the merchant (the system browser in the app).
    pub fn with_browser_opener(mut self, open: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.open_browser = open;
        self
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

    /// Every event leaves through here, so error text is scrubbed of secrets.
    fn emit(&self, e: BackendEvent) {
        let e = match e {
            BackendEvent::Error(m) => BackendEvent::Error(self.redactor().apply(&m)),
            BackendEvent::TurnEnded { error } => BackendEvent::TurnEnded {
                error: error.map(|m| self.redactor().apply(&m)),
            },
            BackendEvent::TaskFinished { name, error } => BackendEvent::TaskFinished {
                name,
                error: error.map(|m| self.redactor().apply(&m)),
            },
            other => other,
        };
        (self.sink)(e)
    }

    /// The LLM key plus every secret credential field of every saved shop.
    fn redactor(&self) -> Redactor {
        let secret_options = |opts: &std::collections::BTreeMap<String, String>| {
            opts.iter()
                .filter(|(k, _)| llm::is_secret_option(k))
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>()
        };
        let mut secrets: Vec<String> = Vec::new();
        if let Some((cfg, _)) = &self.llm {
            secrets.push(cfg.api_key.clone());
            secrets.extend(secret_options(&cfg.options));
        }
        for r in self.store.llm_settings().unwrap_or_default() {
            secrets.extend(secret_options(&r.options));
            secrets.push(r.api_key);
        }
        if let Ok(Some(app)) = self.google_app() {
            secrets.extend([app.client_secret, app.developer_token]);
        }
        if let Some(p) = &self.google_pending {
            secrets.push(p.refresh_token.clone());
        }
        for shop in self.store.list_shops().unwrap_or_default() {
            for row in self.store.credentials(shop.id).unwrap_or_default() {
                let Some(kind) = ConnectorKind::parse(&row.kind) else {
                    continue;
                };
                for field in ConnectorRegistry::credential_schema(kind).fields() {
                    if field.secret {
                        secrets.extend(row.credentials.get(field.key).cloned());
                    }
                }
            }
        }
        Redactor::new(secrets)
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
            Command::RunDueTasks => {
                self.run_due_tasks_at(chrono::Local::now().naive_local())
                    .await;
                Ok(())
            }
            Command::CreateTask {
                name,
                prompt,
                schedule,
            } => self.create_task(&name, &prompt, &schedule),
            Command::SetTaskEnabled { id, enabled } => self
                .store
                .set_task_enabled(id, enabled)
                .map_err(err)
                .and_then(|_| self.emit_tasks()),
            Command::DeleteTask(id) => self
                .store
                .delete_task(id)
                .map_err(err)
                .and_then(|_| self.emit_tasks()),
            Command::LoadAudit => self.load_audit(),
            Command::SaveGoogleAdsApp {
                client_id,
                client_secret,
                developer_token,
            } => self.save_google_app(&client_id, &client_secret, &developer_token),
            Command::GoogleSignIn => self.start_google_sign_in(),
            Command::CancelGoogleSignIn => {
                if let Some(c) = self.google_sign_in.take() {
                    c.cancel();
                }
                self.emit(BackendEvent::GoogleSignIn(GoogleSignInState::Idle));
                Ok(())
            }
            Command::GoogleSignInFinished(result) => {
                self.finish_google_sign_in(result).await;
                Ok(())
            }
            Command::ConnectGoogleAds { customer_id } => {
                self.connect_google_ads(&customer_id).await
            }
            Command::DisconnectConnector(kind) => self.disconnect_connector(kind),
            Command::RunTaskNow(id) => {
                match self
                    .store
                    .list_tasks(None)
                    .map_err(err)
                    .map(|tasks| tasks.into_iter().find(|t| t.id == id))
                {
                    Ok(Some(task)) => {
                        self.run_task(task, chrono::Local::now().naive_local())
                            .await;
                        Ok(())
                    }
                    Ok(None) => Err(format!("task {id} no longer exists")),
                    Err(e) => Err(e),
                }
            }
        };
        if let Err(e) = result {
            self.emit(BackendEvent::Error(e));
        }
    }

    fn init(&mut self) -> Res<()> {
        self.emit_google_app()?;
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
                        options: row.options,
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
                    options: cfg.options.clone(),
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
    /// The form never shows saved secrets, so a blank secret field means
    /// "keep the saved one" (for options, only while the AWS key id is unchanged).
    fn with_saved_key(&self, mut cfg: ProviderConfig) -> Res<ProviderConfig> {
        // A secret belongs to its AWS key id; never pair it with another one.
        let saved = self
            .store
            .llm_settings()
            .map_err(err)?
            .into_iter()
            .find(|r| r.provider == cfg.provider.id())
            .filter(|r| {
                r.options.get(llm::OPT_AWS_ACCESS_KEY_ID)
                    == cfg.options.get(llm::OPT_AWS_ACCESS_KEY_ID)
            });
        if cfg.api_key.trim().is_empty() {
            cfg.api_key = saved
                .as_ref()
                .map(|r| r.api_key.clone())
                .ok_or_else(|| format!("Enter an API key for {}.", cfg.provider.display_name()))?;
        }
        if let Some(saved) = &saved {
            for (k, v) in &saved.options {
                let blank = cfg.options.get(k).is_none_or(|x| x.trim().is_empty());
                if llm::is_secret_option(k) && blank {
                    cfg.options.insert(k.clone(), v.clone());
                }
            }
        }
        cfg.options.retain(|_, v| !v.trim().is_empty());
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
                    has_secret_options: r.options.keys().any(|k| llm::is_secret_option(k)),
                    options: r
                        .options
                        .into_iter()
                        .filter(|(k, _)| !llm::is_secret_option(k))
                        .collect(),
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

    /// Every connector saved for the shop; unusable ones are reported and skipped.
    fn build_connectors(&self, shop_id: i64, url: &str) -> Res<Vec<Arc<dyn Connector>>> {
        let mut out = Vec::new();
        for row in self.store.credentials(shop_id).map_err(err)? {
            let Some(kind) = ConnectorKind::parse(&row.kind) else {
                continue;
            };
            match ConnectorRegistry::build(kind, url, &row.credentials, self.transport.clone()) {
                Ok(c) => out.push(c),
                Err(e) => self.emit(BackendEvent::Error(format!(
                    "Saved {} credentials for {url} are unusable: {e}",
                    kind.display_name()
                ))),
            }
        }
        Ok(out)
    }

    fn select_shop(&mut self, id: i64) -> Res<()> {
        let row = self
            .store
            .shop(id)
            .map_err(err)?
            .ok_or_else(|| format!("shop {id} no longer exists"))?;
        let connectors = self.build_connectors(id, &row.url)?;
        self.store.touch_shop(id).map_err(err)?;
        self.shop = Some(ActiveShop {
            id,
            url: row.url,
            name: row.name,
            connectors,
        });
        self.rebuild_tools();
        self.emit_shops()?;
        self.emit_tasks()?;
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
        let connectors = self
            .shop
            .as_ref()
            .map(|s| s.connectors.clone())
            .unwrap_or_default();
        self.tools = self.tools_for(self.active_shop_id(), &connectors);
    }

    fn tools_for(&self, shop_id: Option<i64>, connectors: &[Arc<dyn Connector>]) -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        for c in connectors {
            tools.extend(c.tools());
        }
        tools.extend(skills::skill_tools(
            self.skills.clone(),
            ScriptLimits::default(),
        ));
        let memory: Vec<Arc<dyn Tool>> = crate::memory::memory_tools(self.store.clone(), shop_id);
        tools.extend(memory);
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
                let connectors = self
                    .store
                    .credentials(s.id)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|r| ConnectorKind::parse(&r.kind))
                    .collect();
                ShopSummary {
                    id: s.id,
                    url: s.url,
                    name: s.name,
                    kind,
                    connectors,
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
        let shop = self.shop.as_ref();
        self.prompt_context_for(
            shop.map(|s| s.id),
            shop.map(|s| s.url.clone()),
            shop.and_then(|s| s.name.clone()),
            shop.map(|s| s.connectors.as_slice()).unwrap_or_default(),
        )
    }

    fn prompt_context_for(
        &self,
        shop_id: Option<i64>,
        url: Option<String>,
        name: Option<String>,
        connectors: &[Arc<dyn Connector>],
    ) -> PromptContext {
        let notes = self.store.memory_notes(shop_id).unwrap_or_default();
        let skip = notes.len().saturating_sub(MAX_PROMPT_NOTES);
        PromptContext {
            today: Some(chrono::Local::now().date_naive()),
            shop_name: name,
            shop_url: url,
            connectors: connectors.iter().map(|c| c.kind().display_name()).collect(),
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
            redactor: self.redactor(),
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

fn to_local(rfc3339: &str) -> Option<NaiveDateTime> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|t| t.with_timezone(&chrono::Local).naive_local())
}

fn local_to_rfc3339(t: NaiveDateTime) -> String {
    use chrono::TimeZone;
    chrono::Local
        .from_local_datetime(&t)
        .earliest()
        .map(|l| l.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| t.and_utc())
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

const UNATTENDED_NOTE: &str = "\n\nThis is a scheduled task running unattended; the merchant is not watching. You cannot change the shop: report what you find and what you would recommend.";

impl Backend {
    fn emit_tasks(&self) -> Res<()> {
        let tasks = match self.active_shop_id() {
            Some(id) => self.store.list_tasks(Some(id)).map_err(err)?,
            None => Vec::new(),
        };
        self.emit(BackendEvent::Tasks(
            tasks
                .into_iter()
                .map(|t| TaskSummary {
                    id: t.id,
                    name: t.name,
                    prompt: t.prompt,
                    schedule: t.schedule,
                    enabled: t.enabled,
                    last_run_at: t.last_run_at,
                })
                .collect(),
        ));
        Ok(())
    }

    fn load_audit(&self) -> Res<()> {
        let Some(shop) = self.active_shop_id() else {
            self.emit(BackendEvent::Audit(Vec::new()));
            return Ok(());
        };
        let rows = self.store.audit_log(Some(shop), 200).map_err(err)?;
        self.emit(BackendEvent::Audit(
            rows.into_iter()
                .map(|r| AuditSummary {
                    created_at: r.created_at,
                    tool: r.tool,
                    arguments: r.arguments,
                    stage: r.stage,
                    detail: r.detail,
                })
                .collect(),
        ));
        Ok(())
    }

    fn create_task(&mut self, name: &str, prompt: &str, schedule: &str) -> Res<()> {
        let shop_id = self
            .active_shop_id()
            .ok_or("Connect a shop before scheduling tasks.")?;
        let (name, prompt) = (name.trim(), prompt.trim());
        if name.is_empty() || prompt.is_empty() {
            return Err("A task needs a name and a prompt.".into());
        }
        let schedule: Schedule = schedule.parse()?;
        self.store
            .create_task(&store::NewTask {
                shop_id,
                name: name.into(),
                prompt: prompt.into(),
                schedule: schedule.to_string(),
            })
            .map_err(err)?;
        self.emit_tasks()
    }

    /// Runs every enabled task that is due at `now` (local time). A task
    /// missed while the app was closed runs once.
    pub async fn run_due_tasks_at(&mut self, now: NaiveDateTime) {
        if self.llm.is_none() {
            return;
        }
        let tasks = match self.store.list_tasks(None) {
            Ok(t) => t,
            Err(e) => return self.emit(BackendEvent::Error(e.to_string())),
        };
        for task in tasks.into_iter().filter(|t| t.enabled) {
            let Ok(schedule) = task.schedule.parse::<Schedule>() else {
                continue;
            };
            let anchor = task
                .last_run_at
                .as_deref()
                .and_then(to_local)
                .or_else(|| to_local(&task.created_at));
            if anchor.is_some_and(|a| schedule.is_due(a, now)) {
                self.run_task(task, now).await;
            }
        }
    }

    async fn run_task(&mut self, task: store::TaskRow, now: NaiveDateTime) {
        let name = task.name.clone();
        let error = self.run_task_inner(task, now).await.err();
        self.emit(BackendEvent::TaskFinished { name, error });
        let _ = self.emit_tasks();
        let _ = self.emit_conversations();
    }

    async fn run_task_inner(&mut self, task: store::TaskRow, now: NaiveDateTime) -> Res<()> {
        let Some((cfg, provider)) = self.llm.clone() else {
            return Err("Configure an AI provider in Settings first.".into());
        };
        let shop = self
            .store
            .shop(task.shop_id)
            .map_err(err)?
            .ok_or("the task's shop no longer exists")?;
        let connectors = self.build_connectors(shop.id, &shop.url)?;
        let tools = self.tools_for(Some(shop.id), &connectors);
        let conversation = match task.conversation_id {
            Some(id) if self.store.conversation(id).map_err(err)?.is_some() => id,
            _ => self
                .store
                .create_conversation(Some(shop.id), &format!("Task: {}", task.name))
                .map_err(err)?,
        };
        // Recorded before running so a failing task is not retried every minute.
        self.store
            .mark_task_run(task.id, &local_to_rfc3339(now), Some(conversation))
            .map_err(err)?;

        let text = match skills::expand_command(&task.prompt, &self.skills) {
            Some(expanded) => format!("{}\n\n{expanded}", task.prompt),
            None => task.prompt.clone(),
        };
        let mut history = vec![ChatMessage::user(format!(
            "[Scheduled task: {}] {text}",
            task.name
        ))];
        let usage = Arc::new(Mutex::new(Usage::default()));
        let sink: crate::agent::EventSink = {
            let usage = usage.clone();
            Arc::new(move |e| {
                if let AgentEvent::Usage(u) = e {
                    let mut total = usage.lock().unwrap();
                    total.input_tokens += u.input_tokens;
                    total.output_tokens += u.output_tokens;
                }
            })
        };
        let config = AgentConfig {
            model: cfg.model.clone(),
            ..self.agent_defaults.clone()
        };
        let audit = StoreAudit {
            store: self.store.clone(),
            shop_id: Some(shop.id),
            conversation_id: Some(conversation),
        };
        let ctx = ToolContext {
            shop_id: Some(shop.id.to_string()),
            unattended: true,
            cancel: CancellationToken::new(),
        };
        let prompt = format!(
            "{}{UNATTENDED_NOTE}",
            system_prompt(&self.prompt_context_for(
                Some(shop.id),
                Some(shop.url.clone()),
                shop.name.clone(),
                &connectors,
            ))
        );
        let agent = Agent {
            provider: provider.as_ref(),
            tools: &tools,
            gate: &AutoDeny,
            audit: &audit,
            sink,
            config: &config,
            redactor: self.redactor(),
        };
        let outcome = agent.run_turn(&prompt, &mut history, &ctx).await;
        let serialized: Vec<String> = history
            .iter()
            .filter_map(|m| serde_json::to_string(m).ok())
            .collect();
        self.store
            .append_messages(conversation, &serialized)
            .map_err(err)?;
        let u = *usage.lock().unwrap();
        self.store
            .add_usage(conversation, u.input_tokens, u.output_tokens)
            .map_err(err)?;
        if self.conversation == Some(conversation) {
            self.open_conversation(conversation)?;
        }
        outcome.map(|_| ()).map_err(err)
    }
}

impl Backend {
    fn google_app(&self) -> Res<Option<GoogleApp>> {
        self.store
            .app_setting(GOOGLE_APP_KEY)
            .map_err(err)?
            .map(|json| serde_json::from_str(&json).map_err(err))
            .transpose()
    }

    fn emit_google_app(&self) -> Res<()> {
        let app = self.google_app()?;
        self.emit(BackendEvent::GoogleAdsApp {
            configured: app.is_some(),
            client_id: app.map(|a| a.client_id).unwrap_or_default(),
        });
        Ok(())
    }

    fn save_google_app(
        &mut self,
        client_id: &str,
        client_secret: &str,
        developer_token: &str,
    ) -> Res<()> {
        let saved = self.google_app()?.unwrap_or_default();
        let keep = |new: &str, old: String| {
            if new.trim().is_empty() {
                old
            } else {
                new.trim().to_string()
            }
        };
        let app = GoogleApp {
            client_id: keep(client_id, saved.client_id),
            client_secret: keep(client_secret, saved.client_secret),
            developer_token: keep(developer_token, saved.developer_token),
        };
        if app.client_id.is_empty()
            || app.client_secret.is_empty()
            || app.developer_token.is_empty()
        {
            return Err("Enter the OAuth client ID, client secret and developer token.".into());
        }
        self.store
            .set_app_setting(GOOGLE_APP_KEY, &serde_json::to_string(&app).map_err(err)?)
            .map_err(err)?;
        self.emit_google_app()
    }

    fn start_google_sign_in(&mut self) -> Res<()> {
        if self.active_shop_id().is_none() {
            return Err("Connect a shop first: Google Ads is linked to a shop.".into());
        }
        let app = self
            .google_app()?
            .ok_or("Set up the Google Ads app credentials first.")?;
        let tx = self
            .commands
            .clone()
            .ok_or("Google sign-in is not available in this build.")?;
        if let Some(previous) = self.google_sign_in.take() {
            previous.cancel();
        }
        let cancel = CancellationToken::new();
        self.google_sign_in = Some(cancel.clone());
        self.google_pending = None;
        self.emit(BackendEvent::GoogleSignIn(GoogleSignInState::Waiting));
        let transport = self.transport.clone();
        let open = self.open_browser.clone();
        tokio::spawn(async move {
            let client = connectors::google_ads::OAuthClient {
                client_id: app.client_id,
                client_secret: app.client_secret,
            };
            let result = connectors::google_ads::oauth::authorize(
                transport.as_ref(),
                &Default::default(),
                &client,
                move |url: &str| open(url),
                cancel,
                GOOGLE_SIGN_IN_TIMEOUT,
            )
            .await
            .map_err(|e| e.to_string())
            .and_then(|t| {
                t.refresh_token
                    .map(Secret)
                    .ok_or_else(|| "no refresh token".into())
            });
            let _ = tx.send(Command::GoogleSignInFinished(result));
        });
        Ok(())
    }

    async fn finish_google_sign_in(&mut self, result: Result<Secret, String>) {
        self.google_sign_in = None;
        let state = match self.load_google_accounts(result).await {
            Ok(accounts) => GoogleSignInState::Accounts(accounts),
            Err(e) => GoogleSignInState::Failed(self.redactor().apply(&e)),
        };
        self.emit(BackendEvent::GoogleSignIn(state));
    }

    async fn load_google_accounts(
        &mut self,
        result: Result<Secret, String>,
    ) -> Res<Vec<AdsAccount>> {
        let refresh_token = result?.0;
        let app = self
            .google_app()?
            .ok_or("the Google Ads app setup was removed")?;
        let client = AdsClient::new(
            AdsAuth {
                client: connectors::google_ads::OAuthClient {
                    client_id: app.client_id.clone(),
                    client_secret: app.client_secret.clone(),
                },
                developer_token: app.developer_token.clone(),
                refresh_token: refresh_token.clone(),
            },
            self.transport.clone(),
        );
        let accounts = connectors::google_ads::list_accounts(&client)
            .await
            .map_err(err)?;
        if accounts.is_empty() {
            return Err("This Google user has no Google Ads accounts.".into());
        }
        self.google_pending = Some(GooglePending {
            app,
            refresh_token,
            accounts: accounts.clone(),
        });
        Ok(accounts)
    }

    async fn connect_google_ads(&mut self, customer_id: &str) -> Res<()> {
        use connectors::google_ads as g;
        let shop = self
            .shop
            .as_ref()
            .map(|s| (s.id, s.url.clone()))
            .ok_or("Connect a shop first.")?;
        let pending = self
            .google_pending
            .as_ref()
            .ok_or("Sign in with Google first.")?;
        let id = g::normalize_customer_id(customer_id).map_err(err)?;
        let account = pending
            .accounts
            .iter()
            .find(|a| a.customer_id == id)
            .ok_or("That account is not one this Google user can access.")?;
        if account.manager {
            return Err(
                "Manager accounts cannot hold campaigns; pick one of its client accounts.".into(),
            );
        }
        let mut creds = Credentials::from([
            (g::CLIENT_ID.to_string(), pending.app.client_id.clone()),
            (
                g::CLIENT_SECRET.to_string(),
                pending.app.client_secret.clone(),
            ),
            (
                g::DEVELOPER_TOKEN.to_string(),
                pending.app.developer_token.clone(),
            ),
            (g::REFRESH_TOKEN.to_string(), pending.refresh_token.clone()),
            (g::CUSTOMER_ID.to_string(), id.clone()),
        ]);
        if let Some(login) = &account.login_customer_id {
            creds.insert(g::LOGIN_CUSTOMER_ID.to_string(), login.clone());
        }
        let name = account.name.clone();
        let connector = ConnectorRegistry::build(
            ConnectorKind::GoogleAds,
            &shop.1,
            &creds,
            self.transport.clone(),
        )
        .map_err(err)?;
        connector.verify().await.map_err(err)?;
        self.store
            .save_credentials(shop.0, &ConnectorKind::GoogleAds.id(), &creds)
            .map_err(err)?;
        self.google_pending = None;
        self.emit(BackendEvent::GoogleSignIn(GoogleSignInState::Idle));
        self.emit(BackendEvent::Notice(format!(
            "Google Ads account {name} ({}) linked to this shop. Campaigns the assistant drafts are created paused.",
            g::format_customer_id(&id)
        )));
        self.select_shop(shop.0)
    }

    fn disconnect_connector(&mut self, kind: ConnectorKind) -> Res<()> {
        if kind.is_shop() {
            return Err("Remove the shop instead.".into());
        }
        let shop = self.active_shop_id().ok_or("No shop is selected.")?;
        self.store
            .delete_credentials(shop, &kind.id())
            .map_err(err)?;
        self.emit(BackendEvent::Notice(format!(
            "{} unlinked from this shop.",
            kind.display_name()
        )));
        self.select_shop(shop)
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
        transport: Arc<FakeTransport>,
    }

    fn fixture_with(store: Store) -> Fixture {
        let events = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider::new());
        let ev = events.clone();
        let p = provider.clone();
        let transport = Arc::new(FakeTransport::new());
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
            transport.clone(),
            store.clone(),
        );
        Fixture {
            backend,
            events,
            provider,
            store,
            transport,
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
            options: Default::default(),
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
    async fn bedrock_options_persist_and_blank_secrets_keep_saved_ones() {
        let mut f = fixture();
        let bedrock = |key: &str, akid: &str, token: &str| ProviderConfig {
            provider: ProviderId::Bedrock,
            api_key: key.into(),
            base_url: None,
            model: "anthropic.claude-opus-5-5".into(),
            options: [
                (llm::OPT_REGION, "eu-west-1"),
                (llm::OPT_AWS_ACCESS_KEY_ID, akid),
                (llm::OPT_AWS_SESSION_TOKEN, token),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        };
        f.backend
            .handle(Command::SetLlm(bedrock(
                "aws-secret",
                "AKIA1",
                "tok-123456",
            )))
            .await;
        f.backend
            .handle(Command::SetLlm(bedrock("", "AKIA1", "")))
            .await;
        let row = f.store.active_llm().unwrap().unwrap();
        assert_eq!(row.api_key, "aws-secret");
        assert_eq!(row.options[llm::OPT_AWS_SESSION_TOKEN], "tok-123456");
        assert_eq!(row.options[llm::OPT_REGION], "eu-west-1");

        f.backend
            .handle(Command::SetLlm(bedrock("", "AKIA2", "")))
            .await;
        assert!(
            matches!(f.events().last(), Some(BackendEvent::Error(e)) if e.contains("API key")),
            "the old secret is not paired with a new key id"
        );
        f.backend
            .handle(Command::SetLlm(bedrock("aws-secret-2", "AKIA2", "")))
            .await;
        let row = f.store.active_llm().unwrap().unwrap();
        assert!(
            !row.options.contains_key(llm::OPT_AWS_SESSION_TOKEN),
            "a token is not reused for a different key id"
        );

        let summary = f
            .events()
            .into_iter()
            .rev()
            .find_map(|e| match e {
                BackendEvent::LlmSettings(s) => Some(s),
                _ => None,
            })
            .unwrap();
        assert_eq!(summary[0].options[llm::OPT_AWS_ACCESS_KEY_ID], "AKIA2");
        assert!(!summary[0].options.contains_key(llm::OPT_AWS_SESSION_TOKEN));

        f.backend
            .handle(Command::SetLlm(bedrock(
                "aws-secret",
                "AKIA1",
                "tok-123456",
            )))
            .await;
        f.backend
            .emit(BackendEvent::Error("bad token tok-123456".into()));
        assert!(
            matches!(f.events().last(), Some(BackendEvent::Error(e)) if e == "bad token [redacted]")
        );
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
        f.connect("shop.test", Platform::PrestaShop).await;
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
        f.connect("b.test", Platform::PrestaShop).await;
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
        f.connect("b.test", Platform::PrestaShop).await;
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

    fn tomorrow_at(hour: u32) -> NaiveDateTime {
        (chrono::Local::now().date_naive() + chrono::Days::new(1))
            .and_hms_opt(hour, 0, 0)
            .unwrap()
    }

    async fn shop_with_task(f: &mut Fixture, schedule: &str) -> i64 {
        f.configure().await;
        f.connect("a.test", Platform::PrestaShop).await;
        f.backend
            .handle(Command::CreateTask {
                name: "Morning".into(),
                prompt: "Summarise yesterday".into(),
                schedule: schedule.into(),
            })
            .await;
        f.store.list_tasks(None).unwrap()[0].id
    }

    #[tokio::test]
    async fn due_task_runs_once_per_period() {
        let mut f = fixture();
        shop_with_task(&mut f, "daily 09:00").await;
        f.provider.push_text("Sales were fine").push_text("again");
        // The first 09:00 after creation, so the test holds at any local time.
        let created = to_local(&f.store.list_tasks(None).unwrap()[0].created_at).unwrap();
        let mut slot = created.date().and_hms_opt(9, 0, 0).unwrap();
        if slot <= created {
            slot += chrono::Duration::days(1);
        }
        f.backend
            .run_due_tasks_at(slot - chrono::Duration::minutes(1))
            .await;
        assert!(f.provider.requests().is_empty(), "not due before 09:00");
        f.backend.run_due_tasks_at(slot).await;
        f.backend
            .run_due_tasks_at(slot + chrono::Duration::hours(1))
            .await;
        assert_eq!(f.provider.requests().len(), 1);

        let task = &f.store.list_tasks(None).unwrap()[0];
        let conv = task.conversation_id.unwrap();
        assert_eq!(f.store.messages(conv).unwrap().len(), 2);
        assert!(f.events().iter().any(
            |e| matches!(e, BackendEvent::TaskFinished { name, error: None } if name == "Morning")
        ));
        let req = &f.provider.requests()[0];
        assert!(
            matches!(&req.messages[0], ChatMessage::System { text } if text.contains("unattended"))
        );
        assert_eq!(
            f.backend.conversation_id(),
            None,
            "the open chat is untouched"
        );
    }

    #[tokio::test]
    async fn missed_task_runs_once_on_startup() {
        let store = Store::open_in_memory().unwrap();
        let mut first = fixture_with(store.clone());
        shop_with_task(&mut first, "daily 09:00").await;

        let mut second = fixture_with(store);
        second.provider.push_text("Caught up").push_text("x");
        second.backend.handle(Command::Init).await;
        let three_days_later = tomorrow_at(12) + chrono::Days::new(2);
        second.backend.run_due_tasks_at(three_days_later).await;
        second.backend.run_due_tasks_at(three_days_later).await;
        assert_eq!(second.provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn unattended_run_never_executes_mutating_tool() {
        let mut f = fixture();
        let id = shop_with_task(&mut f, "every 6h").await;
        f.provider
            .push_tool_call(
                "c",
                "shop_update_order_status",
                json!({"id": "1", "status": "completed"}),
            )
            .push_text("I would complete order 1");
        f.backend.handle(Command::RunTaskNow(id)).await;
        let audit = f.store.audit_log(None, 10).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].stage, "denied");
        assert!(
            !f.events()
                .iter()
                .any(|e| matches!(e, BackendEvent::Agent(_))),
            "unattended runs do not stream into the open chat"
        );
    }

    #[tokio::test]
    async fn invalid_schedules_and_shopless_tasks_are_rejected() {
        let mut f = fixture();
        f.backend
            .handle(Command::CreateTask {
                name: "x".into(),
                prompt: "y".into(),
                schedule: "daily 09:00".into(),
            })
            .await;
        assert!(
            matches!(f.events().last(), Some(BackendEvent::Error(e)) if e.contains("Connect a shop"))
        );
        f.connect("a.test", Platform::PrestaShop).await;
        f.backend
            .handle(Command::CreateTask {
                name: "x".into(),
                prompt: "y".into(),
                schedule: "sometimes".into(),
            })
            .await;
        assert!(
            matches!(f.events().last(), Some(BackendEvent::Error(e)) if e.contains("unknown schedule"))
        );
        assert!(f.store.list_tasks(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn error_events_never_contain_saved_secrets() {
        let mut f = fixture();
        f.configure().await;
        f.backend
            .handle(Command::ConnectShop {
                url: "a.test".into(),
                kind: ConnectorKind::Shop(Platform::Shopify),
                credentials: Credentials::from([(
                    "access_token".into(),
                    "shpat_supersecret".into(),
                )]),
            })
            .await;
        f.backend.handle(Command::OpenConversation(9999)).await;
        f.backend.emit(BackendEvent::Error(
            "upstream said: token shpat_supersecret and key k".into(),
        ));
        assert!(matches!(
            f.events().last(),
            Some(BackendEvent::Error(e)) if e == "upstream said: token [redacted] and key k"
        ));
    }

    #[tokio::test]
    async fn audit_log_is_listed_for_the_active_shop() {
        let mut f = fixture();
        f.configure().await;
        f.connect("shop.test", Platform::PrestaShop).await;
        f.provider
            .push_tool_call(
                "c1",
                "shop_update_order_status",
                json!({"id": "1", "status": "completed"}),
            )
            .push_text("done");
        f.send("complete order 1").await;
        f.backend.handle(Command::LoadAudit).await;
        let Some(BackendEvent::Audit(rows)) = f.events().last().cloned() else {
            panic!("{:?}", f.events().last());
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].stage, "failed", "newest first");
        assert_eq!(rows[1].tool, "shop_update_order_status");
    }

    /// Google endpoints for one user with a single client account 1234567890.
    fn google_routes(t: &FakeTransport) {
        use connectors::transport::{HttpResponse, Method};
        t.on(
            Method::Post,
            "/token",
            HttpResponse::new(200, r#"{"access_token":"ya29.x","expires_in":3600,"refresh_token":"1//refresh-secret"}"#),
        );
        t.on(
            Method::Get,
            "/v26/customers:listAccessibleCustomers",
            HttpResponse::new(200, r#"{"resourceNames":["customers/1234567890"]}"#),
        );
        t.on(
            Method::Post,
            "/v26/customers/1234567890/googleAds:search",
            HttpResponse::new(200, r#"{"results":[{"customer":{"id":"1234567890","descriptiveName":"Hoodies ES","currencyCode":"EUR","manager":false}}]}"#),
        );
    }

    /// Plays the browser: approves the consent and follows the redirect.
    fn approving_browser() -> Arc<dyn Fn(&str) + Send + Sync> {
        Arc::new(|url: &str| {
            let q: std::collections::HashMap<_, _> = reqwest::Url::parse(url)
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect();
            let target = format!("{}/?code=4%2Fok&state={}", q["redirect_uri"], q["state"]);
            tokio::spawn(async move {
                let _ = reqwest::get(target).await;
            });
        })
    }

    #[tokio::test]
    async fn google_ads_sign_in_lists_accounts_and_links_the_chosen_one() {
        let mut f = fixture();
        google_routes(&f.transport);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        f.backend.commands = Some(tx);
        f.backend.open_browser = approving_browser();

        f.backend.handle(Command::GoogleSignIn).await;
        assert!(
            matches!(f.events().last(), Some(BackendEvent::Error(e)) if e.contains("Connect a shop")),
            "Google Ads belongs to a shop"
        );
        f.connect("a.test", Platform::Shopify).await;
        f.backend.handle(Command::GoogleSignIn).await;
        assert!(
            matches!(f.events().last(), Some(BackendEvent::Error(e)) if e.contains("app credentials"))
        );

        f.backend
            .handle(Command::SaveGoogleAdsApp {
                client_id: "cid.apps.googleusercontent.com".into(),
                client_secret: "GOCSPX-secret".into(),
                developer_token: "devtoken-secret".into(),
            })
            .await;
        f.backend.handle(Command::GoogleSignIn).await;
        assert!(matches!(
            f.events().last(),
            Some(BackendEvent::GoogleSignIn(GoogleSignInState::Waiting))
        ));
        let finished = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !format!("{finished:?}").contains("refresh-secret"),
            "tokens never appear in Debug output"
        );
        f.backend.handle(finished).await;
        let Some(BackendEvent::GoogleSignIn(GoogleSignInState::Accounts(accounts))) =
            f.events().last().cloned()
        else {
            panic!("{:?}", f.events().last());
        };
        assert_eq!(accounts[0].name, "Hoodies ES");

        f.backend
            .handle(Command::ConnectGoogleAds {
                customer_id: "123-456-7890".into(),
            })
            .await;
        assert!(f.backend.tools().get("ads_draft_search_campaign").is_some());
        assert!(
            f.backend.tools().get("shop_list_orders").is_some(),
            "shop tools stay"
        );
        let shop = f.backend.active_shop_id().unwrap();
        let saved = f.store.credentials(shop).unwrap();
        let ads = saved.iter().find(|r| r.kind == "ads:google").unwrap();
        assert_eq!(ads.credentials["refresh_token"], "1//refresh-secret");
        assert_eq!(ads.credentials["customer_id"], "1234567890");
        assert!(f.events().iter().any(|e| matches!(
            e,
            BackendEvent::Shops { shops, .. } if shops[0].connectors.contains(&ConnectorKind::GoogleAds)
        )));

        f.backend.emit(BackendEvent::Error(
            "echo 1//refresh-secret devtoken-secret".into(),
        ));
        assert!(matches!(
            f.events().last(),
            Some(BackendEvent::Error(e)) if e == "echo [redacted] [redacted]"
        ));

        f.backend
            .handle(Command::DisconnectConnector(ConnectorKind::GoogleAds))
            .await;
        assert!(f.backend.tools().get("ads_draft_search_campaign").is_none());
        assert_eq!(f.store.credentials(shop).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn google_app_setup_keeps_saved_secrets_and_never_emits_them() {
        let mut f = fixture();
        f.backend
            .handle(Command::SaveGoogleAdsApp {
                client_id: "cid".into(),
                client_secret: "".into(),
                developer_token: "tok".into(),
            })
            .await;
        assert!(matches!(f.events().last(), Some(BackendEvent::Error(_))));
        f.backend
            .handle(Command::SaveGoogleAdsApp {
                client_id: "cid".into(),
                client_secret: "sec".into(),
                developer_token: "tok".into(),
            })
            .await;
        f.backend
            .handle(Command::SaveGoogleAdsApp {
                client_id: "cid2".into(),
                client_secret: "".into(),
                developer_token: "".into(),
            })
            .await;
        assert!(matches!(
            f.events().last(),
            Some(BackendEvent::GoogleAdsApp { configured: true, client_id }) if client_id == "cid2"
        ));
        let app: serde_json::Value =
            serde_json::from_str(&f.store.app_setting(GOOGLE_APP_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(app["client_secret"], "sec");
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
