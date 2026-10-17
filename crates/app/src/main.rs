//! Native desktop shell: chat view, input with the `+` connector menu, and the
//! shop connector flow (platform picker, then credential form from the schema).

use chat_core::{Credentials, Role, Session};
use connectors::Platform;
use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "merchantGPT",
        options,
        Box::new(|_cc| Ok(Box::new(App::default()))),
    )
}

#[derive(Default)]
enum ConnectorFlow {
    #[default]
    Closed,
    PickConnector,
    PickPlatform,
    EnterCredentials {
        platform: Platform,
        values: Credentials,
    },
}

#[derive(Default)]
struct App {
    shop_url: String,
    session: Option<Session>,
    input: String,
    flow: ConnectorFlow,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| match self.session.as_mut() {
            None => self.shop_url_prompt(ui),
            Some(_) => {
                self.chat(ui);
                self.connector_flow(ui);
            }
        });
    }
}

impl App {
    fn shop_url_prompt(&mut self, ui: &mut egui::Ui) {
        ui.heading("merchantGPT");
        ui.label("Enter your shop URL to start.");
        ui.text_edit_singleline(&mut self.shop_url);
        if ui.button("Start").clicked() && !self.shop_url.trim().is_empty() {
            self.session = Some(Session::new(self.shop_url.trim()));
        }
    }

    fn chat(&mut self, ui: &mut egui::Ui) {
        let session = self.session.as_mut().expect("chat requires a session");
        ui.heading(&session.shop_url);
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(ui.available_height() - 60.0)
            .show(ui, |ui| {
                for m in &session.history {
                    let who = match m.role {
                        Role::User => "You",
                        Role::Assistant => "merchantGPT",
                    };
                    ui.label(format!("{who}: {}", m.text));
                }
            });
        ui.horizontal(|ui| {
            if ui.button("+").on_hover_text("Add a connector").clicked() {
                self.flow = ConnectorFlow::PickConnector;
            }
            let response = ui.text_edit_multiline(&mut self.input);
            let submitted = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Send").clicked() || submitted) && !self.input.trim().is_empty() {
                session.handle_message(self.input.trim());
                self.input.clear();
            }
        });
    }

    fn connector_flow(&mut self, ui: &mut egui::Ui) {
        let mut next: Option<ConnectorFlow> = None;
        match &mut self.flow {
            ConnectorFlow::Closed => {}
            ConnectorFlow::PickConnector => {
                egui::Window::new("Connectors").show(ui.ctx(), |ui| {
                    if ui.button("Shop").clicked() {
                        next = Some(ConnectorFlow::PickPlatform);
                    }
                    if ui.button("Cancel").clicked() {
                        next = Some(ConnectorFlow::Closed);
                    }
                });
            }
            ConnectorFlow::PickPlatform => {
                egui::Window::new("Choose your platform").show(ui.ctx(), |ui| {
                    for p in Platform::ALL {
                        if ui.button(p.display_name()).clicked() {
                            next = Some(ConnectorFlow::EnterCredentials {
                                platform: p,
                                values: Credentials::new(),
                            });
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        next = Some(ConnectorFlow::Closed);
                    }
                });
            }
            ConnectorFlow::EnterCredentials { platform, values } => {
                let schema = platform.credential_schema();
                let title = format!("{} credentials", platform.display_name());
                egui::Window::new(title).show(ui.ctx(), |ui| {
                    for field in schema.fields() {
                        ui.label(field.label);
                        let value = values.entry(field.key.to_string()).or_default();
                        ui.add(egui::TextEdit::singleline(value).password(field.secret));
                    }
                    let complete = schema
                        .fields()
                        .iter()
                        .all(|f| values.get(f.key).is_some_and(|v| !v.trim().is_empty()));
                    if ui
                        .add_enabled(complete, egui::Button::new("Connect"))
                        .clicked()
                    {
                        if let Some(session) = self.session.as_mut() {
                            session.connect_shop(*platform, values.clone());
                        }
                        next = Some(ConnectorFlow::Closed);
                    }
                    if ui.button("Cancel").clicked() {
                        next = Some(ConnectorFlow::Closed);
                    }
                });
            }
        }
        if let Some(n) = next {
            self.flow = n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_starts_without_session_and_with_flow_closed() {
        let app = App::default();
        assert!(app.session.is_none());
        assert!(matches!(app.flow, ConnectorFlow::Closed));
    }
}
