//! Email and marketing services linked to a shop: SMTP for one-to-one mail,
//! Brevo, Mailchimp and Klaviyo for lists and campaigns. Every send is a
//! Mutate tool, and marketing needs the merchant to confirm consent.

pub mod brevo;
pub mod klaviyo;
pub mod mailchimp;
pub mod smtp;

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tools::ToolError;

use crate::transport::{HttpRequest, HttpTransport};
use crate::{Connector, ConnectorError, ConnectorKind, Credentials};

/// Recipients per send call; larger audiences belong in a marketing service.
pub const MAX_RECIPIENTS: usize = 50;
/// Contacts per list call.
pub const MAX_CONTACTS: usize = 100;

pub fn build(
    kind: ConnectorKind,
    credentials: &Credentials,
    transport: Arc<dyn HttpTransport>,
) -> Result<Arc<dyn Connector>, ConnectorError> {
    Ok(match kind {
        ConnectorKind::Smtp => Arc::new(smtp::Smtp::from_credentials(credentials)?),
        ConnectorKind::Brevo => Arc::new(brevo::Brevo::new(credentials, transport)),
        ConnectorKind::Mailchimp => Arc::new(mailchimp::Mailchimp::new(credentials, transport)?),
        ConnectorKind::Klaviyo => Arc::new(klaviyo::Klaviyo::new(credentials, transport)),
        other => {
            return Err(ConnectorError::InvalidCredentials(format!(
                "{} is not an email service",
                other.display_name()
            )))
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    /// About the recipient's own order, cart or account.
    Transactional,
    Marketing,
}

pub fn purpose_schema() -> Value {
    json!({
        "purpose": {"type": "string", "enum": ["transactional", "marketing"],
            "description": "transactional: about the recipient's own order, cart or account; marketing: anything promotional"},
        "consent_confirmed": {"type": "boolean",
            "description": "true only if every recipient opted in to marketing (accepts_marketing, or the merchant confirms)"}
    })
}

pub fn check_consent(purpose: Purpose, consent_confirmed: bool) -> Result<(), ToolError> {
    if purpose == Purpose::Marketing && !consent_confirmed {
        return Err(ToolError::InvalidArguments(
            "marketing email needs consent_confirmed: send it only to customers who opted in"
                .into(),
        ));
    }
    Ok(())
}

/// Plain shape check; the service validates further.
pub fn check_emails(emails: &[String], max: usize) -> Result<(), ToolError> {
    if emails.is_empty() || emails.len() > max {
        return Err(ToolError::InvalidArguments(format!(
            "between 1 and {max} recipients per call"
        )));
    }
    for e in emails {
        let ok = e
            .split_once('@')
            .is_some_and(|(user, host)| !user.is_empty() && host.contains('.'))
            && !e.contains(char::is_whitespace)
            && !e.contains([',', ';', '<', '>']);
        if !ok {
            return Err(ToolError::InvalidArguments(format!(
                "{e:?} is not one email address"
            )));
        }
    }
    Ok(())
}

pub fn string_arg<'a>(credentials: &'a Credentials, key: &str) -> Option<&'a str> {
    credentials
        .get(key)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

/// The useful part of an error body: Brevo `message`, Mailchimp `detail`,
/// Klaviyo `errors[].detail`.
pub fn error_text(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.chars().take(300).collect();
    };
    let listed: Vec<String> = v["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let text = e["detail"].as_str().or(e["message"].as_str())?;
            Some(match e["field"].as_str().filter(|f| !f.is_empty()) {
                Some(f) => format!("{f}: {text}"),
                None => text.to_string(),
            })
        })
        .collect();
    let head = ["message", "detail", "title"]
        .iter()
        .find_map(|k| v[*k].as_str())
        .map(str::to_string);
    match (head, listed.is_empty()) {
        (Some(h), true) => h,
        (Some(h), false) => format!("{h} ({})", listed.join("; ")),
        (None, false) => listed.join("; "),
        (None, true) => body.chars().take(300).collect(),
    }
}

/// Sends a JSON API request and returns the parsed body (`Null` when empty).
pub async fn call(
    transport: &dyn HttpTransport,
    service: &str,
    req: HttpRequest,
) -> Result<Value, ConnectorError> {
    let resp = match transport.send(req).await {
        Ok(r) => r,
        // The shared transport error says "the shop"; this is another service.
        Err(ConnectorError::Transport(e)) => {
            return Err(ConnectorError::Unreachable(format!(
                "could not reach {service}: {e}"
            )))
        }
        Err(e) => return Err(e),
    };
    if !resp.is_success() {
        let message = format!("{service}: {}", error_text(&resp.body));
        return Err(match resp.status {
            401 | 403 => ConnectorError::Auth(message),
            404 => ConnectorError::NotFound(message),
            status => ConnectorError::Api { status, message },
        });
    }
    if resp.body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&resp.body).map_err(|e| {
        ConnectorError::InvalidResponse(format!("{service} answered with invalid JSON: {e}"))
    })
}

/// A minimal HTML body for services that require one.
pub fn text_to_html(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<html><body><p>{}</p></body></html>",
        escaped.replace("\n\n", "</p><p>").replace('\n', "<br>")
    )
}

pub fn to_json<T: serde::Serialize>(v: T) -> Result<String, ToolError> {
    serde_json::to_string(&v).map_err(|e| ToolError::Failed(e.to_string()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Contact {
    pub email: String,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
}

pub fn contacts_schema() -> Value {
    json!({
        "type": "array",
        "items": {"type": "object", "properties": {
            "email": {"type": "string"},
            "first_name": {"type": "string"},
            "last_name": {"type": "string"}
        }, "required": ["email"]}
    })
}

pub fn check_contacts(contacts: &[Contact]) -> Result<(), ToolError> {
    let emails: Vec<String> = contacts
        .iter()
        .map(|c| c.email.trim().to_string())
        .collect();
    check_emails(&emails, MAX_CONTACTS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marketing_needs_confirmed_consent() {
        assert!(check_consent(Purpose::Marketing, false).is_err());
        assert!(check_consent(Purpose::Marketing, true).is_ok());
        assert!(check_consent(Purpose::Transactional, false).is_ok());
    }

    #[test]
    fn recipients_are_single_addresses_within_the_limit() {
        assert!(check_emails(&["ana@shop.es".into()], 50).is_ok());
        for bad in ["ana", "a@b", "a@b.c, d@e.f", "Ana <ana@shop.es>", " a@b.c"] {
            assert!(check_emails(&[bad.into()], 50).is_err(), "{bad}");
        }
        assert!(check_emails(&[], 50).is_err());
        assert!(check_emails(&vec!["a@b.co".to_string(); 51], 50).is_err());
    }

    #[test]
    fn error_text_reads_each_services_shape() {
        assert_eq!(
            error_text(r#"{"code":"unauthorized","message":"Key not found"}"#),
            "Key not found"
        );
        assert_eq!(
            error_text(
                r#"{"title":"Invalid Resource","status":400,"detail":"Your merge fields were invalid.","errors":[{"field":"FNAME","message":"too long"}]}"#
            ),
            "Your merge fields were invalid. (FNAME: too long)"
        );
        assert_eq!(
            error_text(
                r#"{"errors":[{"id":"1","status":400,"code":"invalid","title":"Invalid input.","detail":"list not found"}]}"#
            ),
            "list not found"
        );
        assert_eq!(error_text("<html>502</html>"), "<html>502</html>");
    }

    #[test]
    fn text_becomes_escaped_html() {
        assert_eq!(
            text_to_html("Hi <Ana>\n\nYour cart & more\nBye"),
            "<html><body><p>Hi &lt;Ana&gt;</p><p>Your cart &amp; more<br>Bye</p></body></html>"
        );
    }
}
