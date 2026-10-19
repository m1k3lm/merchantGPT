//! OpenAI Chat Completions codec. DeepSeek exposes the same API, so it reuses
//! this client with a different base URL.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::dsml::DsmlFilter;
use crate::http::send_with_retry;
use crate::sse::{decode_stream, sse_events, SseDecoder, SseEvent};
use crate::{
    ChatMessage, ChatRequest, EventStream, LlmError, LlmProvider, ProviderConfig, ProviderId,
    StopReason, StreamEvent, ToolCall, ToolChoice, Usage,
};

pub struct OpenAiProvider {
    cfg: ProviderConfig,
    http: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(cfg: ProviderConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> ProviderId {
        self.cfg.provider
    }

    async fn stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        let url = format!("{}/chat/completions", self.cfg.base_url());
        let body = encode(&req, self.cfg.provider);
        let resp = send_with_retry(|| {
            self.http
                .post(&url)
                .bearer_auth(&self.cfg.api_key)
                .json(&body)
        })
        .await?;
        let events = sse_events(Box::pin(resp.bytes_stream()));
        Ok(decode_stream(events, Decoder::default()))
    }
}

pub(crate) fn encode(req: &ChatRequest, provider: ProviderId) -> Value {
    let mut messages = Vec::new();
    for m in &req.messages {
        match m {
            ChatMessage::System { text } => {
                messages.push(json!({"role": "system", "content": text}))
            }
            ChatMessage::User { text } => messages.push(json!({"role": "user", "content": text})),
            ChatMessage::Assistant {
                text, tool_calls, ..
            } => {
                let mut msg = json!({
                    "role": "assistant",
                    "content": if text.is_empty() { Value::Null } else { json!(text) },
                });
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {"name": c.name, "arguments": args_string(&c.arguments)},
                            })
                        })
                        .collect();
                }
                messages.push(msg);
            }
            ChatMessage::ToolResults { results } => {
                for r in results {
                    let content = if r.is_error {
                        format!("Error: {}", r.content)
                    } else {
                        r.content.clone()
                    };
                    messages.push(
                        json!({"role": "tool", "tool_call_id": r.call_id, "content": content}),
                    );
                }
            }
        }
    }
    // OpenAI deprecated max_tokens for newer models; DeepSeek only knows max_tokens.
    let (max_key, max_tokens) = match provider {
        // deepseek-chat rejects max_tokens above 8192.
        ProviderId::DeepSeek => ("max_tokens", req.max_tokens.min(8192)),
        _ => ("max_completion_tokens", req.max_tokens),
    };
    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        max_key: max_tokens,
    });
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {"name": t.name, "description": t.description, "parameters": t.parameters},
                })
            })
            .collect();
        if req.tool_choice == ToolChoice::None {
            body["tool_choice"] = json!("none");
        }
    }
    body
}

/// Invalid JSON produced by the model travels as a raw string; re-send it as is.
fn args_string(args: &Value) -> String {
    match args {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn parse_args(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
pub(crate) struct Decoder {
    calls: BTreeMap<u64, PartialCall>,
    stop: Option<StopReason>,
    done: bool,
    dsml: DsmlFilter,
    native_calls: bool,
}

impl Decoder {
    fn flush_calls(&mut self) -> Vec<StreamEvent> {
        self.native_calls |= !self.calls.is_empty();
        std::mem::take(&mut self.calls)
            .into_values()
            .map(|c| {
                StreamEvent::ToolCall(ToolCall {
                    id: c.id,
                    name: c.name,
                    arguments: parse_args(&c.args),
                })
            })
            .collect()
    }
}

impl SseDecoder for Decoder {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, LlmError> {
        if ev.data.trim() == "[DONE]" {
            return Ok(self.finish());
        }
        let v: Value =
            serde_json::from_str(&ev.data).map_err(|e| LlmError::Decode(e.to_string()))?;
        if let Some(err) = v.get("error") {
            let message = err["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string();
            return Err(LlmError::Http { status: 0, message });
        }
        let mut out = Vec::new();
        if let Some(choice) = v["choices"].get(0) {
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                let shown = self.dsml.push(text);
                if !shown.is_empty() {
                    out.push(StreamEvent::TextDelta(shown));
                }
            }
            for tc in delta["tool_calls"].as_array().into_iter().flatten() {
                let call = self
                    .calls
                    .entry(tc["index"].as_u64().unwrap_or(0))
                    .or_default();
                if let Some(id) = tc["id"].as_str() {
                    call.id = id.to_string();
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    call.name.push_str(name);
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    call.args.push_str(args);
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.stop = Some(match reason {
                    "stop" => StopReason::EndTurn,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    other => StopReason::Other(other.to_string()),
                });
                out.extend(self.flush_calls());
            }
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            out.push(StreamEvent::Usage(Usage {
                input_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
            }));
        }
        Ok(out)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = self.flush_calls();
        let (held, leaked) = self.dsml.finish(self.native_calls);
        if !held.is_empty() {
            out.insert(0, StreamEvent::TextDelta(held));
        }
        let mut stop = self.stop.take().unwrap_or(StopReason::EndTurn);
        if !leaked.is_empty() && stop == StopReason::EndTurn {
            stop = StopReason::ToolUse;
        }
        out.extend(leaked.into_iter().map(StreamEvent::ToolCall));
        out.push(StreamEvent::Done(stop));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolDefinition, ToolResult};
    use tools::Mutability;

    fn data(d: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: d.into(),
        }
    }

    fn run(chunks: &[&str]) -> Vec<StreamEvent> {
        let mut dec = Decoder::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(dec.on_event(&data(c)).unwrap());
        }
        out.extend(dec.finish());
        out
    }

    /// DeepSeek V4 can put its tool calls in the text; the stream then
    /// carries calls instead of markup and stops for tool use.
    #[test]
    fn deepseek_tool_calls_leaked_into_text_become_calls() {
        let chunk = |t: &str| json!({"choices": [{"delta": {"content": t}}]}).to_string();
        let out = run(&[
            &chunk("Añado la opción. < ｜｜DSML"),
            &chunk("｜｜ calls> < ｜｜DSML｜｜ invoke name=\"shop_api_write\"> < ｜｜DSML｜｜ parameter name=\"method\" string=\"true\">POST</｜｜DSML｜｜ parameter>"),
            &chunk(" </｜｜DSML｜｜ invoke> </｜｜DSML｜｜ calls>"),
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        let text: String = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Añado la opción. ");
        let calls: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCall(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shop_api_write");
        assert_eq!(calls[0].arguments, json!({"method": "POST"}));
        assert_eq!(out.last(), Some(&StreamEvent::Done(StopReason::ToolUse)));
    }

    #[test]
    fn openai_accumulates_tool_call_argument_fragments_by_index() {
        let out = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"one","arguments":"{\"x\""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"two","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{oops"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#,
            "[DONE]",
        ]);
        assert_eq!(
            out,
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "a".into(),
                    name: "one".into(),
                    arguments: json!({"x": 1})
                }),
                StreamEvent::ToolCall(ToolCall {
                    id: "b".into(),
                    name: "two".into(),
                    arguments: Value::String("{oops".into())
                }),
                StreamEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 3
                }),
                StreamEvent::Done(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn text_deltas_stream_and_missing_done_marker_still_finishes() {
        let out = run(&[
            r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"length"}]}"#,
        ]);
        assert_eq!(
            out,
            vec![
                StreamEvent::TextDelta("Hel".into()),
                StreamEvent::TextDelta("lo".into()),
                StreamEvent::Done(StopReason::MaxTokens),
            ]
        );
    }

    #[test]
    fn in_stream_error_is_reported() {
        let mut dec = Decoder::default();
        let err = dec
            .on_event(&data(r#"{"error":{"message":"overloaded"}}"#))
            .unwrap_err();
        assert_eq!(
            err,
            LlmError::Http {
                status: 0,
                message: "overloaded".into()
            }
        );
    }

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages,
            tools: vec![ToolDefinition {
                name: "shop_get_order".into(),
                description: "d".into(),
                parameters: json!({"type": "object"}),
                mutability: Mutability::Read,
            }],
            tool_choice: ToolChoice::Auto,
            max_tokens: 100,
            temperature: None,
        }
    }

    #[test]
    fn openai_encodes_tool_results_as_tool_role_messages() {
        let req = request(vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::assistant_with_calls(
                String::new(),
                vec![
                    ToolCall {
                        id: "a".into(),
                        name: "shop_get_order".into(),
                        arguments: json!({"id": "1"}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "shop_get_order".into(),
                        arguments: json!({"id": "2"}),
                    },
                ],
            ),
            ChatMessage::ToolResults {
                results: vec![ToolResult::ok("a", "{}"), ToolResult::error("b", "boom")],
            },
        ]);
        let body = encode(&req, ProviderId::OpenAi);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(msgs[2]["content"], Value::Null);
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"id":"1"}"#
        );
        assert_eq!(
            msgs[3],
            json!({"role": "tool", "tool_call_id": "a", "content": "{}"})
        );
        assert_eq!(
            msgs[4],
            json!({"role": "tool", "tool_call_id": "b", "content": "Error: boom"})
        );
        assert_eq!(body["tools"][0]["function"]["name"], "shop_get_order");
        assert_eq!(body["max_completion_tokens"], 100);
    }

    #[test]
    fn deepseek_uses_capped_max_tokens_and_tools_are_omitted_when_empty() {
        let mut req = request(vec![ChatMessage::user("hi")]);
        req.tools.clear();
        req.max_tokens = 16000;
        let body = encode(&req, ProviderId::DeepSeek);
        assert_eq!(body["max_tokens"], 8192);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn tool_choice_none_keeps_tools_declared() {
        let mut req = request(vec![ChatMessage::user("hi")]);
        req.tool_choice = ToolChoice::None;
        let body = encode(&req, ProviderId::OpenAi);
        assert_eq!(body["tool_choice"], "none");
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    }
}
