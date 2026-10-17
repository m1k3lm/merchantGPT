//! Shared HTTP plumbing for provider clients: bounded retries before the
//! stream starts and status-code to `LlmError` mapping.

use std::time::Duration;

use crate::LlmError;

const MAX_RETRIES: u32 = 2;

/// Sends the request built by `build`, retrying 429/5xx with backoff. Retries
/// only happen before any streamed bytes are consumed, so they are safe.
pub(crate) async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, LlmError> {
    let mut attempt = 0;
    loop {
        let result = build().send().await;
        let retry = match &result {
            Ok(r) => matches!(r.status().as_u16(), 429 | 500 | 502 | 503 | 504 | 529),
            Err(e) => e.is_connect() || e.is_timeout(),
        };
        if !retry || attempt >= MAX_RETRIES {
            let resp = result.map_err(|e| LlmError::Transport(e.without_url().to_string()))?;
            return check_status(resp).await;
        }
        tokio::time::sleep(Duration::from_millis(1000 * 2u64.pow(attempt))).await;
        attempt += 1;
    }
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, LlmError> {
    let status = resp.status().as_u16();
    if resp.status().is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let message = error_message(&body);
    Err(match status {
        401 | 403 => LlmError::Auth(message),
        429 => LlmError::RateLimited(message),
        _ => LlmError::Http { status, message },
    })
}

/// Pulls a human-readable message out of the error shapes the providers use.
fn error_message(body: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return body.chars().take(500).collect(),
    };
    let found = [
        v.pointer("/error/message"),
        v.pointer("/0/error/message"),
        v.get("message"),
    ]
    .into_iter()
    .flatten()
    .find_map(|m| m.as_str().map(str::to_string));
    found.unwrap_or_else(|| body.chars().take(500).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_message_handles_provider_shapes() {
        assert_eq!(
            error_message(r#"{"error":{"message":"bad key"}}"#),
            "bad key"
        );
        assert_eq!(error_message(r#"[{"error":{"message":"quota"}}]"#), "quota");
        assert_eq!(error_message(r#"{"message":"nope"}"#), "nope");
        assert_eq!(error_message("plain text"), "plain text");
    }
}
