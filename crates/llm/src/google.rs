//! Google Gemini `streamGenerateContent` codec.
//!
//! Gemini may omit function-call ids, so missing ids are synthesised and tool
//! results are matched back to calls by id (falling back to order). Model parts
//! carry thought signatures that must be replayed, so they are kept verbatim.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::http::send_with_retry;
use crate::sse::{decode_stream, sse_events, SseDecoder, SseEvent};
use crate::{
    ChatMessage, ChatRequest, EventStream, LlmError, LlmProvider, ProviderConfig, ProviderId,
    StopReason, StreamEvent, ToolCall, ToolChoice, Usage,
};

pub struct GoogleProvider {
    cfg: ProviderConfig,
    http: reqwest::Client,
}

impl GoogleProvider {
    pub fn new(cfg: ProviderConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }
}

#[async_trait]
impl LlmProvider for GoogleProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Google
    }

    async fn stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.cfg.base_url(),
            self.cfg.model
        );
        let body = encode(&req);
        let resp = send_with_retry(|| {
            self.http
                .post(&url)
                .header("x-goog-api-key", &self.cfg.api_key)
                .json(&body)
        })
        .await?;
        let events = sse_events(Box::pin(resp.bytes_stream()));
        Ok(decode_stream(events, Decoder::default()))
    }
}

const ALLOWED_KEYS: &[&str] = &[
    "type",
    "format",
    "description",
    "nullable",
    "enum",
    "properties",
    "required",
    "items",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
];

/// Reduces a JSON Schema to the OpenAPI subset Gemini's `parameters` accepts.
pub(crate) fn sanitize_schema(schema: &Value) -> Value {
    let Some(obj) = schema.as_object() else {
        return schema.clone();
    };
    let mut out = Map::new();
    for (k, v) in obj {
        if !ALLOWED_KEYS.contains(&k.as_str()) {
            continue;
        }
        let v = match k.as_str() {
            "properties" => Value::Object(
                v.as_object()
                    .map(|props| {
                        props
                            .iter()
                            .map(|(name, s)| (name.clone(), sanitize_schema(s)))
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            "items" => sanitize_schema(v),
            // Gemini only accepts these string formats.
            "format" if !matches!(v.as_str(), Some("enum" | "date-time")) => continue,
            "required" if v.as_array().is_some_and(|a| a.is_empty()) => continue,
            _ => v.clone(),
        };
        out.insert(k.clone(), v);
    }
    Value::Object(out)
}

fn push(contents: &mut Vec<Value>, role: &str, mut parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut() {
        if last["role"] == role {
            if let Some(p) = last["parts"].as_array_mut() {
                p.append(&mut parts);
                return;
            }
        }
    }
    contents.push(json!({"role": role, "parts": parts}));
}

pub(crate) fn encode(req: &ChatRequest) -> Value {
    let mut system = Vec::new();
    let mut contents = Vec::new();
    // call id -> (function name, the id Gemini gave it, if any)
    let mut calls: HashMap<String, (String, bool)> = HashMap::new();
    let mut call_order: Vec<String> = Vec::new();
    for m in &req.messages {
        match m {
            ChatMessage::System { text } => system.push(text.clone()),
            ChatMessage::User { text } => push(&mut contents, "user", vec![json!({"text": text})]),
            ChatMessage::Assistant {
                text,
                tool_calls,
                provider_content,
            } => {
                for c in tool_calls {
                    let native = !c.id.starts_with(SYNTHETIC_PREFIX);
                    calls.insert(c.id.clone(), (c.name.clone(), native));
                    call_order.push(c.id.clone());
                }
                let replay = provider_content
                    .as_ref()
                    .filter(|p| p.provider == ProviderId::Google)
                    .and_then(|p| p.content.as_array().cloned());
                let parts = replay.unwrap_or_else(|| {
                    let mut p = Vec::new();
                    if !text.is_empty() {
                        p.push(json!({"text": text}));
                    }
                    for c in tool_calls {
                        let args = if c.arguments.is_object() {
                            c.arguments.clone()
                        } else {
                            json!({})
                        };
                        p.push(json!({"functionCall": {"name": c.name, "args": args}}));
                    }
                    p
                });
                push(&mut contents, "model", parts);
            }
            ChatMessage::ToolResults { results } => {
                let parts = results
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let (name, native) = calls
                            .get(&r.call_id)
                            .cloned()
                            .or_else(|| {
                                let start = call_order.len().saturating_sub(results.len());
                                call_order
                                    .get(start + i)
                                    .and_then(|id| calls.get(id).cloned())
                            })
                            .unwrap_or_default();
                        let payload: Value = serde_json::from_str(&r.content)
                            .unwrap_or_else(|_| Value::String(r.content.clone()));
                        let response = if r.is_error {
                            json!({"error": payload})
                        } else {
                            json!({"result": payload})
                        };
                        let mut fr = json!({"name": name, "response": response});
                        if native {
                            fr["id"] = json!(r.call_id);
                        }
                        json!({"functionResponse": fr})
                    })
                    .collect();
                push(&mut contents, "user", parts);
            }
        }
    }
    let mut body = json!({
        "contents": contents,
        "generationConfig": {"maxOutputTokens": req.max_tokens},
    });
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": system.join("\n\n")}]});
    }
    if let Some(t) = req.temperature {
        body["generationConfig"]["temperature"] = json!(t);
    }
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": sanitize_schema(&t.parameters),
                })
            })
            .collect();
        body["tools"] = json!([{"functionDeclarations": decls}]);
        let mode = match req.tool_choice {
            ToolChoice::Auto => "AUTO",
            ToolChoice::None => "NONE",
        };
        body["toolConfig"] = json!({"functionCallingConfig": {"mode": mode}});
    }
    body
}

const SYNTHETIC_PREFIX: &str = "gemini-call-";

#[derive(Default)]
pub(crate) struct Decoder {
    parts: Vec<Value>,
    calls: Vec<ToolCall>,
    usage: Usage,
    finish: Option<String>,
    done: bool,
}

impl SseDecoder for Decoder {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, LlmError> {
        if ev.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let v: Value =
            serde_json::from_str(&ev.data).map_err(|e| LlmError::Decode(e.to_string()))?;
        if let Some(err) = v.get("error") {
            let message = err["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string();
            let status = err["code"].as_u64().unwrap_or(0) as u16;
            return Err(LlmError::Http { status, message });
        }
        let mut out = Vec::new();
        let candidate = &v["candidates"][0];
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let mut part = part.clone();
            if let Some(fc) = part.get("functionCall") {
                let id = fc["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{SYNTHETIC_PREFIX}{}", uuid::Uuid::new_v4()));
                self.calls.push(ToolCall {
                    id,
                    name: fc["name"].as_str().unwrap_or_default().to_string(),
                    arguments: if fc["args"].is_object() {
                        fc["args"].clone()
                    } else {
                        json!({})
                    },
                });
            } else if part["thought"] != json!(true) {
                if let Some(t) = part["text"].as_str().filter(|t| !t.is_empty()) {
                    out.push(StreamEvent::TextDelta(t.to_string()));
                }
            }
            if part.is_object() {
                self.parts.push(std::mem::take(&mut part));
            }
        }
        if let Some(f) = candidate["finishReason"].as_str() {
            self.finish = Some(f.to_string());
        }
        if let Some(u) = v.get("usageMetadata") {
            self.usage = Usage {
                input_tokens: u["promptTokenCount"].as_u64().unwrap_or(0),
                output_tokens: u["candidatesTokenCount"].as_u64().unwrap_or(0)
                    + u["thoughtsTokenCount"].as_u64().unwrap_or(0),
            };
        }
        Ok(out)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let stop = match self.finish.as_deref() {
            _ if !self.calls.is_empty() => StopReason::ToolUse,
            None | Some("STOP") => StopReason::EndTurn,
            Some("MAX_TOKENS") => StopReason::MaxTokens,
            Some("SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "RECITATION") => {
                StopReason::Refusal
            }
            Some(other) => StopReason::Other(other.to_string()),
        };
        let mut out: Vec<StreamEvent> = std::mem::take(&mut self.calls)
            .into_iter()
            .map(StreamEvent::ToolCall)
            .collect();
        out.push(StreamEvent::Usage(self.usage));
        out.push(StreamEvent::ProviderContent(Value::Array(std::mem::take(
            &mut self.parts,
        ))));
        out.push(StreamEvent::Done(stop));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderContent, ToolDefinition, ToolResult};
    use tools::Mutability;

    fn run(chunks: &[&str]) -> Vec<StreamEvent> {
        let mut dec = Decoder::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(
                dec.on_event(&SseEvent {
                    event: None,
                    data: c.to_string(),
                })
                .unwrap(),
            );
        }
        out.extend(dec.finish());
        out
    }

    fn req(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "gemini".into(),
            messages,
            tools: vec![ToolDefinition {
                name: "shop_list_orders".into(),
                description: "List orders".into(),
                parameters: json!({
                    "type": "object",
                    "$schema": "x",
                    "additionalProperties": false,
                    "properties": {
                        "after": {"type": "string", "format": "date", "description": "YYYY-MM-DD"},
                        "page": {"type": "integer", "minimum": 1, "default": 1},
                        "tags": {"type": "array", "items": {"type": "string", "pattern": "x"}}
                    },
                    "required": []
                }),
                mutability: Mutability::Read,
            }],
            tool_choice: ToolChoice::Auto,
            max_tokens: 1000,
            temperature: None,
        }
    }

    #[test]
    fn gemini_sanitiser_strips_unsupported_keywords() {
        let s = sanitize_schema(&req(vec![]).tools[0].parameters);
        assert_eq!(
            s,
            json!({
                "type": "object",
                "properties": {
                    "after": {"type": "string", "description": "YYYY-MM-DD"},
                    "page": {"type": "integer", "minimum": 1},
                    "tags": {"type": "array", "items": {"type": "string"}}
                }
            })
        );
    }

    #[test]
    fn decoder_synthesises_ids_keeps_signed_parts_and_hides_thoughts() {
        let out = run(&[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"thinking...","thought":true},{"text":"Let me look"}]}}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"shop_list_orders","args":{"status":"processing"}},"thoughtSignature":"sig1"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":30,"candidatesTokenCount":5,"thoughtsTokenCount":7}}"#,
        ]);
        assert_eq!(out[0], StreamEvent::TextDelta("Let me look".into()));
        let StreamEvent::ToolCall(call) = &out[1] else {
            panic!("{out:?}");
        };
        assert!(call.id.starts_with(SYNTHETIC_PREFIX));
        assert_eq!(call.arguments, json!({"status": "processing"}));
        assert_eq!(
            out[2],
            StreamEvent::Usage(Usage {
                input_tokens: 30,
                output_tokens: 12
            })
        );
        let StreamEvent::ProviderContent(parts) = &out[3] else {
            panic!("{out:?}");
        };
        assert_eq!(parts[2]["thoughtSignature"], "sig1");
        assert_eq!(out[4], StreamEvent::Done(StopReason::ToolUse));
    }

    #[test]
    fn gemini_results_match_calls_by_order() {
        let calls = vec![
            ToolCall {
                id: format!("{SYNTHETIC_PREFIX}1"),
                name: "shop_get_order".into(),
                arguments: json!({"id": "1"}),
            },
            ToolCall {
                id: "native-2".into(),
                name: "shop_list_orders".into(),
                arguments: json!({}),
            },
        ];
        let body = encode(&req(vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::assistant_with_calls("", calls),
            ChatMessage::ToolResults {
                results: vec![
                    ToolResult::ok("unknown-id", r#"{"total":"5.00"}"#),
                    ToolResult::error("native-2", "boom"),
                ],
            },
        ]));
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "sys");
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[1]["role"], "model");
        let parts = contents[2]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["functionResponse"]["name"], "shop_get_order");
        assert_eq!(
            parts[0]["functionResponse"]["response"],
            json!({"result": {"total": "5.00"}})
        );
        assert!(parts[0]["functionResponse"].get("id").is_none());
        assert_eq!(parts[1]["functionResponse"]["name"], "shop_list_orders");
        assert_eq!(parts[1]["functionResponse"]["id"], "native-2");
        assert_eq!(
            parts[1]["functionResponse"]["response"],
            json!({"error": "boom"})
        );
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
    }

    #[test]
    fn replays_raw_parts_and_maps_safety_to_refusal() {
        let raw = json!([{"functionCall": {"name": "x", "args": {}}, "thoughtSignature": "s"}]);
        let msg = ChatMessage::Assistant {
            text: String::new(),
            tool_calls: vec![],
            provider_content: Some(ProviderContent {
                provider: ProviderId::Google,
                content: raw.clone(),
            }),
        };
        let body = encode(&req(vec![ChatMessage::user("x"), msg]));
        assert_eq!(body["contents"][1]["parts"], raw);

        let out = run(&[r#"{"candidates":[{"finishReason":"SAFETY"}]}"#]);
        assert_eq!(out.last(), Some(&StreamEvent::Done(StopReason::Refusal)));
    }
}
