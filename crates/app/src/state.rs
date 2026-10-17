//! Headless UI state. Every backend event goes through `AppState::apply`, so
//! the chat's behaviour is unit-testable without a window.

use chat_core::{AgentEvent, BackendEvent, Command, Decision, Responder};
use connectors::ConnectorKind;
use llm::{ProviderId, Usage};
use tools::{CancellationToken, Mutability, ToolCall};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardState {
    Pending,
    Approved,
    Denied,
}

#[derive(Debug, Clone)]
pub enum ChatItem {
    User(String),
    Assistant(String),
    Tool {
        call_id: String,
        name: String,
        arguments: String,
        mutability: Mutability,
        status: ToolStatus,
    },
    Confirmation {
        call: ToolCall,
        description: String,
        responder: Responder,
        state: CardState,
    },
    Notice(String),
    Error(String),
}

#[derive(Default)]
pub struct AppState {
    pub items: Vec<ChatItem>,
    pub busy: bool,
    pub cancel: Option<CancellationToken>,
    pub usage: Usage,
    pub shop: Option<(String, ConnectorKind)>,
    pub llm: Option<(ProviderId, String)>,
}

pub fn pretty_args(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn preview(s: &str) -> String {
    const MAX: usize = 300;
    match s.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

impl AppState {
    /// Records the user's message and returns the command to send, or `None`
    /// while a turn is already running or the text is blank.
    pub fn begin_send(&mut self, text: &str) -> Option<Command> {
        let text = text.trim();
        if self.busy || text.is_empty() {
            return None;
        }
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.busy = true;
        self.items.push(ChatItem::User(text.to_string()));
        Some(Command::Send {
            text: text.to_string(),
            cancel,
        })
    }

    pub fn cancel_turn(&mut self) {
        if let Some(c) = &self.cancel {
            c.cancel();
        }
        for item in &mut self.items {
            if let ChatItem::Confirmation {
                responder, state, ..
            } = item
            {
                if *state == CardState::Pending {
                    responder.respond(Decision::Deny(Some("cancelled".into())));
                    *state = CardState::Denied;
                }
            }
        }
    }

    pub fn answer_confirmation(&mut self, index: usize, approve: bool) {
        if let Some(ChatItem::Confirmation {
            responder, state, ..
        }) = self.items.get_mut(index)
        {
            if *state != CardState::Pending {
                return;
            }
            if approve {
                responder.respond(Decision::Approve);
                *state = CardState::Approved;
            } else {
                responder.respond(Decision::Deny(None));
                *state = CardState::Denied;
            }
        }
    }

    pub fn apply(&mut self, ev: BackendEvent) {
        match ev {
            BackendEvent::Agent(a) => self.apply_agent(a),
            BackendEvent::TurnEnded { error } => {
                self.busy = false;
                self.cancel = None;
                if let Some(e) = error {
                    self.items.push(ChatItem::Error(e));
                }
            }
            BackendEvent::Notice(n) => self.items.push(ChatItem::Notice(n)),
            BackendEvent::Error(e) => self.items.push(ChatItem::Error(e)),
            BackendEvent::ShopConnected { url, kind } => {
                self.items.push(ChatItem::Notice(format!(
                    "Connected {} at {url}.",
                    kind.display_name()
                )));
                self.shop = Some((url, kind));
            }
            BackendEvent::LlmConfigured { provider, model } => {
                self.llm = Some((provider, model));
            }
            BackendEvent::ConversationReset => {
                self.items.clear();
                self.usage = Usage::default();
            }
        }
    }

    fn apply_agent(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TextDelta(d) => match self.items.last_mut() {
                Some(ChatItem::Assistant(text)) => text.push_str(&d),
                _ => self.items.push(ChatItem::Assistant(d)),
            },
            AgentEvent::ConfirmationRequested {
                call,
                description,
                responder,
            } => self.items.push(ChatItem::Confirmation {
                call,
                description,
                responder,
                state: CardState::Pending,
            }),
            AgentEvent::ToolStarted { call, mutability } => self.items.push(ChatItem::Tool {
                call_id: call.id,
                name: call.name,
                arguments: pretty_args(&call.arguments),
                mutability,
                status: ToolStatus::Running,
            }),
            AgentEvent::ToolFinished { result, .. } => {
                let running = self.items.iter_mut().rev().find_map(|i| match i {
                    ChatItem::Tool {
                        call_id, status, ..
                    } if *call_id == result.call_id => Some(status),
                    _ => None,
                });
                let new_status = if result.is_error {
                    ToolStatus::Failed(preview(&result.content))
                } else {
                    ToolStatus::Ok(preview(&result.content))
                };
                // Denied mutations never started, so they have no running chip.
                if let Some(status) = running {
                    *status = new_status;
                }
            }
            AgentEvent::Usage(u) => {
                self.usage.input_tokens += u.input_tokens;
                self.usage.output_tokens += u.output_tokens;
            }
            AgentEvent::Finished { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tools::ToolResult;

    fn delta(s: &str) -> BackendEvent {
        BackendEvent::Agent(AgentEvent::TextDelta(s.into()))
    }

    #[test]
    fn app_state_appends_deltas_to_in_progress_message() {
        let mut s = AppState::default();
        assert!(s.begin_send("hi").is_some());
        assert!(s.begin_send("again").is_none(), "one turn at a time");
        s.apply(delta("Hel"));
        s.apply(delta("lo"));
        s.apply(BackendEvent::Agent(AgentEvent::ToolStarted {
            call: ToolCall {
                id: "c".into(),
                name: "shop_list_orders".into(),
                arguments: json!({}),
            },
            mutability: Mutability::Read,
        }));
        s.apply(BackendEvent::Agent(AgentEvent::ToolFinished {
            name: "shop_list_orders".into(),
            result: ToolResult::ok("c", "[]"),
        }));
        s.apply(delta("Done"));
        s.apply(BackendEvent::TurnEnded { error: None });

        let texts: Vec<_> = s
            .items
            .iter()
            .filter_map(|i| match i {
                ChatItem::Assistant(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Hello", "Done"]);
        assert!(
            matches!(&s.items[2], ChatItem::Tool { status: ToolStatus::Ok(o), .. } if o == "[]")
        );
        assert!(!s.busy);
    }

    #[test]
    fn pending_confirmation_resolves_oneshot() {
        let mut s = AppState::default();
        let (responder, mut rx) = Responder::new();
        s.apply(BackendEvent::Agent(AgentEvent::ConfirmationRequested {
            call: ToolCall {
                id: "c".into(),
                name: "shop_update_product".into(),
                arguments: json!({"id": "1", "regular_price": "9.90"}),
            },
            description: "Update product".into(),
            responder,
        }));
        s.answer_confirmation(0, true);
        s.answer_confirmation(0, false);
        assert_eq!(rx.try_recv().unwrap(), Decision::Approve);
        assert!(matches!(
            &s.items[0],
            ChatItem::Confirmation {
                state: CardState::Approved,
                ..
            }
        ));
    }

    #[test]
    fn cancel_denies_open_cards_and_triggers_token() {
        let mut s = AppState::default();
        s.begin_send("go");
        let token = s.cancel.clone().unwrap();
        let (responder, mut rx) = Responder::new();
        s.apply(BackendEvent::Agent(AgentEvent::ConfirmationRequested {
            call: ToolCall {
                id: "c".into(),
                name: "x".into(),
                arguments: json!({}),
            },
            description: String::new(),
            responder,
        }));
        s.cancel_turn();
        assert!(token.is_cancelled());
        assert!(matches!(rx.try_recv().unwrap(), Decision::Deny(_)));
    }

    #[test]
    fn turn_error_is_shown_and_releases_input() {
        let mut s = AppState::default();
        s.begin_send("go");
        s.apply(BackendEvent::TurnEnded {
            error: Some("boom".into()),
        });
        assert!(!s.busy);
        assert!(matches!(s.items.last(), Some(ChatItem::Error(e)) if e == "boom"));
    }
}
