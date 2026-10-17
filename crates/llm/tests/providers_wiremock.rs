//! Anthropic and Gemini through the real reqwest path, replaying hand-written
//! streams in each provider's documented SSE format.

use futures::StreamExt;
use llm::{
    build_provider, ChatMessage, ChatRequest, ProviderConfig, ProviderId, StopReason, StreamEvent,
    ToolCall, Usage,
};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![
            ChatMessage::system("You are merchantGPT."),
            ChatMessage::user("orders in processing?"),
        ],
        tools: vec![],
        tool_choice: Default::default(),
        max_tokens: 1024,
        temperature: None,
    }
}

async fn collect(cfg: ProviderConfig, model: &str) -> Vec<StreamEvent> {
    build_provider(&cfg, reqwest::Client::new())
        .unwrap()
        .stream(request(model))
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect()
        .await
}

#[tokio::test]
async fn anthropic_stream_through_reqwest() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_partial_json(json!({
            "model": "claude-opus-5-5",
            "stream": true,
            "system": "You are merchantGPT.",
            "max_tokens": 1024
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/anthropic_tool_use.sse"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = ProviderConfig {
        provider: ProviderId::Anthropic,
        api_key: "sk-ant-test".into(),
        base_url: Some(server.uri()),
        model: "claude-opus-5-5".into(),
    };
    let events = collect(cfg, "claude-opus-5-5").await;
    assert_eq!(events[0], StreamEvent::TextDelta("Let me check.".into()));
    assert_eq!(
        events[1],
        StreamEvent::ToolCall(ToolCall {
            id: "toolu_01".into(),
            name: "shop_list_orders".into(),
            arguments: json!({"status": "processing"}),
        })
    );
    assert_eq!(
        events[2],
        StreamEvent::Usage(Usage {
            input_tokens: 210,
            output_tokens: 64
        })
    );
    assert!(
        matches!(&events[3], StreamEvent::ProviderContent(c) if c[0]["signature"] == "EqQBCkYIARgCIkB")
    );
    assert_eq!(events[4], StreamEvent::Done(StopReason::ToolUse));

    let sent = &server.received_requests().await.unwrap()[0];
    assert!(
        !sent.headers.contains_key("anthropic-beta"),
        "no beta fallbacks through a custom base URL"
    );
}

#[tokio::test]
async fn gemini_stream_through_reqwest() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-test:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .and(header("x-goog-api-key", "g-key"))
        .and(body_partial_json(json!({
            "systemInstruction": {"parts": [{"text": "You are merchantGPT."}]},
            "generationConfig": {"maxOutputTokens": 1024}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/gemini_function_call.sse"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = ProviderConfig {
        provider: ProviderId::Google,
        api_key: "g-key".into(),
        base_url: Some(server.uri()),
        model: "gemini-test".into(),
    };
    let events = collect(cfg, "gemini-test").await;
    assert_eq!(events[0], StreamEvent::TextDelta("Let me check.".into()));
    assert!(
        matches!(&events[1], StreamEvent::ToolCall(c) if c.name == "shop_list_orders" && c.arguments == json!({"status": "processing"}))
    );
    assert_eq!(
        events[2],
        StreamEvent::Usage(Usage {
            input_tokens: 180,
            output_tokens: 60
        })
    );
    assert!(
        matches!(&events[3], StreamEvent::ProviderContent(p) if p[1]["thoughtSignature"] == "CiQBVKhc")
    );
    assert_eq!(events[4], StreamEvent::Done(StopReason::ToolUse));
}
