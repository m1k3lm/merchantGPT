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
        options: Default::default(),
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
        options: Default::default(),
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

fn bedrock_cfg(server: &MockServer, options: &[(&str, &str)]) -> ProviderConfig {
    ProviderConfig {
        provider: ProviderId::Bedrock,
        api_key: "bedrock-secret".into(),
        base_url: Some(format!("{}/anthropic", server.uri())),
        model: "anthropic.claude-opus-5-5".into(),
        options: options
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

async fn mount_bedrock(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_partial_json(
            json!({"model": "anthropic.claude-opus-5-5", "stream": true}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/anthropic_tool_use.sse"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn bedrock_api_key_goes_in_x_api_key() {
    let server = MockServer::start().await;
    mount_bedrock(&server).await;
    let events = collect(
        bedrock_cfg(&server, &[(llm::OPT_REGION, "eu-west-1")]),
        "anthropic.claude-opus-5-5",
    )
    .await;
    assert_eq!(events.last(), Some(&StreamEvent::Done(StopReason::ToolUse)));
    let sent = &server.received_requests().await.unwrap()[0];
    assert_eq!(sent.headers.get("x-api-key").unwrap(), "bedrock-secret");
    assert!(sent.headers.get("authorization").is_none());
    let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert!(body.get("fallbacks").is_none(), "not available on Bedrock");
}

#[tokio::test]
async fn bedrock_access_keys_are_sigv4_signed() {
    let server = MockServer::start().await;
    mount_bedrock(&server).await;
    collect(
        bedrock_cfg(
            &server,
            &[
                (llm::OPT_REGION, "eu-west-1"),
                (llm::OPT_AWS_ACCESS_KEY_ID, "AKIATEST"),
                (llm::OPT_AWS_SESSION_TOKEN, "session-tok"),
            ],
        ),
        "anthropic.claude-opus-5-5",
    )
    .await;
    let sent = &server.received_requests().await.unwrap()[0];
    let auth = sent.headers.get("authorization").unwrap().to_str().unwrap();
    let date = sent.headers.get("x-amz-date").unwrap().to_str().unwrap();
    assert!(
        auth.starts_with(&format!(
            "AWS4-HMAC-SHA256 Credential=AKIATEST/{}/eu-west-1/bedrock-mantle/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature="
        , &date[..8])),
        "{auth}"
    );
    assert_eq!(
        sent.headers.get("x-amz-security-token").unwrap(),
        "session-tok"
    );
    assert_eq!(
        sent.headers
            .get("x-amz-content-sha256")
            .unwrap()
            .to_str()
            .unwrap(),
        llm::sigv4::sha256_hex(&sent.body)
    );
    assert!(sent.headers.get("x-api-key").is_none());
    assert!(!auth.contains("bedrock-secret"));
}
