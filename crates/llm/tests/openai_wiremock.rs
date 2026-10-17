//! Exercises the real reqwest path (headers, auth, SSE framing) against a
//! local mock server replaying a hand-written stream in OpenAI's documented
//! chunk format.

use futures::StreamExt;
use llm::{
    build_provider, ChatMessage, ChatRequest, LlmError, ProviderConfig, ProviderId, StopReason,
    StreamEvent, ToolCall, Usage,
};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(server: &MockServer, provider: ProviderId) -> ProviderConfig {
    ProviderConfig {
        provider,
        api_key: "sk-test".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        model: "test-model".into(),
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "test-model".into(),
        messages: vec![ChatMessage::user("orders in processing?")],
        tools: vec![],
        max_tokens: 256,
        temperature: None,
    }
}

#[tokio::test]
async fn openai_stream_replays_fixture_through_reqwest() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .and(body_partial_json(
            json!({"model": "test-model", "stream": true}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/openai_tool_call.sse"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        build_provider(&config(&server, ProviderId::OpenAi), reqwest::Client::new()).unwrap();
    let events: Vec<_> = provider
        .stream(request())
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect()
        .await;

    assert_eq!(
        events,
        vec![
            StreamEvent::TextDelta("Let me check.".into()),
            StreamEvent::ToolCall(ToolCall {
                id: "call_abc".into(),
                name: "shop_list_orders".into(),
                arguments: json!({"status": "processing"}),
            }),
            StreamEvent::Usage(Usage {
                input_tokens: 120,
                output_tokens: 18
            }),
            StreamEvent::Done(StopReason::ToolUse),
        ]
    );
}

#[tokio::test]
async fn unauthorized_maps_to_auth_error_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"error": {"message": "Incorrect API key"}})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(
        &config(&server, ProviderId::DeepSeek),
        reqwest::Client::new(),
    )
    .unwrap();
    let err = provider.stream(request()).await.err().unwrap();
    assert_eq!(err, LlmError::Auth("Incorrect API key".into()));
}
