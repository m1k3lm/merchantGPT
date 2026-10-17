//! Scripted provider for tests: each `stream` call plays the next script entry
//! and records the request it received.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use crate::{
    ChatRequest, EventStream, LlmError, LlmProvider, ProviderId, StopReason, StreamEvent, ToolCall,
};

enum Script {
    Events(Vec<StreamEvent>),
    /// Emits the events, then never completes (for cancellation tests).
    Hang(Vec<StreamEvent>),
    Fail(LlmError),
}

#[derive(Default)]
pub struct FakeProvider {
    script: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl FakeProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, events: Vec<StreamEvent>) -> &Self {
        self.script
            .lock()
            .unwrap()
            .push_back(Script::Events(events));
        self
    }

    pub fn push_text(&self, text: &str) -> &Self {
        self.push(vec![
            StreamEvent::TextDelta(text.to_string()),
            StreamEvent::Done(StopReason::EndTurn),
        ])
    }

    pub fn push_tool_call(&self, id: &str, name: &str, args: Value) -> &Self {
        self.push(vec![
            StreamEvent::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args,
            }),
            StreamEvent::Done(StopReason::ToolUse),
        ])
    }

    pub fn push_hang(&self, events: Vec<StreamEvent>) -> &Self {
        self.script.lock().unwrap().push_back(Script::Hang(events));
        self
    }

    pub fn push_error(&self, err: LlmError) -> &Self {
        self.script.lock().unwrap().push_back(Script::Fail(err));
        self
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for FakeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenAi
    }

    async fn stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        self.requests.lock().unwrap().push(req);
        let next = self.script.lock().unwrap().pop_front();
        match next {
            Some(Script::Events(evs)) => Ok(futures::stream::iter(evs.into_iter().map(Ok)).boxed()),
            Some(Script::Hang(evs)) => Ok(futures::stream::iter(evs.into_iter().map(Ok))
                .chain(futures::stream::pending())
                .boxed()),
            Some(Script::Fail(e)) => Err(e),
            None => Err(LlmError::Config("FakeProvider script exhausted".into())),
        }
    }
}
