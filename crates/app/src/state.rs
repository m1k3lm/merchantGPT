//! Headless UI state. Every backend event goes through `AppState::apply`, so
//! the chat's behaviour is unit-testable without a window.

use chat_core::{
    AgentEvent, AuditSummary, BackendEvent, Command, ConversationSummary, Decision,
    LlmSettingSummary, Responder, ShopSummary, SkillSummary, TaskSummary,
};
use llm::{ChatMessage, ProviderId, Usage};
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
        /// Unknown for calls restored from history.
        mutability: Option<Mutability>,
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
    pub llm: Option<(ProviderId, String)>,
    pub llm_settings: Vec<LlmSettingSummary>,
    pub shops: Vec<ShopSummary>,
    pub active_shop: Option<i64>,
    pub conversations: Vec<ConversationSummary>,
    pub active_conversation: Option<i64>,
    pub skills: Vec<SkillSummary>,
    pub tasks: Vec<TaskSummary>,
    pub audit: Vec<AuditSummary>,
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

fn status_of(content: &str, is_error: bool) -> ToolStatus {
    if is_error {
        ToolStatus::Failed(preview(content))
    } else {
        ToolStatus::Ok(preview(content))
    }
}

/// Rebuilds the transcript of a saved conversation. Confirmation cards are
/// not restored; their calls show up as tool entries with their outcome.
pub fn items_from_history(messages: &[ChatMessage]) -> Vec<ChatItem> {
    let mut items = Vec::new();
    for m in messages {
        match m {
            ChatMessage::System { .. } => {}
            ChatMessage::User { text } => items.push(ChatItem::User(text.clone())),
            ChatMessage::Assistant {
                text, tool_calls, ..
            } => {
                if !text.is_empty() {
                    items.push(ChatItem::Assistant(text.clone()));
                }
                for c in tool_calls {
                    items.push(ChatItem::Tool {
                        call_id: c.id.clone(),
                        name: c.name.clone(),
                        arguments: pretty_args(&c.arguments),
                        mutability: None,
                        status: ToolStatus::Running,
                    });
                }
            }
            ChatMessage::ToolResults { results } => {
                for r in results {
                    set_tool_status(&mut items, &r.call_id, status_of(&r.content, r.is_error));
                }
            }
        }
    }
    items
}

fn set_tool_status(items: &mut [ChatItem], id: &str, new_status: ToolStatus) {
    let found = items.iter_mut().rev().find_map(|i| match i {
        ChatItem::Tool {
            call_id, status, ..
        } if call_id == id => Some(status),
        _ => None,
    });
    if let Some(status) = found {
        *status = new_status;
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

    /// Skills matching a `/prefix` being typed, before the first space.
    pub fn skill_suggestions(&self, input: &str) -> Vec<&SkillSummary> {
        let Some(prefix) = input.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.contains(char::is_whitespace) {
            return Vec::new();
        }
        self.skills
            .iter()
            .filter(|s| s.name.starts_with(prefix))
            .collect()
    }

    pub fn active_shop(&self) -> Option<&ShopSummary> {
        self.shops.iter().find(|s| Some(s.id) == self.active_shop)
    }

    pub fn saved_llm(&self, provider: ProviderId) -> Option<&LlmSettingSummary> {
        self.llm_settings.iter().find(|s| s.provider == provider)
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
            BackendEvent::LlmConfigured { provider, model } => self.llm = Some((provider, model)),
            BackendEvent::LlmSettings(s) => self.llm_settings = s,
            BackendEvent::Skills(s) => self.skills = s,
            BackendEvent::Tasks(t) => self.tasks = t,
            BackendEvent::Audit(rows) => self.audit = rows,
            BackendEvent::TaskFinished { name, error } => self.items.push(match error {
                None => ChatItem::Notice(format!(
                    "Scheduled task \"{name}\" finished; its report is in the \"Task: {name}\" chat."
                )),
                Some(e) => ChatItem::Error(format!("Scheduled task \"{name}\" failed: {e}")),
            }),
            BackendEvent::Shops { shops, active } => {
                self.shops = shops;
                self.active_shop = active;
            }
            BackendEvent::Conversations { items, active } => {
                self.conversations = items;
                self.active_conversation = active;
            }
            BackendEvent::ConversationOpened {
                id,
                messages,
                usage,
            } => {
                // Notices emitted just before (e.g. "Connected …") stay visible.
                let notices: Vec<_> = self
                    .items
                    .drain(..)
                    .rev()
                    .take_while(|i| matches!(i, ChatItem::Notice(_) | ChatItem::Error(_)))
                    .collect();
                self.items = items_from_history(&messages);
                self.items.extend(notices.into_iter().rev());
                self.active_conversation = id;
                self.usage = usage;
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
                mutability: Some(mutability),
                status: ToolStatus::Running,
            }),
            AgentEvent::ToolFinished { result, .. } => {
                // Denied mutations never started, so they have no running entry.
                set_tool_status(
                    &mut self.items,
                    &result.call_id,
                    status_of(&result.content, result.is_error),
                );
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

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    #[test]
    fn app_state_appends_deltas_to_in_progress_message() {
        let mut s = AppState::default();
        assert!(s.begin_send("hi").is_some());
        assert!(s.begin_send("again").is_none(), "one turn at a time");
        s.apply(delta("Hel"));
        s.apply(delta("lo"));
        s.apply(BackendEvent::Agent(AgentEvent::ToolStarted {
            call: call("c", "shop_list_orders"),
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
            call: call("c", "x"),
            description: String::new(),
            responder,
        }));
        s.cancel_turn();
        assert!(token.is_cancelled());
        assert!(matches!(rx.try_recv().unwrap(), Decision::Deny(_)));
    }

    #[test]
    fn skill_suggestions_follow_the_slash_prefix() {
        let mut s = AppState::default();
        s.apply(BackendEvent::Skills(vec![
            SkillSummary {
                name: "daily-sales".into(),
                description: String::new(),
            },
            SkillSummary {
                name: "restock".into(),
                description: String::new(),
            },
        ]));
        assert_eq!(s.skill_suggestions("/").len(), 2);
        assert_eq!(s.skill_suggestions("/da")[0].name, "daily-sales");
        assert!(s.skill_suggestions("/daily-sales now").is_empty());
        assert!(s.skill_suggestions("hello").is_empty());
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

    #[test]
    fn opened_conversation_rebuilds_transcript_and_keeps_trailing_notices() {
        let mut s = AppState::default();
        s.items.push(ChatItem::User("old".into()));
        s.items.push(ChatItem::Notice("Connected".into()));
        s.apply(BackendEvent::ConversationOpened {
            id: Some(3),
            messages: vec![
                ChatMessage::user("orders?"),
                ChatMessage::assistant_with_calls("Checking", vec![call("a", "shop_list_orders")]),
                ChatMessage::ToolResults {
                    results: vec![ToolResult::error("a", "nope")],
                },
                ChatMessage::assistant("Failed"),
            ],
            usage: Usage {
                input_tokens: 5,
                output_tokens: 1,
            },
        });
        assert_eq!(s.items.len(), 5);
        assert!(
            matches!(&s.items[2], ChatItem::Tool { status: ToolStatus::Failed(e), .. } if e == "nope")
        );
        assert!(matches!(&s.items[4], ChatItem::Notice(n) if n == "Connected"));
        assert_eq!(s.active_conversation, Some(3));
        assert_eq!(s.usage.input_tokens, 5);
    }
}
