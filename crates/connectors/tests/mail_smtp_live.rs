//! SMTP against a local mailpit (`docker compose --profile mail up -d mailpit`).
//! Ignored by default. MAILPIT_HOST overrides `localhost`.

use std::sync::Arc;

use connectors::{ConnectorKind, ConnectorRegistry, Credentials, ReqwestTransport};
use serde_json::{json, Value};
use tools::ToolContext;

#[tokio::test]
#[ignore = "needs a local mailpit"]
async fn emails_reach_mailpit_one_per_recipient() {
    let host = std::env::var("MAILPIT_HOST").unwrap_or_else(|_| "localhost".into());
    let api = format!("http://{host}:8025/api/v1/messages");
    let http = reqwest::Client::new();
    http.delete(&api).send().await.unwrap();

    let creds = Credentials::from([
        ("host".into(), host.clone()),
        ("port".into(), "1025".into()),
        ("security".into(), "none".into()),
        ("username".into(), "shop@example.com".into()),
        ("password".into(), "any".into()),
        ("from_email".into(), "shop@example.com".into()),
        ("from_name".into(), "Acme Shop".into()),
    ]);
    let smtp = ConnectorRegistry::build(
        ConnectorKind::Smtp,
        "",
        &creds,
        Arc::new(ReqwestTransport::default()),
    )
    .unwrap();
    smtp.verify().await.unwrap();
    let out = smtp.tools()[0]
        .call(
            json!({
                "to": ["ana@example.com", "jo@example.com"],
                "subject": "Your cart is waiting",
                "text": "You left 2 items in your cart.",
                "html": "<p>You left <b>2 items</b> in your cart.</p>",
                "reply_to": "help@example.com",
                "purpose": "transactional",
                "consent_confirmed": false
            }),
            &ToolContext::default(),
        )
        .await
        .unwrap();
    assert_eq!(out, r#"{"failed":[],"sent":2}"#);

    let list: Value = http.get(&api).send().await.unwrap().json().await.unwrap();
    let messages = list["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{list}");
    for m in messages {
        assert_eq!(m["Subject"], "Your cart is waiting");
        assert_eq!(m["From"]["Address"], "shop@example.com");
        assert_eq!(
            m["To"].as_array().unwrap().len(),
            1,
            "one recipient per message"
        );
    }
    let mut to: Vec<&str> = messages
        .iter()
        .map(|m| m["To"][0]["Address"].as_str().unwrap())
        .collect();
    to.sort();
    assert_eq!(to, ["ana@example.com", "jo@example.com"]);
}
