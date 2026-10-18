//! The agent loop: model -> tool calls -> results -> model, until the model
//! answers without tools or the iteration budget runs out.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use llm::{
    ChatMessage, ChatRequest, LlmError, LlmProvider, ProviderContent, StopReason, StreamEvent,
    ToolChoice, Usage,
};
use tokio::sync::oneshot;
use tokio::time::Instant;
use tools::{Mutability, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult};

use crate::redact::Redactor;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub max_iterations: u32,
    pub max_tokens: u32,
    pub max_tool_result_chars: usize,
    pub turn_timeout: Duration,
}

impl AgentConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_iterations: 10,
            max_tokens: 16_000,
            max_tool_result_chars: 20_000,
            turn_timeout: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny(Option<String>),
}

/// One-shot reply channel for a confirmation card. Cloneable so it can travel
/// inside UI events; only the first `respond` takes effect.
#[derive(Clone)]
pub struct Responder(Arc<Mutex<Option<oneshot::Sender<Decision>>>>);

impl Responder {
    pub fn new() -> (Self, oneshot::Receiver<Decision>) {
        let (tx, rx) = oneshot::channel();
        (Self(Arc::new(Mutex::new(Some(tx)))), rx)
    }

    pub fn respond(&self, decision: Decision) {
        if let Some(tx) = self.0.lock().unwrap().take() {
            let _ = tx.send(decision);
        }
    }

    pub fn is_pending(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }
}

impl std::fmt::Debug for Responder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Responder(pending: {})", self.is_pending())
    }
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    TextDelta(String),
    /// A mutating call waiting for the user. The card must show `call.arguments`
    /// verbatim, never a paraphrase.
    ConfirmationRequested {
        call: ToolCall,
        description: String,
        responder: Responder,
    },
    ToolStarted {
        call: ToolCall,
        mutability: Mutability,
    },
    ToolFinished {
        name: String,
        result: ToolResult,
    },
    Usage(Usage),
    Finished {
        text: String,
    },
}

pub type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

#[async_trait]
pub trait ConfirmationGate: Send + Sync {
    async fn confirm(&self, call: &ToolCall, def: &ToolDefinition) -> Decision;

    /// Whether the gate may decide unattended runs. Only a gate built from
    /// the merchant's standing permission does; every other gate is replaced
    /// by a denial there.
    fn decides_unattended(&self) -> bool {
        false
    }

    /// Who approved, for the audit log when it was not the merchant live.
    fn approved_by(&self) -> Option<String> {
        None
    }
}

pub struct AlwaysApprove;

#[async_trait]
impl ConfirmationGate for AlwaysApprove {
    async fn confirm(&self, _: &ToolCall, _: &ToolDefinition) -> Decision {
        Decision::Approve
    }
}

/// Used for scheduled and other unattended runs: nothing mutates without a human.
pub struct AutoDeny;

#[async_trait]
impl ConfirmationGate for AutoDeny {
    async fn confirm(&self, _: &ToolCall, _: &ToolDefinition) -> Decision {
        Decision::Deny(Some(
            "unattended runs cannot change the shop; tell the merchant what you would do".into(),
        ))
    }
}

/// A scheduled task's unattended runs: approves only the changing tools and
/// `tool:action` pairs the merchant allowed when creating the task, so data
/// read during the run cannot steer it into anything else.
pub struct TaskGate {
    pub task: String,
    pub allowed: Vec<String>,
}

impl TaskGate {
    pub fn allows(&self, call: &ToolCall) -> bool {
        let action = call
            .arguments
            .get("action")
            .and_then(serde_json::Value::as_str);
        self.allowed.iter().any(|a| match a.split_once(':') {
            None => *a == call.name,
            Some((tool, act)) => tool == call.name && Some(act) == action,
        })
    }
}

#[async_trait]
impl ConfirmationGate for TaskGate {
    async fn confirm(&self, call: &ToolCall, _: &ToolDefinition) -> Decision {
        if self.allows(call) {
            return Decision::Approve;
        }
        let allowed = if self.allowed.is_empty() {
            "change nothing".to_string()
        } else {
            format!("use only {}", self.allowed.join(", "))
        };
        Decision::Deny(Some(format!(
            "this scheduled task may {allowed}; tell the merchant what you would do"
        )))
    }

    fn decides_unattended(&self) -> bool {
        true
    }

    fn approved_by(&self) -> Option<String> {
        Some(format!("allowed by scheduled task \"{}\"", self.task))
    }
}

/// Asks the UI through the event sink and waits for the card's answer. A
/// dropped card counts as a denial.
pub struct UiGate {
    sink: EventSink,
}

impl UiGate {
    pub fn new(sink: EventSink) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl ConfirmationGate for UiGate {
    async fn confirm(&self, call: &ToolCall, def: &ToolDefinition) -> Decision {
        let (responder, rx) = Responder::new();
        (self.sink)(AgentEvent::ConfirmationRequested {
            call: call.clone(),
            description: def.description.clone(),
            responder,
        });
        rx.await.unwrap_or(Decision::Deny(None))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditStage {
    Denied(Option<String>),
    /// Carries who approved when it was not the merchant at the card.
    Approved(Option<String>),
    Succeeded(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    pub tool: String,
    pub arguments: serde_json::Value,
    pub stage: AuditStage,
}

/// Receives one entry when a mutation is decided and one when it completes.
pub trait AuditSink: Send + Sync {
    fn record(&self, entry: AuditEntry);
}

pub struct NoAudit;

impl AuditSink for NoAudit {
    fn record(&self, _: AuditEntry) {}
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error("cancelled")]
    Cancelled,
    #[error("the turn took too long and was stopped")]
    TimedOut,
    #[error("the AI provider declined this request; rephrase it or try another model")]
    Refused,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnOutcome {
    pub text: String,
    pub usage: Usage,
    pub iterations: u32,
}

pub struct Agent<'a> {
    pub provider: &'a dyn LlmProvider,
    pub tools: &'a ToolRegistry,
    pub gate: &'a dyn ConfirmationGate,
    pub audit: &'a dyn AuditSink,
    pub sink: EventSink,
    pub config: &'a AgentConfig,
    pub redactor: Redactor,
}

const FORCED_ANSWER_NOTE: &str = "\n\nThe tool budget for this turn is exhausted. Answer the merchant now with what you already know, and say what is still missing.";

struct Reply {
    text: String,
    calls: Vec<ToolCall>,
    provider_content: Option<serde_json::Value>,
    stop: StopReason,
}

enum Interrupt {
    Cancelled,
    TimedOut,
}

impl From<Interrupt> for AgentError {
    fn from(i: Interrupt) -> Self {
        match i {
            Interrupt::Cancelled => AgentError::Cancelled,
            Interrupt::TimedOut => AgentError::TimedOut,
        }
    }
}

impl Agent<'_> {
    /// Runs one user turn. `history` must already end with the user's message;
    /// assistant and tool-result messages are appended to it as the turn runs,
    /// and it is left consistent (every tool call answered) even on error.
    pub async fn run_turn(
        &self,
        system_prompt: &str,
        history: &mut Vec<ChatMessage>,
        ctx: &ToolContext,
    ) -> Result<TurnOutcome, AgentError> {
        let deadline = Instant::now() + self.config.turn_timeout;
        let mut usage = Usage::default();
        for iteration in 0..=self.config.max_iterations {
            let forced = iteration == self.config.max_iterations;
            let system = if forced {
                format!("{system_prompt}{FORCED_ANSWER_NOTE}")
            } else {
                system_prompt.to_string()
            };
            let req = ChatRequest {
                model: self.config.model.clone(),
                messages: std::iter::once(ChatMessage::system(system))
                    .chain(history.iter().cloned())
                    .collect(),
                tools: self.tools.definitions(),
                tool_choice: if forced {
                    ToolChoice::None
                } else {
                    ToolChoice::Auto
                },
                max_tokens: self.config.max_tokens,
                temperature: None,
            };

            let reply = match self.stream_reply(req, ctx, deadline, &mut usage).await {
                Ok(reply) => reply,
                Err((partial, e)) => {
                    if !partial.is_empty() {
                        history.push(ChatMessage::assistant(partial));
                    }
                    return Err(e);
                }
            };
            // Partial output from a declined request is not trustworthy.
            if reply.stop == StopReason::Refusal {
                return Err(AgentError::Refused);
            }
            let Reply {
                text,
                mut calls,
                mut provider_content,
                stop,
            } = reply;
            if forced && !calls.is_empty() {
                calls.clear();
                provider_content = None;
            }
            history.push(ChatMessage::Assistant {
                text: text.clone(),
                tool_calls: calls.clone(),
                provider_content: provider_content.map(|content| ProviderContent {
                    provider: self.provider.id(),
                    content,
                }),
            });
            if calls.is_empty() {
                (self.sink)(AgentEvent::Finished { text: text.clone() });
                return Ok(TurnOutcome {
                    text,
                    usage,
                    iterations: iteration + 1,
                });
            }

            if stop == StopReason::MaxTokens {
                // A call cut off by the output limit may carry truncated arguments.
                let results = calls
                    .iter()
                    .map(|c| {
                        ToolResult::error(
                            &c.id,
                            "not run: the reply hit the output token limit before this call was complete; retry with a smaller request",
                        )
                    })
                    .collect();
                history.push(ChatMessage::ToolResults { results });
                continue;
            }

            let mut results = Vec::with_capacity(calls.len());
            for call in &calls {
                match self.execute(call, ctx, deadline).await {
                    Ok(r) => results.push(r),
                    Err(i) => {
                        let reason = match i {
                            Interrupt::Cancelled => "cancelled by the user",
                            Interrupt::TimedOut => "stopped: turn time limit reached",
                        };
                        for pending in &calls[results.len()..] {
                            results.push(ToolResult::error(&pending.id, reason));
                        }
                        history.push(ChatMessage::ToolResults { results });
                        return Err(i.into());
                    }
                }
            }
            history.push(ChatMessage::ToolResults { results });
        }
        unreachable!("the forced final iteration always returns")
    }

    async fn guarded<F: std::future::Future>(
        &self,
        ctx: &ToolContext,
        deadline: Instant,
        fut: F,
    ) -> Result<F::Output, Interrupt> {
        tokio::select! {
            out = fut => Ok(out),
            _ = ctx.cancel.cancelled() => Err(Interrupt::Cancelled),
            _ = tokio::time::sleep_until(deadline) => Err(Interrupt::TimedOut),
        }
    }

    /// Streams one model reply. On failure returns the text received so far.
    async fn stream_reply(
        &self,
        req: ChatRequest,
        ctx: &ToolContext,
        deadline: Instant,
        usage: &mut Usage,
    ) -> Result<Reply, (String, AgentError)> {
        let mut stream = match self.guarded(ctx, deadline, self.provider.stream(req)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err((String::new(), e.into())),
            Err(i) => return Err((String::new(), i.into())),
        };
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut provider_content = None;
        let mut stop = StopReason::EndTurn;
        loop {
            let next = match self.guarded(ctx, deadline, stream.next()).await {
                Ok(n) => n,
                Err(i) => return Err((text, i.into())),
            };
            match next {
                None => break,
                Some(Ok(StreamEvent::Done(s))) => {
                    stop = s;
                    break;
                }
                Some(Ok(StreamEvent::ProviderContent(c))) => provider_content = Some(c),
                Some(Ok(StreamEvent::TextDelta(d))) => {
                    text.push_str(&d);
                    (self.sink)(AgentEvent::TextDelta(d));
                }
                Some(Ok(StreamEvent::ToolCall(c))) => calls.push(c),
                Some(Ok(StreamEvent::Usage(u))) => {
                    usage.input_tokens += u.input_tokens;
                    usage.output_tokens += u.output_tokens;
                    (self.sink)(AgentEvent::Usage(u));
                }
                Some(Err(e)) => return Err((text, e.into())),
            }
        }
        Ok(Reply {
            text,
            calls,
            provider_content,
            stop,
        })
    }

    async fn execute(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
        deadline: Instant,
    ) -> Result<ToolResult, Interrupt> {
        let Some(tool) = self.tools.get(&call.name) else {
            return Ok(ToolResult::error(
                &call.id,
                format!("unknown tool {:?}", call.name),
            ));
        };
        if !call.arguments.is_object() {
            return Ok(ToolResult::error(
                &call.id,
                format!("arguments must be a JSON object, got: {}", call.arguments),
            ));
        }
        let def = tool.definition();
        let mutating = def.mutability == Mutability::Mutate;
        if mutating {
            let decision = if ctx.unattended && !self.gate.decides_unattended() {
                AutoDeny.confirm(call, &def).await
            } else if ctx.unattended {
                self.gate.confirm(call, &def).await
            } else {
                self.guarded(ctx, deadline, self.gate.confirm(call, &def))
                    .await?
            };
            if let Decision::Deny(reason) = decision {
                self.audit(call, AuditStage::Denied(reason.clone()));
                let msg = match reason {
                    Some(r) => format!("The merchant declined this action: {r}"),
                    None => "The merchant declined this action.".to_string(),
                };
                let result = ToolResult::error(&call.id, msg);
                (self.sink)(AgentEvent::ToolFinished {
                    name: call.name.clone(),
                    result: result.clone(),
                });
                return Ok(result);
            }
            self.audit(call, AuditStage::Approved(self.gate.approved_by()));
        }

        (self.sink)(AgentEvent::ToolStarted {
            call: call.clone(),
            mutability: def.mutability,
        });
        let outcome = self
            .guarded(ctx, deadline, tool.call(call.arguments.clone(), ctx))
            .await;
        let outcome = match outcome {
            Ok(o) => o,
            Err(i) => {
                if mutating {
                    self.audit(
                        call,
                        AuditStage::Failed("interrupted; the outcome is unknown".into()),
                    );
                }
                return Err(i);
            }
        };
        let result = match outcome {
            Ok(content) => {
                let content = truncate(
                    self.redactor.apply(&content),
                    self.config.max_tool_result_chars,
                );
                if mutating {
                    self.audit(call, AuditStage::Succeeded(truncate(content.clone(), 500)));
                }
                ToolResult::ok(&call.id, content)
            }
            Err(e) => {
                if mutating {
                    self.audit(call, AuditStage::Failed(e.to_string()));
                }
                ToolResult::error(&call.id, self.redactor.apply(&e.to_string()))
            }
        };
        (self.sink)(AgentEvent::ToolFinished {
            name: call.name.clone(),
            result: result.clone(),
        });
        Ok(result)
    }

    fn audit(&self, call: &ToolCall, stage: AuditStage) {
        let r = &self.redactor;
        let stage = match stage {
            AuditStage::Denied(reason) => AuditStage::Denied(reason.map(|x| r.apply(&x))),
            AuditStage::Approved(by) => AuditStage::Approved(by),
            AuditStage::Succeeded(x) => AuditStage::Succeeded(r.apply(&x)),
            AuditStage::Failed(x) => AuditStage::Failed(r.apply(&x)),
        };
        self.audit.record(AuditEntry {
            tool: call.name.clone(),
            arguments: call.arguments.clone(),
            stage,
        });
    }
}

fn truncate(mut s: String, max_chars: usize) -> String {
    if let Some((idx, _)) = s.char_indices().nth(max_chars) {
        let dropped = s[idx..].chars().count();
        s.truncate(idx);
        s.push_str(&format!(
            "\n[truncated {dropped} characters; narrow the query or use paging]"
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::fake::FakeProvider;
    use llm::StopReason;
    use serde_json::{json, Value};
    use tools::{Tool, ToolError};

    struct TestTool {
        name: &'static str,
        mutability: Mutability,
        output: &'static str,
    }

    #[async_trait]
    impl Tool for TestTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name.into(),
                description: format!("{} tool", self.name),
                parameters: json!({"type": "object"}),
                mutability: self.mutability,
            }
        }
        async fn call(&self, args: Value, _: &ToolContext) -> Result<String, ToolError> {
            if self.output == "hang" {
                futures::future::pending::<()>().await;
            }
            Ok(format!("{}:{}", self.output, args))
        }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(TestTool {
            name: "read_thing",
            mutability: Mutability::Read,
            output: "read",
        }));
        r.register(Arc::new(TestTool {
            name: "change_thing",
            mutability: Mutability::Mutate,
            output: "changed",
        }));
        r.register(Arc::new(TestTool {
            name: "slow_thing",
            mutability: Mutability::Read,
            output: "hang",
        }));
        r
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<AgentEvent>>,
        audit: Mutex<Vec<AuditEntry>>,
    }

    impl AuditSink for Recorder {
        fn record(&self, entry: AuditEntry) {
            self.audit.lock().unwrap().push(entry);
        }
    }

    struct Harness {
        provider: FakeProvider,
        tools: ToolRegistry,
        rec: Arc<Recorder>,
        config: AgentConfig,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                provider: FakeProvider::new(),
                tools: registry(),
                rec: Arc::new(Recorder::default()),
                config: AgentConfig::new("test-model"),
            }
        }

        async fn run(
            &self,
            gate: &dyn ConfirmationGate,
            history: &mut Vec<ChatMessage>,
            ctx: &ToolContext,
        ) -> Result<TurnOutcome, AgentError> {
            let rec = self.rec.clone();
            let agent = Agent {
                provider: &self.provider,
                tools: &self.tools,
                gate,
                audit: self.rec.as_ref(),
                sink: Arc::new(move |e| rec.events.lock().unwrap().push(e)),
                config: &self.config,
                redactor: Redactor::new(["s3cr3t-value".to_string()]),
            };
            agent.run_turn("SYSTEM", history, ctx).await
        }
    }

    fn user(text: &str) -> Vec<ChatMessage> {
        vec![ChatMessage::user(text)]
    }

    fn tool_results(history: &[ChatMessage]) -> Vec<ToolResult> {
        history
            .iter()
            .filter_map(|m| match m {
                ChatMessage::ToolResults { results } => Some(results.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[tokio::test]
    async fn loop_runs_tool_then_returns_final_answer() {
        let h = Harness::new();
        h.provider
            .push_tool_call("c1", "read_thing", json!({"q": 1}))
            .push_text("Here you go");
        let mut history = user("show me");
        let out = h
            .run(&AlwaysApprove, &mut history, &ToolContext::default())
            .await
            .unwrap();

        assert_eq!(out.text, "Here you go");
        assert_eq!(out.iterations, 2);
        assert_eq!(history.len(), 4);
        assert_eq!(
            tool_results(&history),
            vec![ToolResult::ok("c1", r#"read:{"q":1}"#)]
        );
        let reqs = h.provider.requests();
        assert_eq!(reqs[0].messages[0], ChatMessage::system("SYSTEM"));
        assert_eq!(reqs[0].tools.len(), 3);
        assert_eq!(
            reqs[1].messages.len(),
            4,
            "system + user + assistant + results"
        );
        assert!(
            h.rec.audit.lock().unwrap().is_empty(),
            "reads are not audited"
        );
    }

    #[tokio::test]
    async fn denied_mutation_feeds_error_result_back_to_model() {
        let h = Harness::new();
        h.provider
            .push_tool_call("c1", "change_thing", json!({"price": "9"}))
            .push_text("Okay, I left it unchanged.");
        let mut history = user("change it");
        let gate = UiGate::new(Arc::new(|e| {
            if let AgentEvent::ConfirmationRequested { responder, .. } = e {
                responder.respond(Decision::Deny(Some("too cheap".into())));
            }
        }));
        h.run(&gate, &mut history, &ToolContext::default())
            .await
            .unwrap();

        let results = tool_results(&history);
        assert!(results[0].is_error);
        assert_eq!(
            results[0].content,
            "The merchant declined this action: too cheap"
        );
        assert_eq!(
            *h.rec.audit.lock().unwrap(),
            vec![AuditEntry {
                tool: "change_thing".into(),
                arguments: json!({"price": "9"}),
                stage: AuditStage::Denied(Some("too cheap".into())),
            }]
        );
    }

    #[tokio::test]
    async fn approved_mutation_runs_and_is_audited_twice() {
        let h = Harness::new();
        h.provider
            .push_tool_call("c1", "change_thing", json!({}))
            .push_text("Done");
        let mut history = user("change it");
        h.run(&AlwaysApprove, &mut history, &ToolContext::default())
            .await
            .unwrap();
        let stages: Vec<_> = h
            .rec
            .audit
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.stage.clone())
            .collect();
        assert_eq!(
            stages,
            vec![
                AuditStage::Approved(None),
                AuditStage::Succeeded("changed:{}".into())
            ]
        );
    }

    #[tokio::test]
    async fn task_gates_allow_only_what_the_merchant_allowed() {
        let h = Harness::new();
        h.provider
            .push_tool_call("c1", "change_thing", json!({"action": "note"}))
            .push_tool_call("c2", "change_thing", json!({"action": "delete"}))
            .push_text("Noted; I would also delete it");
        let mut history = user("note it, then delete it");
        let ctx = ToolContext {
            unattended: true,
            ..Default::default()
        };
        let gate = TaskGate {
            task: "Nightly".into(),
            allowed: vec!["change_thing:note".into()],
        };
        h.run(&gate, &mut history, &ctx).await.unwrap();
        let results = tool_results(&history);
        assert!(!results[0].is_error, "{}", results[0].content);
        assert!(results[1].is_error);
        assert!(results[1]
            .content
            .contains("may use only change_thing:note"));
        let stages: Vec<_> = h
            .rec
            .audit
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.stage.clone())
            .collect();
        assert_eq!(
            stages[0],
            AuditStage::Approved(Some("allowed by scheduled task \"Nightly\"".into()))
        );
        assert!(matches!(stages.last(), Some(AuditStage::Denied(_))));
    }

    #[tokio::test]
    async fn unattended_runs_never_mutate_even_with_approving_gate() {
        let h = Harness::new();
        h.provider
            .push_tool_call("c1", "change_thing", json!({}))
            .push_text("I would change it");
        let mut history = user("change it");
        let ctx = ToolContext {
            unattended: true,
            ..Default::default()
        };
        h.run(&AlwaysApprove, &mut history, &ctx).await.unwrap();
        assert!(tool_results(&history)[0].is_error);
        assert!(matches!(
            h.rec.audit.lock().unwrap()[0].stage,
            AuditStage::Denied(_)
        ));
    }

    #[tokio::test]
    async fn stops_at_max_iterations_with_forced_answer() {
        let mut h = Harness::new();
        h.config.max_iterations = 2;
        h.provider
            .push_tool_call("c1", "read_thing", json!({}))
            .push_tool_call("c2", "read_thing", json!({}))
            .push_text("Partial answer");
        let mut history = user("loop forever");
        let out = h
            .run(&AlwaysApprove, &mut history, &ToolContext::default())
            .await
            .unwrap();
        assert_eq!(out.text, "Partial answer");
        assert_eq!(out.iterations, 3);
        let last = h.provider.requests().pop().unwrap();
        assert_eq!(last.tool_choice, ToolChoice::None);
        assert_eq!(
            last.tools.len(),
            3,
            "history references tools, so they stay declared"
        );
        assert!(
            matches!(&last.messages[0], ChatMessage::System { text } if text.contains("budget"))
        );
    }

    #[tokio::test]
    async fn cancellation_stops_stream() {
        let h = Harness::new();
        h.provider
            .push_hang(vec![StreamEvent::TextDelta("Thinking".into())]);
        let mut history = user("hi");
        let ctx = ToolContext::default();
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel.cancel();
        });
        let err = h.run(&AlwaysApprove, &mut history, &ctx).await.unwrap_err();
        assert_eq!(err, AgentError::Cancelled);
        assert_eq!(history.last(), Some(&ChatMessage::assistant("Thinking")));
    }

    #[tokio::test]
    async fn cancellation_during_tool_answers_every_pending_call() {
        let h = Harness::new();
        h.provider.push(vec![
            StreamEvent::ToolCall(ToolCall {
                id: "a".into(),
                name: "slow_thing".into(),
                arguments: json!({}),
            }),
            StreamEvent::ToolCall(ToolCall {
                id: "b".into(),
                name: "read_thing".into(),
                arguments: json!({}),
            }),
            StreamEvent::Done(StopReason::ToolUse),
        ]);
        let mut history = user("hi");
        let ctx = ToolContext::default();
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel.cancel();
        });
        let err = h.run(&AlwaysApprove, &mut history, &ctx).await.unwrap_err();
        assert_eq!(err, AgentError::Cancelled);
        let ids: Vec<_> = tool_results(&history)
            .into_iter()
            .map(|r| r.call_id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[tokio::test(start_paused = true)]
    async fn turn_timeout_stops_a_hanging_stream() {
        let mut h = Harness::new();
        h.config.turn_timeout = Duration::from_secs(5);
        h.provider.push_hang(vec![]);
        let mut history = user("hi");
        let err = h
            .run(&AlwaysApprove, &mut history, &ToolContext::default())
            .await
            .unwrap_err();
        assert_eq!(err, AgentError::TimedOut);
    }

    #[tokio::test]
    async fn bad_calls_are_reported_to_the_model_not_raised() {
        let h = Harness::new();
        h.provider
            .push(vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "a".into(),
                    name: "nope".into(),
                    arguments: json!({}),
                }),
                StreamEvent::ToolCall(ToolCall {
                    id: "b".into(),
                    name: "read_thing".into(),
                    arguments: Value::String("{broken".into()),
                }),
                StreamEvent::Done(StopReason::ToolUse),
            ])
            .push_text("sorry");
        let mut history = user("hi");
        h.run(&AlwaysApprove, &mut history, &ToolContext::default())
            .await
            .unwrap();
        let results = tool_results(&history);
        assert!(results.iter().all(|r| r.is_error));
        assert!(results[0].content.contains("unknown tool"));
        assert!(results[1].content.contains("JSON object"));
    }

    #[tokio::test]
    async fn refusal_runs_no_tools_and_keeps_partial_output_out_of_history() {
        let h = Harness::new();
        h.provider.push(vec![
            StreamEvent::TextDelta("Sure, I".into()),
            StreamEvent::ToolCall(ToolCall {
                id: "c".into(),
                name: "change_thing".into(),
                arguments: json!({}),
            }),
            StreamEvent::Done(StopReason::Refusal),
        ]);
        let mut history = user("do it");
        let err = h
            .run(&AlwaysApprove, &mut history, &ToolContext::default())
            .await
            .unwrap_err();
        assert_eq!(err, AgentError::Refused);
        assert_eq!(history.len(), 1);
        assert!(h.rec.audit.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn calls_cut_off_by_max_tokens_are_answered_but_not_run() {
        let h = Harness::new();
        h.provider
            .push(vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "c".into(),
                    name: "change_thing".into(),
                    arguments: json!({"price": "1"}),
                }),
                StreamEvent::Done(StopReason::MaxTokens),
            ])
            .push_text("Retrying later");
        let mut history = user("do it");
        h.run(&AlwaysApprove, &mut history, &ToolContext::default())
            .await
            .unwrap();
        let results = tool_results(&history);
        assert!(results[0].is_error && results[0].content.contains("token limit"));
        assert!(h.rec.audit.lock().unwrap().is_empty(), "never executed");
    }

    #[tokio::test]
    async fn provider_content_is_attached_to_the_assistant_message() {
        let h = Harness::new();
        h.provider.push(vec![
            StreamEvent::TextDelta("hi".into()),
            StreamEvent::ProviderContent(json!([{"type": "thinking", "signature": "s"}])),
            StreamEvent::Done(StopReason::EndTurn),
        ]);
        let mut history = user("hi");
        h.run(&AlwaysApprove, &mut history, &ToolContext::default())
            .await
            .unwrap();
        let ChatMessage::Assistant {
            provider_content: Some(p),
            ..
        } = &history[1]
        else {
            panic!("{:?}", history[1]);
        };
        assert_eq!(p.provider, llm::ProviderId::OpenAi);
        assert_eq!(p.content[0]["signature"], "s");
    }

    struct Leaky;

    #[async_trait]
    impl Tool for Leaky {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "leaky_change".into(),
                description: "d".into(),
                parameters: json!({"type": "object"}),
                mutability: Mutability::Mutate,
            }
        }
        async fn call(&self, _: Value, _: &ToolContext) -> Result<String, ToolError> {
            Err(ToolError::Failed("401: key s3cr3t-value rejected".into()))
        }
    }

    #[tokio::test]
    async fn secrets_are_redacted_from_tool_results_and_audit() {
        let mut h = Harness::new();
        h.tools.register(Arc::new(Leaky));
        h.provider
            .push_tool_call("c", "leaky_change", json!({}))
            .push_text("ok");
        let mut history = user("go");
        h.run(&AlwaysApprove, &mut history, &ToolContext::default())
            .await
            .unwrap();
        assert_eq!(
            tool_results(&history)[0].content,
            "401: key [redacted] rejected"
        );
        let audit = h.rec.audit.lock().unwrap();
        assert_eq!(
            audit[1].stage,
            AuditStage::Failed("401: key [redacted] rejected".into())
        );
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc".into(), 5), "abc");
        let t = truncate("ééééé".into(), 2);
        assert!(t.starts_with("éé\n[truncated 3 characters"));
    }

    #[test]
    fn responder_answers_once() {
        let (r, mut rx) = Responder::new();
        assert!(r.is_pending());
        r.clone().respond(Decision::Approve);
        r.respond(Decision::Deny(None));
        assert!(!r.is_pending());
        assert_eq!(rx.try_recv().unwrap(), Decision::Approve);
    }
}
