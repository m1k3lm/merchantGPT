//! Native desktop shell. Rendering only: behaviour lives in `state::AppState`
//! and the backend in `chat-core`.

mod runtime;
mod state;

use chat_core::{Command, Credentials};
use connectors::{ConnectorKind, ConnectorRegistry, Platform};
use eframe::egui::{self, Color32, RichText};
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
}

struct SettingsForm {
    open: bool,
    provider: ProviderId,
    model: String,
    base_url: String,
    api_key: String,
}

impl Default for SettingsForm {
    fn default() -> Self {
        Self {
            open: false,
            provider: ProviderId::Anthropic,
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
        }
    }
}

struct App {
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
        match self.state.saved_llm(provider) {
            Some(saved) => {
                s.model = saved.model.clone();
                s.base_url = saved.base_url.clone().unwrap_or_default();
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
                    let resp = ui
                        .add_enabled(
                            !self.state.busy,
                            egui::Button::selectable(active, shop.label()),
                        )
                        .on_hover_text(&shop.url);
                    if resp.clicked() && !active {
                        cmds.push(Command::SelectShop(shop.id));
                    }
                    resp.context_menu(|ui| {
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
                    render_item(ui, idx, item, &mut answers);
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
        let has_saved_key = self.state.saved_llm(self.settings.provider).is_some();
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
                        ui.label("API key");
                        let hint = if has_saved_key {
                            "saved; leave blank to keep"
                        } else {
                            ""
                        };
                        ui.add(
                            egui::TextEdit::singleline(&mut s.api_key)
                                .password(true)
                                .hint_text(hint),
                        );
                        ui.end_row();
                        ui.label("Base URL");
                        ui.add(
                            egui::TextEdit::singleline(&mut s.base_url)
                                .hint_text(s.provider.default_base_url()),
                        );
                        ui.end_row();
                    });
                ui.weak("Keys are stored in the encrypted local database, never in config.toml.");
                let ready =
                    !s.model.trim().is_empty() && (has_saved_key || !s.api_key.trim().is_empty());
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
                provider: s.provider,
                api_key: s.api_key.trim().to_string(),
                base_url: Some(s.base_url.trim().to_string()).filter(|u| !u.is_empty()),
                model: s.model.trim().to_string(),
            }));
            self.settings.api_key.clear();
            open = false;
        }
        self.settings.open = open;
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
                        ui.weak("More connectors (ads, carriers) are coming.");
                        if ui.button("Cancel").clicked() {
                            next = Some(ConnectorFlow::Closed);
                        }
                    });
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

fn render_item(ui: &mut egui::Ui, idx: usize, item: &ChatItem, answers: &mut Vec<(usize, bool)>) {
    match item {
        ChatItem::User(text) => {
            ui.label(RichText::new("You").strong());
            ui.label(text);
        }
        ChatItem::Assistant(text) => {
            ui.label(RichText::new("merchantGPT").strong().color(BLUE));
            ui.label(text);
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
