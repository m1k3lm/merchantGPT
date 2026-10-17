//! Anthropic Messages API codec (`POST /v1/messages`, streamed).
//!
//! Current Claude models always think; their thinking blocks must be replayed
//! unchanged, so the decoder returns the turn's full content blocks as
//! `StreamEvent::ProviderContent` and the encoder replays them verbatim.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::http::send_with_retry;
use crate::openai::parse_args;
use crate::sse::{decode_stream, sse_events, SseDecoder, SseEvent};
use crate::{
    ChatMessage, ChatRequest, EventStream, LlmError, LlmProvider, ProviderConfig, ProviderId,
    StopReason, StreamEvent, ToolCall, ToolChoice, Usage,
};

const API_VERSION: &str = "2023-06-01";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Models that accept `fallbacks: "default"` on the Claude API.
const FALLBACK_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];

pub struct AnthropicProvider {
    cfg: ProviderConfig,
    http: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(cfg: ProviderConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Options {
    /// Server-side refusal fallbacks; only on the official endpoint, since a
    /// proxy may reject the beta field.
    pub fallbacks: bool,
    pub eager_input_streaming: bool,
}

impl Options {
    fn for_config(cfg: &ProviderConfig) -> Self {
        let official = cfg.is_official_endpoint();
        Self {
            fallbacks: official && FALLBACK_MODELS.contains(&cfg.model.as_str()),
            eager_input_streaming: official,
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Anthropic
    }

    async fn stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        let url = format!("{}/v1/messages", self.cfg.base_url());
        let opts = Options::for_config(&self.cfg);
        let body = encode(&req, opts);
        let resp = send_with_retry(|| {
            let mut b = self
                .http
                .post(&url)
                .header("x-api-key", &self.cfg.api_key)
                .header("anthropic-version", API_VERSION)
                .json(&body);
            if opts.fallbacks {
                b = b.header("anthropic-beta", FALLBACK_BETA);
            }
            b
        })
        .await?;
        let events = sse_events(Box::pin(resp.bytes_stream()));
        Ok(decode_stream(events, Decoder::default()))
    }
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// Appends a message, merging it into the previous one when the role repeats.
fn push(messages: &mut Vec<Value>, role: &str, mut blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut() {
        if last["role"] == role {
            if let Some(content) = last["content"].as_array_mut() {
                content.append(&mut blocks);
                return;
            }
        }
    }
    messages.push(json!({"role": role, "content": blocks}));
}

pub(crate) fn encode(req: &ChatRequest, opts: Options) -> Value {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for m in &req.messages {
        match m {
            ChatMessage::System { text } => system.push(text.clone()),
            ChatMessage::User { text } => push(&mut messages, "user", vec![text_block(text)]),
            ChatMessage::Assistant {
                text,
                tool_calls,
                provider_content,
            } => {
                let replay = provider_content
                    .as_ref()
                    .filter(|p| p.provider == ProviderId::Anthropic)
                    .and_then(|p| p.content.as_array().cloned());
                let blocks = replay.unwrap_or_else(|| {
                    let mut b = Vec::new();
                    if !text.is_empty() {
                        b.push(text_block(text));
                    }
                    for c in tool_calls {
                        let input = if c.arguments.is_object() {
                            c.arguments.clone()
                        } else {
                            json!({})
                        };
                        b.push(
                            json!({"type": "tool_use", "id": c.id, "name": c.name, "input": input}),
                        );
                    }
                    b
                });
                push(&mut messages, "assistant", blocks);
            }
            ChatMessage::ToolResults { results } => {
                let blocks = results
                    .iter()
                    .map(|r| {
                        json!({
                            "type": "tool_result",
                            "tool_use_id": r.call_id,
                            "content": r.content,
                            "is_error": r.is_error,
                        })
                    })
                    .collect();
                push(&mut messages, "user", blocks);
            }
        }
    }
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": true,
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| {
                let mut tool = json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                });
                if opts.eager_input_streaming {
                    tool["eager_input_streaming"] = json!(true);
                }
                tool
            })
            .collect();
        if req.tool_choice == ToolChoice::None {
            body["tool_choice"] = json!({"type": "none"});
        }
    }
    if opts.fallbacks {
        body["fallbacks"] = json!("default");
    }
    body
}

#[derive(Default)]
struct Block {
    value: Value,
    partial_json: String,
}

#[derive(Default)]
pub(crate) struct Decoder {
    blocks: BTreeMap<u64, Block>,
    usage: Usage,
    stop: Option<StopReason>,
    done: bool,
}

impl Decoder {
    /// Applies the echo rule for a mid-output fallback: thinking, tool_use and
    /// unrecognised blocks before the last `fallback` marker are dropped.
    fn final_blocks(&mut self) -> Vec<Value> {
        let blocks: Vec<Value> = std::mem::take(&mut self.blocks)
            .into_values()
            .map(|b| b.value)
            .collect();
        let Some(boundary) = blocks.iter().rposition(|b| b["type"] == "fallback") else {
            return blocks;
        };
        blocks
            .into_iter()
            .enumerate()
            .filter(|(i, b)| *i > boundary || b["type"] == "text")
            .map(|(_, b)| b)
            .collect()
    }

    fn finish_message(&mut self) -> Vec<StreamEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let blocks = self.final_blocks();
        let mut out: Vec<StreamEvent> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| {
                StreamEvent::ToolCall(ToolCall {
                    id: b["id"].as_str().unwrap_or_default().to_string(),
                    name: b["name"].as_str().unwrap_or_default().to_string(),
                    arguments: b["input"].clone(),
                })
            })
            .collect();
        out.push(StreamEvent::Usage(self.usage));
        out.push(StreamEvent::ProviderContent(Value::Array(blocks)));
        out.push(StreamEvent::Done(
            self.stop.take().unwrap_or(StopReason::EndTurn),
        ));
        out
    }
}

fn stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        other => StopReason::Other(other.to_string()),
    }
}

impl SseDecoder for Decoder {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, LlmError> {
        if ev.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let v: Value =
            serde_json::from_str(&ev.data).map_err(|e| LlmError::Decode(e.to_string()))?;
        let index = v["index"].as_u64().unwrap_or(0);
        match v["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let u = &v["message"]["usage"];
                self.usage.input_tokens = [
                    "input_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ]
                .iter()
                .filter_map(|k| u[*k].as_u64())
                .sum();
                self.usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                self.blocks.insert(
                    index,
                    Block {
                        value: v["content_block"].clone(),
                        partial_json: String::new(),
                    },
                );
            }
            "content_block_delta" => {
                let d = &v["delta"];
                let Some(block) = self.blocks.get_mut(&index) else {
                    return Ok(Vec::new());
                };
                match d["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or_default();
                        let current = block.value["text"].as_str().unwrap_or_default();
                        block.value["text"] = json!(format!("{current}{t}"));
                        if !t.is_empty() {
                            return Ok(vec![StreamEvent::TextDelta(t.to_string())]);
                        }
                    }
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or_default();
                        let current = block.value["thinking"].as_str().unwrap_or_default();
                        block.value["thinking"] = json!(format!("{current}{t}"));
                    }
                    "signature_delta" => {
                        let s = d["signature"].as_str().unwrap_or_default();
                        let current = block.value["signature"].as_str().unwrap_or_default();
                        block.value["signature"] = json!(format!("{current}{s}"));
                    }
                    "input_json_delta" => {
                        block
                            .partial_json
                            .push_str(d["partial_json"].as_str().unwrap_or_default());
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(block) = self.blocks.get_mut(&index) {
                    if block.value["type"] == "tool_use" && !block.partial_json.is_empty() {
                        // Invalid JSON stays a string so the agent can report it
                        // back; the replayed block itself needs an object.
                        let parsed = parse_args(&block.partial_json);
                        block.value["input"] = if parsed.is_object() {
                            parsed
                        } else {
                            json!({"INVALID_JSON": block.partial_json})
                        };
                    }
                }
            }
            "message_delta" => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(stop_reason(s));
                }
                if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = o;
                }
            }
            "message_stop" => return Ok(self.finish_message()),
            "error" => {
                let e = &v["error"];
                let message = e["message"].as_str().unwrap_or("unknown error").to_string();
                return Err(match e["type"].as_str() {
                    Some("rate_limit_error") => LlmError::RateLimited(message),
                    Some("authentication_error" | "permission_error") => LlmError::Auth(message),
                    _ => LlmError::Http { status: 0, message },
                });
            }
            _ => {}
        }
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        self.finish_message()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderContent, ToolDefinition, ToolResult};
    use tools::Mutability;

    fn run(events: &[&str]) -> Vec<StreamEvent> {
        let mut dec = Decoder::default();
        let mut out = Vec::new();
        for e in events {
            out.extend(
                dec.on_event(&SseEvent {
                    event: None,
                    data: e.to_string(),
                })
                .unwrap(),
            );
        }
        out.extend(dec.finish());
        out
    }

    fn opts() -> Options {
        Options {
            fallbacks: false,
            eager_input_streaming: false,
        }
    }

    fn req(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "claude-opus-5-5".into(),
            messages,
            tools: vec![ToolDefinition {
                name: "shop_get_order".into(),
                description: "Get an order".into(),
                parameters: json!({"type": "object", "properties": {"id": {"type": "string"}}}),
                mutability: Mutability::Read,
            }],
            tool_choice: ToolChoice::Auto,
            max_tokens: 16000,
            temperature: None,
        }
    }

    #[test]
    fn anthropic_input_json_delta_accumulates() {
        let out = run(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":50,"cache_read_input_tokens":10,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig=="}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Checking"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"shop_get_order","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"id\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":" \"727\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(out[0], StreamEvent::TextDelta("Checking".into()));
        assert_eq!(
            out[1],
            StreamEvent::ToolCall(ToolCall {
                id: "toolu_1".into(),
                name: "shop_get_order".into(),
                arguments: json!({"id": "727"}),
            })
        );
        assert_eq!(
            out[2],
            StreamEvent::Usage(Usage {
                input_tokens: 60,
                output_tokens: 40
            })
        );
        let StreamEvent::ProviderContent(blocks) = &out[3] else {
            panic!("expected content, got {:?}", out[3]);
        };
        assert_eq!(
            blocks[0],
            json!({"type": "thinking", "thinking": "", "signature": "sig=="})
        );
        assert_eq!(blocks[2]["input"], json!({"id": "727"}));
        assert_eq!(out[4], StreamEvent::Done(StopReason::ToolUse));
    }

    #[test]
    fn mid_output_fallback_drops_blocks_before_the_boundary() {
        let out = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":"a"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Partial"}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t0","name":"x","input":{}}}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"fallback","from":{"model":"claude-opus-5-5"},"to":{"model":"claude-opus-4-8"}}}"#,
            r#"{"type":"content_block_start","index":4,"content_block":{"type":"text","text":"Rest"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert!(!out.iter().any(|e| matches!(e, StreamEvent::ToolCall(_))));
        let StreamEvent::ProviderContent(blocks) = &out[1] else {
            panic!("{out:?}");
        };
        let types: Vec<_> = blocks
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["text", "text"]);
    }

    #[test]
    fn refusal_and_stream_errors_are_surfaced() {
        let out = run(&[
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":0}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(out.last(), Some(&StreamEvent::Done(StopReason::Refusal)));
        let mut dec = Decoder::default();
        let err = dec
            .on_event(&SseEvent {
                event: Some("error".into()),
                data:
                    r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
                        .into(),
            })
            .unwrap_err();
        assert_eq!(
            err,
            LlmError::Http {
                status: 0,
                message: "Overloaded".into()
            }
        );
    }

    #[test]
    fn invalid_tool_json_is_kept_as_an_object_for_replay() {
        let out = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"x","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"id\": 7"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let StreamEvent::ToolCall(call) = &out[0] else {
            panic!("{out:?}");
        };
        assert_eq!(call.arguments, json!({"INVALID_JSON": "{\"id\": 7"}));
    }

    #[test]
    fn anthropic_groups_tool_results_in_one_user_message() {
        let body = encode(
            &req(vec![
                ChatMessage::system("sys"),
                ChatMessage::user("orders?"),
                ChatMessage::assistant_with_calls(
                    "",
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
                    results: vec![ToolResult::ok("a", "{}"), ToolResult::error("b", "gone")],
                },
                ChatMessage::user("and now?"),
            ]),
            opts(),
        );
        assert_eq!(body["system"], "sys");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "results and the next user text merge");
        assert_eq!(msgs[1]["content"].as_array().unwrap().len(), 2);
        let user = msgs[2]["content"].as_array().unwrap();
        assert_eq!(user[0]["type"], "tool_result");
        assert_eq!(user[1]["is_error"], true);
        assert_eq!(user[2], json!({"type": "text", "text": "and now?"}));
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body.get("fallbacks").is_none());
    }

    #[test]
    fn provider_content_is_replayed_verbatim_only_to_anthropic() {
        let raw = json!([{"type": "thinking", "thinking": "", "signature": "s"}, {"type": "text", "text": "hi"}]);
        let mut msg = ChatMessage::assistant("hi");
        if let ChatMessage::Assistant {
            provider_content, ..
        } = &mut msg
        {
            *provider_content = Some(ProviderContent {
                provider: ProviderId::Anthropic,
                content: raw.clone(),
            });
        }
        let body = encode(&req(vec![ChatMessage::user("x"), msg.clone()]), opts());
        assert_eq!(body["messages"][1]["content"], raw);

        if let ChatMessage::Assistant {
            provider_content: Some(p),
            ..
        } = &mut msg
        {
            p.provider = ProviderId::Google;
        }
        let body = encode(&req(vec![ChatMessage::user("x"), msg]), opts());
        assert_eq!(
            body["messages"][1]["content"],
            json!([{"type": "text", "text": "hi"}])
        );
    }

    #[test]
    fn official_endpoint_enables_fallbacks_and_eager_streaming() {
        let cfg = |model: &str, base: Option<&str>| ProviderConfig {
            provider: ProviderId::Anthropic,
            api_key: "k".into(),
            base_url: base.map(str::to_string),
            model: model.into(),
        };
        let o = Options::for_config(&cfg("claude-opus-5-5", None));
        assert!(o.fallbacks && o.eager_input_streaming);
        assert!(!Options::for_config(&cfg("claude-haiku-5-5", None)).fallbacks);
        let proxied = Options::for_config(&cfg("claude-opus-5-5", Some("https://gw.example")));
        assert!(!proxied.fallbacks && !proxied.eager_input_streaming);

        let mut r = req(vec![ChatMessage::user("x")]);
        r.tool_choice = ToolChoice::None;
        let body = encode(&r, o);
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["tools"][0]["eager_input_streaming"], true);
        assert_eq!(body["tool_choice"], json!({"type": "none"}));
    }
}
