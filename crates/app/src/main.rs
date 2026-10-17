//! Native desktop shell. Rendering only: behaviour lives in `state::AppState`
//! and the backend in `chat-core`.

mod markdown;
mod runtime;
mod state;

use chat_core::{Command, Credentials};
use connectors::{ConnectorKind, ConnectorRegistry, Platform};
use eframe::egui::{self, Color32, RichText};
use egui_commonmark::CommonMarkCache;
use llm::{ProviderConfig, ProviderId};
use runtime::Runtime;
use state::{pretty_args, AppState, CardState, ChatItem, ToolStatus};
use tools::Mutability;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_title("merchantGPT"),
        ..Default::default()
    };
    eframe::run_native(
        "merchantGPT",
        options,
        Box::new(|cc| Ok(Box::new(App::new(Runtime::start(cc.egui_ctx.clone()))))),
    )
}

const RED: Color32 = Color32::from_rgb(200, 60, 60);
const GREEN: Color32 = Color32::from_rgb(60, 150, 60);
const AMBER: Color32 = Color32::from_rgb(220, 160, 40);
const BLUE: Color32 = Color32::from_rgb(70, 130, 180);

#[derive(Default)]
enum ConnectorFlow {
    #[default]
    Closed,
    PickConnector,
    PickPlatform,
    Form {
        kind: ConnectorKind,
        url: String,
        values: Credentials,
    },
    GoogleAds(GoogleAdsForm),
}

#[derive(Default)]
struct GoogleAdsForm {
    show_setup: bool,
    client_id: String,
    client_secret: String,
    developer_token: String,
    selected: Option<String>,
}

#[derive(Default)]
enum Dialog {
    #[default]
    None,
    RemoveShop {
        id: i64,
        label: String,
    },
    RenameConversation {
        id: i64,
        title: String,
    },
    DeleteConversation {
        id: i64,
        title: String,
    },
    NewTask {
        name: String,
        prompt: String,
        schedule: String,
    },
    DeleteTask {
        id: i64,
        name: String,
    },
}

struct SettingsForm {
    open: bool,
    provider: ProviderId,
    model: String,
    base_url: String,
    /// API key, Bedrock API key, or AWS secret access key.
    api_key: String,
    region: String,
    /// Bedrock: sign with AWS access keys instead of a Bedrock API key.
    aws_access_keys: bool,
    access_key_id: String,
    session_token: String,
}

impl Default for SettingsForm {
    fn default() -> Self {
        Self {
            open: false,
            provider: ProviderId::Anthropic,
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            region: "us-east-1".into(),
            aws_access_keys: false,
            access_key_id: String::new(),
            session_token: String::new(),
        }
    }
}

impl SettingsForm {
    fn options(&self) -> std::collections::BTreeMap<String, String> {
        let mut o = std::collections::BTreeMap::new();
        if self.provider == ProviderId::Bedrock {
            o.insert(llm::OPT_REGION.to_string(), self.region.trim().to_string());
            if self.aws_access_keys {
                o.insert(
                    llm::OPT_AWS_ACCESS_KEY_ID.to_string(),
                    self.access_key_id.trim().to_string(),
                );
                o.insert(
                    llm::OPT_AWS_SESSION_TOKEN.to_string(),
                    self.session_token.trim().to_string(),
                );
            }
        }
        o.retain(|_, v| !v.is_empty());
        o
    }

    /// The backend reuses a saved secret only for the same AWS key id.
    fn saved_secret_applies(&self, saved: Option<&chat_core::LlmSettingSummary>) -> bool {
        saved.is_some_and(|s| {
            s.options
                .get(llm::OPT_AWS_ACCESS_KEY_ID)
                .map(String::as_str)
                == self
                    .options()
                    .get(llm::OPT_AWS_ACCESS_KEY_ID)
                    .map(String::as_str)
        })
    }

    fn base_url_hint(&self) -> String {
        self.provider
            .default_base_url()
            .replace("{region}", self.region.trim())
    }
}

struct App {
    markdown: CommonMarkCache,
    audit_open: bool,
    runtime: Result<Runtime, String>,
    state: AppState,
    input: String,
    flow: ConnectorFlow,
    dialog: Dialog,
    settings: SettingsForm,
}

impl App {
    fn new(runtime: Result<Runtime, String>) -> Self {
        Self {
            markdown: CommonMarkCache::default(),
            audit_open: false,
            runtime,
            state: AppState::default(),
            input: String::new(),
            flow: ConnectorFlow::Closed,
            dialog: Dialog::None,
            settings: SettingsForm::default(),
        }
    }

    fn send(&self, cmd: Command) {
        if let Ok(rt) = &self.runtime {
            rt.send(cmd);
        }
    }

    fn send_input(&mut self) {
        if let Some(cmd) = self.state.begin_send(&self.input) {
            self.send(cmd);
            self.input.clear();
        }
    }

    /// Fills the form from saved settings, then config defaults, then suggestions.
    fn load_settings_form(&mut self, provider: ProviderId) {
        let s = &mut self.settings;
        s.provider = provider;
        s.api_key.clear();
        s.session_token.clear();
        match self.state.saved_llm(provider) {
            Some(saved) => {
                s.model = saved.model.clone();
                s.base_url = saved.base_url.clone().unwrap_or_default();
                if let Some(r) = saved.options.get(llm::OPT_REGION) {
                    s.region = r.clone();
                }
                s.access_key_id = saved
                    .options
                    .get(llm::OPT_AWS_ACCESS_KEY_ID)
                    .cloned()
                    .unwrap_or_default();
                s.aws_access_keys = !s.access_key_id.is_empty();
            }
            None => {
                let defaults = self.runtime.as_ref().ok().map(|r| &r.config.llm);
                let same_provider = defaults
                    .and_then(|d| d.provider.as_deref())
                    .and_then(ProviderId::parse)
                    == Some(provider);
                s.model = defaults
                    .filter(|_| same_provider)
                    .and_then(|d| d.model.clone())
                    .unwrap_or_else(|| provider.suggested_model().into());
                s.base_url = defaults
                    .filter(|_| same_provider)
                    .and_then(|d| d.base_url.clone())
                    .unwrap_or_default();
            }
        }
    }

    fn open_settings(&mut self) {
        let provider = self
            .state
            .llm
            .as_ref()
            .map(|(p, _)| *p)
            .or_else(|| {
                self.runtime
                    .as_ref()
                    .ok()
                    .and_then(|r| r.config.llm.provider.as_deref())
                    .and_then(ProviderId::parse)
            })
            .unwrap_or(ProviderId::Anthropic);
        self.load_settings_form(provider);
        self.settings.open = true;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let events: Vec<_> = match &self.runtime {
            Ok(rt) => rt.drain().collect(),
            Err(e) => {
                let e = e.clone();
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("merchantGPT could not start");
                    ui.colored_label(RED, e);
                    ui.label("The encrypted database key is kept in your OS keychain. Make sure it is unlocked and accessible, then restart the app.");
                });
                return;
            }
        };
        for ev in events {
            self.state.apply(ev);
        }
        self.top_bar(ui);
        self.side_panel(ui);
        self.input_bar(ui);
        egui::CentralPanel::default().show(ui, |ui| self.transcript(ui));
        self.settings_window(ui.ctx());
        self.audit_window(ui.ctx());
        self.connector_window(ui.ctx());
        self.dialogs(ui.ctx());
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("merchantGPT");
                ui.separator();
                match self.state.active_shop() {
                    Some(shop) => {
                        let kind = shop.kind.map(|k| k.display_name()).unwrap_or_default();
                        ui.label(format!("{} · {kind}", shop.label()))
                    }
                    None => ui.weak("No shop connected"),
                };
                ui.separator();
                match &self.state.llm {
                    Some((p, m)) => ui.label(format!("{} · {m}", p.display_name())),
                    None => ui.weak("No AI provider"),
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.open_settings();
                    }
                    if ui
                        .add_enabled(
                            self.state.active_shop.is_some(),
                            egui::Button::new("Activity"),
                        )
                        .on_hover_text(
                            "Every change the assistant made or tried to make in this shop",
                        )
                        .clicked()
                    {
                        self.audit_open = true;
                        self.send(Command::LoadAudit);
                    }
                    let u = self.state.usage;
                    if u.input_tokens + u.output_tokens > 0 {
                        ui.weak(format!(
                            "tokens in {} / out {}",
                            u.input_tokens, u.output_tokens
                        ));
                    }
                });
            });
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        let mut cmds = Vec::new();
        egui::Panel::left("side_panel")
            .default_size(230.0)
            .resizable(true)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.strong("Shops");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Add").clicked() {
                            self.flow = ConnectorFlow::PickPlatform;
                        }
                    });
                });
                if self.state.shops.is_empty() {
                    ui.weak("No shops yet.");
                }
                for shop in &self.state.shops {
                    let active = Some(shop.id) == self.state.active_shop;
                    let has_ads = shop.connectors.contains(&ConnectorKind::GoogleAds);
                    let label = if has_ads {
                        format!("{} · Ads", shop.label())
                    } else {
                        shop.label()
                    };
                    let resp = ui
                        .add_enabled(!self.state.busy, egui::Button::selectable(active, label))
                        .on_hover_text(&shop.url);
                    if resp.clicked() && !active {
                        cmds.push(Command::SelectShop(shop.id));
                    }
                    resp.context_menu(|ui| {
                        if active && has_ads && ui.button("Unlink Google Ads").clicked() {
                            cmds.push(Command::DisconnectConnector(ConnectorKind::GoogleAds));
                            ui.close();
                        }
                        if ui.button("Remove shop…").clicked() {
                            self.dialog = Dialog::RemoveShop {
                                id: shop.id,
                                label: shop.label(),
                            };
                            ui.close();
                        }
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.strong("Chats");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!self.state.busy, egui::Button::new("New").small())
                            .clicked()
                        {
                            cmds.push(Command::NewConversation);
                        }
                    });
                });
                if self.state.active_shop.is_some() {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.strong("Tasks");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("New").clicked() {
                                self.dialog = Dialog::NewTask {
                                    name: String::new(),
                                    prompt: String::new(),
                                    schedule: "daily 09:00".into(),
                                };
                            }
                        });
                    });
                    for t in &self.state.tasks {
                        ui.horizontal(|ui| {
                            let mut enabled = t.enabled;
                            if ui.checkbox(&mut enabled, "").changed() {
                                cmds.push(Command::SetTaskEnabled { id: t.id, enabled });
                            }
                            let last = t.last_run_at.as_deref().unwrap_or("never");
                            let resp = ui.label(&t.name).on_hover_text(format!(
                                "{}\nLast run: {last}\n\n{}",
                                t.schedule, t.prompt
                            ));
                            resp.context_menu(|ui| {
                                if ui
                                    .add_enabled(!self.state.busy, egui::Button::new("Run now"))
                                    .clicked()
                                {
                                    cmds.push(Command::RunTaskNow(t.id));
                                    ui.close();
                                }
                                if ui.button("Delete…").clicked() {
                                    self.dialog = Dialog::DeleteTask {
                                        id: t.id,
                                        name: t.name.clone(),
                                    };
                                    ui.close();
                                }
                            });
                        });
                    }
                    ui.separator();
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for c in &self.state.conversations {
                        let active = Some(c.id) == self.state.active_conversation;
                        let resp = ui
                            .add_enabled(
                                !self.state.busy,
                                egui::Button::selectable(active, &c.title).truncate(),
                            )
                            .on_hover_text(&c.updated_at);
                        if resp.clicked() && !active {
                            cmds.push(Command::OpenConversation(c.id));
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Rename…").clicked() {
                                self.dialog = Dialog::RenameConversation {
                                    id: c.id,
                                    title: c.title.clone(),
                                };
                                ui.close();
                            }
                            if ui.button("Delete…").clicked() {
                                self.dialog = Dialog::DeleteConversation {
                                    id: c.id,
                                    title: c.title.clone(),
                                };
                                ui.close();
                            }
                        });
                    }
                });
            });
        for c in cmds {
            self.send(c);
        }
    }

    fn input_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("input_bar").show(ui, |ui| {
            ui.add_space(6.0);
            let suggestions: Vec<(String, String)> = self
                .state
                .skill_suggestions(&self.input)
                .into_iter()
                .map(|s| (s.name.clone(), s.description.clone()))
                .collect();
            if !suggestions.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.weak("Skills:");
                    for (name, description) in suggestions {
                        if ui
                            .small_button(format!("/{name}"))
                            .on_hover_text(description)
                            .clicked()
                        {
                            self.input = format!("/{name} ");
                        }
                    }
                });
            }
            ui.horizontal(|ui| {
                if ui.button("+").on_hover_text("Add a connector").clicked() {
                    self.flow = ConnectorFlow::PickConnector;
                }
                let width = ui.available_width() - 70.0;
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut self.input)
                        .desired_rows(2)
                        .desired_width(width)
                        .hint_text("Ask about orders, products, customers, sales… (Shift+Enter for a new line)"),
                );
                let enter = resp.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                if self.state.busy {
                    if ui.button("Stop").clicked() {
                        self.state.cancel_turn();
                    }
                } else if ui.button("Send").clicked() || enter {
                    self.send_input();
                    resp.request_focus();
                }
            });
            ui.add_space(6.0);
        });
    }

    fn transcript(&mut self, ui: &mut egui::Ui) {
        let mut answers = Vec::new();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.state.items.is_empty() {
                    ui.weak("Connect your shop with the + button and configure an AI provider in Settings, then ask anything about your store. Type / to run a skill.");
                }
                for (idx, item) in self.state.items.iter().enumerate() {
                    render_item(ui, idx, item, &mut answers, &mut self.markdown);
                    ui.add_space(8.0);
                }
                if self.state.busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("Working…");
                    });
                }
            });
        for (idx, approve) in answers {
            self.state.answer_confirmation(idx, approve);
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings.open {
            return;
        }
        let mut open = true;
        let mut save = false;
        let mut switch_to = None;
        let saved = self.state.saved_llm(self.settings.provider).cloned();
        let has_saved_key = self.settings.saved_secret_applies(saved.as_ref());
        let has_saved_token = has_saved_key && saved.as_ref().is_some_and(|s| s.has_secret_options);
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                let s = &mut self.settings;
                egui::Grid::new("llm_settings")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("AI provider");
                        let mut provider = s.provider;
                        egui::ComboBox::from_id_salt("provider")
                            .selected_text(provider.display_name())
                            .show_ui(ui, |ui| {
                                for p in ProviderId::ALL {
                                    ui.selectable_value(&mut provider, p, p.display_name());
                                }
                            });
                        if provider != s.provider {
                            switch_to = Some(provider);
                        }
                        ui.end_row();
                        ui.label("Model");
                        ui.text_edit_singleline(&mut s.model);
                        ui.end_row();
                        let saved_hint = |saved: bool| if saved { "saved; leave blank to keep" } else { "" };
                        if s.provider == ProviderId::Bedrock {
                            ui.label("AWS region");
                            ui.add(egui::TextEdit::singleline(&mut s.region).hint_text("us-east-1"));
                            ui.end_row();
                            ui.label("Authentication");
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut s.aws_access_keys, false, "Bedrock API key");
                                ui.radio_value(&mut s.aws_access_keys, true, "AWS access keys");
                            });
                            ui.end_row();
                            if s.aws_access_keys {
                                ui.label("Access key ID");
                                ui.text_edit_singleline(&mut s.access_key_id);
                                ui.end_row();
                                ui.label("Secret access key");
                            } else {
                                ui.label("Bedrock API key");
                            }
                        } else {
                            ui.label("API key");
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut s.api_key)
                                .password(true)
                                .hint_text(saved_hint(has_saved_key)),
                        );
                        ui.end_row();
                        if s.provider == ProviderId::Bedrock && s.aws_access_keys {
                            ui.label("Session token");
                            ui.add(
                                egui::TextEdit::singleline(&mut s.session_token)
                                    .password(true)
                                    .hint_text(if has_saved_token {
                                        "saved; leave blank to keep"
                                    } else {
                                        "only for temporary credentials"
                                    }),
                            );
                            ui.end_row();
                        }
                        ui.label("Base URL");
                        let hint = s.base_url_hint();
                        ui.add(egui::TextEdit::singleline(&mut s.base_url).hint_text(hint));
                        ui.end_row();
                    });
                if s.provider == ProviderId::Bedrock {
                    ui.weak("Claude in Amazon Bedrock: model ids start with \"anthropic.\". Temporary credentials expire; paste new ones when they do.");
                }
                ui.weak("Keys are stored in the encrypted local database, never in config.toml.");
                let bedrock_ok = s.provider != ProviderId::Bedrock
                    || (!s.region.trim().is_empty()
                        && (!s.aws_access_keys || !s.access_key_id.trim().is_empty()));
                let ready = bedrock_ok
                    && !s.model.trim().is_empty()
                    && (has_saved_key || !s.api_key.trim().is_empty());
                if ui.add_enabled(ready, egui::Button::new("Save")).clicked() {
                    save = true;
                }
            });
        if let Some(p) = switch_to {
            self.load_settings_form(p);
        }
        if save {
            let s = &self.settings;
            self.send(Command::SetLlm(ProviderConfig {
                options: s.options(),
                provider: s.provider,
                api_key: s.api_key.trim().to_string(),
                base_url: Some(s.base_url.trim().to_string()).filter(|u| !u.is_empty()),
                model: s.model.trim().to_string(),
            }));
            self.settings.api_key.clear();
            self.settings.session_token.clear();
            open = false;
        }
        self.settings.open = open;
    }

    fn audit_window(&mut self, ctx: &egui::Context) {
        if !self.audit_open {
            return;
        }
        let mut open = true;
        let mut refresh = false;
        egui::Window::new("Activity log")
            .open(&mut open)
            .default_size([760.0, 420.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.weak("Changes to the shop, approved or denied, newest first.");
                    if ui.small_button("Refresh").clicked() {
                        refresh = true;
                    }
                });
                if self.state.audit.is_empty() {
                    ui.weak("Nothing yet.");
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("audit")
                        .striped(true)
                        .num_columns(5)
                        .show(ui, |ui| {
                            ui.strong("When");
                            ui.strong("Action");
                            ui.strong("Outcome");
                            ui.strong("Arguments");
                            ui.strong("Detail");
                            ui.end_row();
                            for row in &self.state.audit {
                                ui.label(
                                    row.created_at
                                        .replace('T', " ")
                                        .trim_end_matches('Z')
                                        .to_string(),
                                );
                                ui.label(&row.tool);
                                let color = match row.stage.as_str() {
                                    "succeeded" => GREEN,
                                    "failed" | "denied" => RED,
                                    _ => Color32::GRAY,
                                };
                                ui.colored_label(color, &row.stage);
                                ui.label(RichText::new(&row.arguments).monospace().small());
                                ui.label(
                                    RichText::new(row.detail.as_deref().unwrap_or("")).small(),
                                );
                                ui.end_row();
                            }
                        });
                });
            });
        if refresh {
            self.send(Command::LoadAudit);
        }
        self.audit_open = open;
    }

    fn connector_window(&mut self, ctx: &egui::Context) {
        let mut next: Option<ConnectorFlow> = None;
        let mut connect: Option<Command> = None;
        match &mut self.flow {
            ConnectorFlow::Closed => {}
            ConnectorFlow::PickConnector => {
                egui::Window::new("Add a connector")
                    .resizable(false)
                    .show(ctx, |ui| {
                        if ui.button("Shop").clicked() {
                            next = Some(ConnectorFlow::PickPlatform);
                        }
                        let has_shop = self.state.active_shop.is_some();
                        let google = ui
                            .add_enabled(has_shop, egui::Button::new("Google Ads"))
                            .on_hover_text("Draft campaigns in your Google Ads account")
                            .on_disabled_hover_text(
                                "Connect a shop first: Google Ads is linked to a shop",
                            );
                        if google.clicked() {
                            next = Some(ConnectorFlow::GoogleAds(GoogleAdsForm {
                                show_setup: !self.state.google_app_configured,
                                client_id: self.state.google_app_client_id.clone(),
                                ..Default::default()
                            }));
                        }
                        ui.weak("More connectors (carriers) are coming.");
                        if ui.button("Cancel").clicked() {
                            next = Some(ConnectorFlow::Closed);
                        }
                    });
            }
            ConnectorFlow::GoogleAds(form) => {
                let (close, cmd) = google_ads_window(ctx, form, &self.state);
                connect = cmd;
                if close {
                    next = Some(ConnectorFlow::Closed);
                }
            }
            ConnectorFlow::PickPlatform => {
                egui::Window::new("Choose your platform")
                    .resizable(false)
                    .show(ctx, |ui| {
                        for p in Platform::ALL {
                            if ui.button(p.display_name()).clicked() {
                                next = Some(ConnectorFlow::Form {
                                    kind: ConnectorKind::Shop(p),
                                    url: String::new(),
                                    values: Credentials::new(),
                                });
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            next = Some(ConnectorFlow::Closed);
                        }
                    });
            }
            ConnectorFlow::Form { kind, url, values } => {
                let schema = ConnectorRegistry::credential_schema(*kind);
                egui::Window::new(format!("{} credentials", kind.display_name()))
                    .resizable(false)
                    .show(ctx, |ui| {
                        ui.label("Shop URL");
                        ui.add(egui::TextEdit::singleline(url).hint_text("https://myshop.com"));
                        for field in schema.fields() {
                            ui.label(field.label);
                            let value = values.entry(field.key.to_string()).or_default();
                            ui.add(egui::TextEdit::singleline(value).password(field.secret));
                        }
                        if let Some(help) = schema.help() {
                            ui.add_space(4.0);
                            ui.add(egui::Label::new(RichText::new(help).weak().small()).wrap());
                        }
                        let complete = !url.trim().is_empty() && schema.missing(values).is_empty();
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(complete, egui::Button::new("Connect"))
                                .clicked()
                            {
                                connect = Some(Command::ConnectShop {
                                    url: url.clone(),
                                    kind: *kind,
                                    credentials: values.clone(),
                                });
                                next = Some(ConnectorFlow::Closed);
                            }
                            if ui.button("Cancel").clicked() {
                                next = Some(ConnectorFlow::Closed);
                            }
                        });
                    });
            }
        }
        if let Some(cmd) = connect {
            self.send(cmd);
        }
        if let Some(n) = next {
            self.flow = n;
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut cmd = None;
        match &mut self.dialog {
            Dialog::None => {}
            Dialog::RemoveShop { id, label } => {
                egui::Window::new("Remove shop").resizable(false).show(ctx, |ui| {
                    ui.label(format!(
                        "Remove {label}? Its saved credentials, chats, notes and tasks are deleted from this computer. The shop itself is not touched."
                    ));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Remove").color(RED)).clicked() {
                            cmd = Some(Command::RemoveShop(*id));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::RenameConversation { id, title } => {
                egui::Window::new("Rename chat")
                    .resizable(false)
                    .show(ctx, |ui| {
                        ui.text_edit_singleline(title);
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                cmd = Some(Command::RenameConversation {
                                    id: *id,
                                    title: title.clone(),
                                });
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
            }
            Dialog::NewTask {
                name,
                prompt,
                schedule,
            } => {
                egui::Window::new("New scheduled task")
                    .resizable(false)
                    .show(ctx, |ui| {
                        if ui.small_button("Use preset: daily sales summary").clicked() {
                            *name = "Daily sales summary".into();
                            *prompt = "/daily-sales-summary".into();
                            *schedule = "daily 09:00".into();
                        }
                        ui.label("Name");
                        ui.text_edit_singleline(name);
                        ui.label("What should the assistant do? (a /skill works too)");
                        ui.add(egui::TextEdit::multiline(prompt).desired_rows(3));
                        ui.label("Schedule");
                        ui.add(
                            egui::TextEdit::singleline(schedule)
                                .hint_text("daily 09:00 · weekly mon 08:30 · every 6h"),
                        );
                        let parsed = schedule.parse::<chat_core::scheduler::Schedule>();
                        if let Err(e) = &parsed {
                            ui.colored_label(RED, e);
                        }
                        ui.weak("Scheduled runs can read your shop but never change it.");
                        ui.horizontal(|ui| {
                            let ready = parsed.is_ok()
                                && !name.trim().is_empty()
                                && !prompt.trim().is_empty();
                            if ui.add_enabled(ready, egui::Button::new("Create")).clicked() {
                                cmd = Some(Command::CreateTask {
                                    name: name.clone(),
                                    prompt: prompt.clone(),
                                    schedule: schedule.clone(),
                                });
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
            }
            Dialog::DeleteTask { id, name } => {
                egui::Window::new("Delete task").resizable(false).show(ctx, |ui| {
                    ui.label(format!("Delete the scheduled task \"{name}\"? Its past reports stay in their chat."));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Delete").color(RED)).clicked() {
                            cmd = Some(Command::DeleteTask(*id));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::DeleteConversation { id, title } => {
                egui::Window::new("Delete chat")
                    .resizable(false)
                    .show(ctx, |ui| {
                        ui.label(format!("Delete \"{title}\"? This cannot be undone."));
                        ui.horizontal(|ui| {
                            if ui.button(RichText::new("Delete").color(RED)).clicked() {
                                cmd = Some(Command::DeleteConversation(*id));
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
            }
        }
        if let Some(c) = cmd {
            self.send(c);
        }
        if close {
            self.dialog = Dialog::None;
        }
    }
}

/// The Google Ads connector: one-time app setup, then "Sign in with Google",
/// then the account to link. Returns whether to close and a command to send.
fn google_ads_window(
    ctx: &egui::Context,
    form: &mut GoogleAdsForm,
    state: &AppState,
) -> (bool, Option<Command>) {
    use chat_core::GoogleSignInState as S;
    let mut close = false;
    let mut cmd = None;
    let waiting = state.google_sign_in == S::Waiting;
    egui::Window::new("Connect Google Ads")
        .resizable(false)
        .default_width(460.0)
        .show(ctx, |ui| {
            let shop = state.active_shop().map(|s| s.label()).unwrap_or_default();
            ui.add(
                egui::Label::new(format!(
                    "Sign in with the Google account that manages your ads. The assistant will be able to read campaigns and create paused campaign drafts for {shop}; nothing runs until you enable it in Google Ads."
                ))
                .wrap(),
            );
            ui.add_space(6.0);

            egui::CollapsingHeader::new("App credentials (one-time setup)")
                .id_salt("google_app_setup")
                .open(Some(form.show_setup))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new("Whoever distributes merchantGPT creates these once: a Google Cloud OAuth client of type \"Desktop app\" with the Google Ads API enabled, and a developer token from the API Center of a Google Ads manager account.")
                                .weak()
                                .small(),
                        )
                        .wrap(),
                    );
                    let saved = |configured: bool| if configured { "saved; leave blank to keep" } else { "" };
                    egui::Grid::new("google_app").num_columns(2).show(ui, |ui| {
                        ui.label("OAuth client ID");
                        ui.text_edit_singleline(&mut form.client_id);
                        ui.end_row();
                        ui.label("OAuth client secret");
                        ui.add(
                            egui::TextEdit::singleline(&mut form.client_secret)
                                .password(true)
                                .hint_text(saved(state.google_app_configured)),
                        );
                        ui.end_row();
                        ui.label("Developer token");
                        ui.add(
                            egui::TextEdit::singleline(&mut form.developer_token)
                                .password(true)
                                .hint_text(saved(state.google_app_configured)),
                        );
                        ui.end_row();
                    });
                    let ready = !form.client_id.trim().is_empty()
                        && (state.google_app_configured
                            || (!form.client_secret.trim().is_empty()
                                && !form.developer_token.trim().is_empty()));
                    if ui.add_enabled(ready, egui::Button::new("Save")).clicked() {
                        cmd = Some(Command::SaveGoogleAdsApp {
                            client_id: form.client_id.clone(),
                            client_secret: std::mem::take(&mut form.client_secret),
                            developer_token: std::mem::take(&mut form.developer_token),
                        });
                        form.show_setup = false;
                    }
                });
            if !form.show_setup && ui.small_button("Edit app credentials").clicked() {
                form.show_setup = true;
            }
            ui.separator();

            match &state.google_sign_in {
                S::Waiting => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Finish signing in in your browser…");
                    });
                    if ui.button("Cancel sign-in").clicked() {
                        cmd = Some(Command::CancelGoogleSignIn);
                    }
                }
                S::Accounts(accounts) => {
                    ui.strong("Choose the account to link");
                    for a in accounts {
                        let mut label = format!(
                            "{}  ({})  {}",
                            a.name,
                            connectors::google_ads::format_customer_id(&a.customer_id),
                            a.currency
                        );
                        if a.test_account {
                            label.push_str("  · test account");
                        }
                        if a.manager {
                            label.push_str("  · manager");
                        }
                        let selected = form.selected.as_deref() == Some(a.customer_id.as_str());
                        let resp = ui
                            .add_enabled(!a.manager, egui::Button::selectable(selected, label))
                            .on_disabled_hover_text("Manager accounts cannot hold campaigns");
                        if resp.clicked() {
                            form.selected = Some(a.customer_id.clone());
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(form.selected.is_some(), egui::Button::new("Link account"))
                            .clicked()
                        {
                            cmd = Some(Command::ConnectGoogleAds {
                                customer_id: form.selected.clone().unwrap_or_default(),
                            });
                            close = true;
                        }
                        if ui.button("Use another Google account").clicked() {
                            cmd = Some(Command::GoogleSignIn);
                            form.selected = None;
                        }
                    });
                }
                S::Idle | S::Failed(_) => {
                    if let S::Failed(e) = &state.google_sign_in {
                        ui.colored_label(RED, e);
                    }
                    let button = egui::Button::new(RichText::new("Sign in with Google").strong())
                        .min_size(egui::vec2(220.0, 32.0));
                    if ui
                        .add_enabled(state.google_app_configured, button)
                        .on_disabled_hover_text("Save the app credentials first")
                        .clicked()
                    {
                        cmd = Some(Command::GoogleSignIn);
                    }
                }
            }
            ui.add_space(6.0);
            if ui.button("Close").clicked() {
                if waiting {
                    cmd = Some(Command::CancelGoogleSignIn);
                }
                close = true;
            }
        });
    (close, cmd)
}

fn render_item(
    ui: &mut egui::Ui,
    idx: usize,
    item: &ChatItem,
    answers: &mut Vec<(usize, bool)>,
    markdown: &mut CommonMarkCache,
) {
    match item {
        ChatItem::User(text) => {
            ui.label(RichText::new("You").strong());
            ui.label(text);
        }
        ChatItem::Assistant(text) => {
            ui.label(RichText::new("merchantGPT").strong().color(BLUE));
            render_markdown(ui, idx, text, markdown);
        }
        ChatItem::Tool {
            name,
            arguments,
            mutability,
            status,
            ..
        } => {
            let (badge, color) = match status {
                ToolStatus::Running => ("running", Color32::GRAY),
                ToolStatus::Ok(_) => ("done", GREEN),
                ToolStatus::Failed(_) => ("failed", RED),
            };
            let kind = match mutability {
                Some(Mutability::Mutate) => " (change)",
                _ => "",
            };
            egui::CollapsingHeader::new(
                RichText::new(format!("tool {name}{kind} · {badge}")).color(color),
            )
            .id_salt(("tool", idx))
            .show(ui, |ui| {
                ui.label(RichText::new(arguments).monospace());
                if let ToolStatus::Ok(out) | ToolStatus::Failed(out) = status {
                    ui.separator();
                    ui.label(RichText::new(out).monospace().small());
                }
            });
        }
        ChatItem::Confirmation {
            call,
            description,
            state,
            ..
        } => {
            egui::Frame::group(ui.style())
                .stroke(egui::Stroke::new(1.5, AMBER))
                .show(ui, |ui| {
                    ui.label(RichText::new(format!("Confirm change: {}", call.name)).strong());
                    ui.weak(description);
                    ui.label(RichText::new(pretty_args(&call.arguments)).monospace());
                    match state {
                        CardState::Pending => {
                            ui.horizontal(|ui| {
                                if ui.button("Approve").clicked() {
                                    answers.push((idx, true));
                                }
                                if ui.button("Deny").clicked() {
                                    answers.push((idx, false));
                                }
                            });
                        }
                        CardState::Approved => {
                            ui.colored_label(GREEN, "Approved");
                        }
                        CardState::Denied => {
                            ui.colored_label(RED, "Denied");
                        }
                    }
                });
        }
        ChatItem::Notice(text) => {
            ui.label(RichText::new(text).italics().weak());
        }
        ChatItem::Error(text) => {
            ui.colored_label(RED, text);
        }
    }
}

/// Assistant replies are Markdown (tables, lists, code). Each message gets its
/// own id so tables and code blocks of different messages do not collide.
fn render_markdown(ui: &mut egui::Ui, idx: usize, text: &str, cache: &mut CommonMarkCache) {
    let id = ui.scope_id().with(("assistant_md", idx));
    markdown::render(ui, id, text, cache);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bedrock_form_builds_options_per_auth_mode() {
        let mut f = SettingsForm {
            provider: ProviderId::Bedrock,
            region: " eu-west-1 ".into(),
            access_key_id: "AKIA1".into(),
            session_token: "tok".into(),
            ..Default::default()
        };
        let o = f.options();
        assert_eq!(o.len(), 1, "API key mode sends only the region");
        assert_eq!(o[llm::OPT_REGION], "eu-west-1");
        f.aws_access_keys = true;
        let o = f.options();
        assert_eq!(o[llm::OPT_AWS_ACCESS_KEY_ID], "AKIA1");
        assert_eq!(o[llm::OPT_AWS_SESSION_TOKEN], "tok");
        assert_eq!(
            f.base_url_hint(),
            "https://bedrock-mantle.eu-west-1.api.aws/anthropic"
        );
        f.provider = ProviderId::OpenAi;
        assert!(f.options().is_empty());
    }

    fn draw_google_window(state: &AppState, form: &mut GoogleAdsForm) -> (bool, Option<Command>) {
        let ctx = egui::Context::default();
        let mut result = (false, None);
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                result = google_ads_window(ui.ctx(), form, state);
            });
            out.textures_delta.clear();
        }
        result
    }

    #[test]
    fn google_ads_window_renders_every_sign_in_state() {
        use chat_core::GoogleSignInState as S;
        use connectors::google_ads::AdsAccount;
        let mut state = AppState::default();
        let mut form = GoogleAdsForm {
            show_setup: true,
            ..Default::default()
        };
        let (close, cmd) = draw_google_window(&state, &mut form);
        assert!(!close && cmd.is_none());

        state.google_app_configured = true;
        for s in [S::Idle, S::Waiting, S::Failed("denied".into())] {
            state.google_sign_in = s;
            let (close, cmd) = draw_google_window(&state, &mut form);
            assert!(!close && cmd.is_none());
        }

        let account = |id: &str, manager: bool| AdsAccount {
            customer_id: id.into(),
            name: "Shop".into(),
            currency: "EUR".into(),
            manager,
            test_account: false,
            login_customer_id: None,
        };
        state.google_sign_in = S::Accounts(vec![
            account("1234567890", false),
            account("9999999999", true),
        ]);
        form.selected = Some("1234567890".into());
        draw_google_window(&state, &mut form);
        assert_eq!(form.selected.as_deref(), Some("1234567890"));
    }

    #[test]
    fn markdown_replies_render_headless() {
        let reply = "## Orders\n\n| # | Customer | Total |\n|---|---|---|\n| 727 | Ana | **84.50 EUR** |\n\n- one\n- two\n\n```\ncode\n```\n\nPartial *stream";
        let ctx = egui::Context::default();
        let mut cache = CommonMarkCache::default();
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                render_markdown(ui, 0, reply, &mut cache);
                render_markdown(ui, 1, reply, &mut cache);
            });
            assert!(!out.shapes.is_empty(), "something was drawn");
            // Headless: there is no renderer to upload font textures to.
            out.textures_delta.clear();
        }
    }
}
