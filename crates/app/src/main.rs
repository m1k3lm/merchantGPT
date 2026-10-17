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
            .with_inner_size([1000.0, 720.0])
            .with_title("merchantGPT"),
        ..Default::default()
    };
    eframe::run_native(
        "merchantGPT",
        options,
        Box::new(|cc| {
            let runtime = Runtime::start(cc.egui_ctx.clone())?;
            Ok(Box::new(App::new(runtime)))
        }),
    )
}

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
            model: ProviderId::Anthropic.suggested_model().into(),
            base_url: String::new(),
            api_key: String::new(),
        }
    }
}

struct App {
    runtime: Runtime,
    state: AppState,
    input: String,
    flow: ConnectorFlow,
    settings: SettingsForm,
}

impl App {
    fn new(runtime: Runtime) -> Self {
        Self {
            runtime,
            state: AppState::default(),
            input: String::new(),
            flow: ConnectorFlow::Closed,
            settings: SettingsForm::default(),
        }
    }

    fn send_input(&mut self) {
        if let Some(cmd) = self.state.begin_send(&self.input) {
            self.runtime.send(cmd);
            self.input.clear();
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        for ev in self.runtime.drain().collect::<Vec<_>>() {
            self.state.apply(ev);
        }
        self.top_bar(ui);
        self.input_bar(ui);
        egui::CentralPanel::default().show(ui, |ui| self.transcript(ui));
        self.settings_window(ui.ctx());
        self.connector_window(ui.ctx());
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("merchantGPT");
                ui.separator();
                match &self.state.shop {
                    Some((url, kind)) => ui.label(format!("{} · {url}", kind.display_name())),
                    None => ui.weak("No shop connected"),
                };
                ui.separator();
                match &self.state.llm {
                    Some((p, m)) => ui.label(format!("{} · {m}", p.display_name())),
                    None => ui.weak("No AI provider"),
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.settings.open = true;
                    }
                    if ui
                        .add_enabled(!self.state.busy, egui::Button::new("New chat"))
                        .clicked()
                    {
                        self.runtime.send(Command::NewConversation);
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

    fn input_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("input_bar").show(ui, |ui| {
            ui.add_space(6.0);
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
                    ui.weak("Connect your shop with the + button and configure an AI provider in Settings, then ask anything about your store.");
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
        let mut open = self.settings.open;
        let mut save = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                let s = &mut self.settings;
                egui::Grid::new("llm_settings")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("AI provider");
                        let before = s.provider;
                        egui::ComboBox::from_id_salt("provider")
                            .selected_text(s.provider.display_name())
                            .show_ui(ui, |ui| {
                                for p in ProviderId::ALL {
                                    ui.selectable_value(&mut s.provider, p, p.display_name());
                                }
                            });
                        if s.provider != before {
                            s.model = s.provider.suggested_model().into();
                        }
                        ui.end_row();
                        ui.label("Model");
                        ui.text_edit_singleline(&mut s.model);
                        ui.end_row();
                        ui.label("API key");
                        ui.add(egui::TextEdit::singleline(&mut s.api_key).password(true));
                        ui.end_row();
                        ui.label("Base URL");
                        ui.add(
                            egui::TextEdit::singleline(&mut s.base_url)
                                .hint_text(s.provider.default_base_url()),
                        );
                        ui.end_row();
                    });
                let ready = !s.model.trim().is_empty() && !s.api_key.trim().is_empty();
                if ui.add_enabled(ready, egui::Button::new("Save")).clicked() {
                    save = true;
                }
            });
        if save {
            let s = &self.settings;
            self.runtime.send(Command::SetLlm(ProviderConfig {
                provider: s.provider,
                api_key: s.api_key.trim().to_string(),
                base_url: Some(s.base_url.trim().to_string()).filter(|u| !u.is_empty()),
                model: s.model.trim().to_string(),
            }));
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
                                    url: self
                                        .state
                                        .shop
                                        .as_ref()
                                        .map(|(u, _)| u.clone())
                                        .unwrap_or_default(),
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
            self.runtime.send(cmd);
        }
        if let Some(n) = next {
            self.flow = n;
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
            ui.label(
                RichText::new("merchantGPT")
                    .strong()
                    .color(Color32::from_rgb(70, 130, 180)),
            );
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
                ToolStatus::Ok(_) => ("done", Color32::from_rgb(60, 150, 60)),
                ToolStatus::Failed(_) => ("failed", Color32::from_rgb(200, 60, 60)),
            };
            let kind = match mutability {
                Mutability::Read => "read",
                Mutability::Mutate => "change",
            };
            egui::CollapsingHeader::new(
                RichText::new(format!("tool {name} ({kind}) · {badge}")).color(color),
            )
            .id_salt(("tool", idx))
            .show(ui, |ui| {
                ui.label(RichText::new(arguments).monospace());
                match status {
                    ToolStatus::Ok(out) | ToolStatus::Failed(out) => {
                        ui.separator();
                        ui.label(RichText::new(out).monospace().small());
                    }
                    ToolStatus::Running => {}
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
                .stroke(egui::Stroke::new(1.5, Color32::from_rgb(220, 160, 40)))
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
                            ui.colored_label(Color32::from_rgb(60, 150, 60), "Approved");
                        }
                        CardState::Denied => {
                            ui.colored_label(Color32::from_rgb(200, 60, 60), "Denied");
                        }
                    }
                });
        }
        ChatItem::Notice(text) => {
            ui.label(RichText::new(text).italics().weak());
        }
        ChatItem::Error(text) => {
            ui.colored_label(Color32::from_rgb(200, 60, 60), text);
        }
    }
}
