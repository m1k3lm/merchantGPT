//! SMTP through the merchant's own mailbox. One message per recipient, so no
//! customer sees another's address.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lettre::message::header::ContentType;
use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials as SmtpCredentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::Deserialize;
use serde_json::{json, Value};
use tools::{parse_args, Mutability, Tool, ToolContext, ToolDefinition, ToolError};

use super::{check_consent, check_emails, purpose_schema, string_arg, to_json, Purpose};
use crate::{Connector, ConnectorError, ConnectorKind, Credentials};

#[derive(Debug, Clone, PartialEq)]
pub struct OutgoingEmail {
    pub from: Mailbox,
    pub to: String,
    pub reply_to: Option<String>,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
}

/// Sending behind a trait so tools are tested without a server.
#[async_trait]
pub trait Mailer: Send + Sync {
    async fn verify(&self) -> Result<(), String>;
    async fn send(&self, email: &OutgoingEmail) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Security {
    StartTls,
    Tls,
    None,
}

pub struct LettreMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl LettreMailer {
    fn new(credentials: &Credentials) -> Result<Self, ConnectorError> {
        let host = string_arg(credentials, "host")
            .ok_or_else(|| ConnectorError::InvalidCredentials("missing SMTP server".into()))?;
        let security = match string_arg(credentials, "security")
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None | Some("starttls") => Security::StartTls,
            Some("tls" | "ssl") => Security::Tls,
            Some("none") => Security::None,
            Some(other) => {
                return Err(ConnectorError::InvalidCredentials(format!(
                    "security {other:?}: use starttls, tls or none"
                )))
            }
        };
        let port = match string_arg(credentials, "port") {
            Some(p) => p.parse::<u16>().map_err(|_| {
                ConnectorError::InvalidCredentials(format!("{p:?} is not a port number"))
            })?,
            None => match security {
                Security::StartTls => 587,
                Security::Tls => 465,
                Security::None => 25,
            },
        };
        let builder = match security {
            Security::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
            Security::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
            Security::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
                host,
            )),
        }
        .map_err(|e| ConnectorError::InvalidCredentials(format!("SMTP server {host:?}: {e}")))?;
        let user = string_arg(credentials, "username").unwrap_or_default();
        let password = credentials.get("password").cloned().unwrap_or_default();
        let transport = builder
            .port(port)
            .credentials(SmtpCredentials::new(user.to_string(), password))
            .timeout(Some(Duration::from_secs(30)))
            .build();
        Ok(Self { transport })
    }
}

#[async_trait]
impl Mailer for LettreMailer {
    async fn verify(&self) -> Result<(), String> {
        match self.transport.test_connection().await {
            Ok(true) => Ok(()),
            Ok(false) => Err("the SMTP server did not answer".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn send(&self, email: &OutgoingEmail) -> Result<(), String> {
        let mut b = Message::builder()
            .from(email.from.clone())
            .to(email.to.parse::<Mailbox>().map_err(|e| e.to_string())?)
            .subject(email.subject.clone());
        if let Some(r) = &email.reply_to {
            b = b.reply_to(r.parse::<Mailbox>().map_err(|e| e.to_string())?);
        }
        let message = match &email.html {
            Some(html) => b.multipart(MultiPart::alternative_plain_html(
                email.text.clone(),
                html.clone(),
            )),
            None => b.header(ContentType::TEXT_PLAIN).body(email.text.clone()),
        }
        .map_err(|e| e.to_string())?;
        self.transport
            .send(message)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

pub struct Smtp {
    mailer: Arc<dyn Mailer>,
    from: Mailbox,
}

impl Smtp {
    pub fn from_credentials(credentials: &Credentials) -> Result<Self, ConnectorError> {
        let mailer = Arc::new(LettreMailer::new(credentials)?);
        Self::with_mailer(credentials, mailer)
    }

    pub fn with_mailer(
        credentials: &Credentials,
        mailer: Arc<dyn Mailer>,
    ) -> Result<Self, ConnectorError> {
        let email = string_arg(credentials, "from_email")
            .ok_or_else(|| ConnectorError::InvalidCredentials("missing From address".into()))?;
        let address = email.parse().map_err(|_| {
            ConnectorError::InvalidCredentials(format!("{email:?} is not an email address"))
        })?;
        let name = string_arg(credentials, "from_name").map(str::to_string);
        Ok(Self {
            mailer,
            from: Mailbox::new(name, address),
        })
    }
}

#[async_trait]
impl Connector for Smtp {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Smtp
    }

    async fn verify(&self) -> Result<(), ConnectorError> {
        self.mailer
            .verify()
            .await
            .map_err(|e| ConnectorError::Auth(format!("SMTP: {e}")))
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(SendTool {
            mailer: self.mailer.clone(),
            from: self.from.clone(),
        })]
    }
}

struct SendTool {
    mailer: Arc<dyn Mailer>,
    from: Mailbox,
}

#[derive(Deserialize)]
struct SendArgs {
    to: Vec<String>,
    subject: String,
    text: String,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    purpose: Purpose,
    consent_confirmed: bool,
}

#[async_trait]
impl Tool for SendTool {
    fn definition(&self) -> ToolDefinition {
        let mut props = json!({
            "to": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": super::MAX_RECIPIENTS,
                "description": "Recipients; each gets their own message"},
            "subject": {"type": "string"},
            "text": {"type": "string", "description": "Plain-text body"},
            "html": {"type": "string", "description": "Optional HTML body"},
            "reply_to": {"type": "string"}
        });
        if let (Some(p), Some(e)) = (props.as_object_mut(), purpose_schema().as_object()) {
            p.extend(e.clone());
        }
        ToolDefinition {
            name: "email_send".into(),
            description: format!(
                "Send an email from the merchant's mailbox ({}).",
                self.from.email
            ),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["to", "subject", "text", "purpose", "consent_confirmed"]
            }),
            mutability: Mutability::Mutate,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let a: SendArgs = parse_args(args)?;
        check_consent(a.purpose, a.consent_confirmed)?;
        let to: Vec<String> = a.to.iter().map(|t| t.trim().to_string()).collect();
        check_emails(&to, super::MAX_RECIPIENTS)?;
        let mut sent = 0;
        let mut failed = Vec::new();
        for recipient in &to {
            let email = OutgoingEmail {
                from: self.from.clone(),
                to: recipient.clone(),
                reply_to: a.reply_to.clone().filter(|r| !r.trim().is_empty()),
                subject: a.subject.clone(),
                text: a.text.clone(),
                html: a.html.clone().filter(|h| !h.trim().is_empty()),
            };
            match self.mailer.send(&email).await {
                Ok(()) => sent += 1,
                Err(e) => failed.push(json!({"to": recipient, "error": e})),
            }
        }
        if sent == 0 {
            return Err(ToolError::Failed(format!(
                "no email was sent: {}",
                to_json(&failed)?
            )));
        }
        to_json(json!({"sent": sent, "failed": failed}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeMailer {
        sent: Mutex<Vec<OutgoingEmail>>,
        reject: Option<&'static str>,
    }

    #[async_trait]
    impl Mailer for FakeMailer {
        async fn verify(&self) -> Result<(), String> {
            Err("535 5.7.8 Authentication credentials invalid".into())
        }
        async fn send(&self, email: &OutgoingEmail) -> Result<(), String> {
            if Some(email.to.as_str()) == self.reject {
                return Err("550 mailbox unavailable".into());
            }
            self.sent.lock().unwrap().push(email.clone());
            Ok(())
        }
    }

    fn creds() -> Credentials {
        Credentials::from([
            ("host".into(), "smtp.example.com".into()),
            ("username".into(), "shop@example.com".into()),
            ("password".into(), "pw".into()),
            ("from_email".into(), "shop@example.com".into()),
            ("from_name".into(), "Acme Shop".into()),
        ])
    }

    fn smtp(mailer: Arc<FakeMailer>) -> Smtp {
        Smtp::with_mailer(&creds(), mailer).unwrap()
    }

    #[tokio::test]
    async fn each_recipient_gets_their_own_message() {
        let mailer = Arc::new(FakeMailer {
            reject: Some("gone@example.com"),
            ..Default::default()
        });
        let tool = smtp(mailer.clone()).tools().remove(0);
        assert_eq!(tool.definition().mutability, Mutability::Mutate);
        let out = tool
            .call(
                json!({
                    "to": ["ana@example.com", "gone@example.com", "jo@example.com"],
                    "subject": "Your cart is waiting",
                    "text": "Hi!",
                    "html": "<p>Hi!</p>",
                    "purpose": "transactional",
                    "consent_confirmed": false
                }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["sent"], 2);
        assert_eq!(v["failed"][0]["to"], "gone@example.com");
        let sent = mailer.sent.lock().unwrap();
        assert_eq!(sent[0].to, "ana@example.com");
        assert_eq!(sent[0].from.to_string(), "Acme Shop <shop@example.com>");
        assert_eq!(sent[0].html.as_deref(), Some("<p>Hi!</p>"));
    }

    #[tokio::test]
    async fn marketing_without_consent_is_refused() {
        let mailer = Arc::new(FakeMailer::default());
        let tool = smtp(mailer.clone()).tools().remove(0);
        let r = tool
            .call(
                json!({"to": ["ana@example.com"], "subject": "Sale", "text": "20% off",
                    "purpose": "marketing", "consent_confirmed": false}),
                &ToolContext::default(),
            )
            .await;
        assert!(matches!(r, Err(ToolError::InvalidArguments(_))));
        let multi = tool
            .call(
                json!({"to": ["a@x.com, b@y.com"], "subject": "s", "text": "t",
                    "purpose": "transactional", "consent_confirmed": false}),
                &ToolContext::default(),
            )
            .await;
        assert!(matches!(multi, Err(ToolError::InvalidArguments(_))));
        assert!(mailer.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn verify_failures_are_auth_errors() {
        let err = smtp(Arc::new(FakeMailer::default()))
            .verify()
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(m) if m.contains("535")));
    }

    // The pooled transport may need a runtime when built.
    #[tokio::test]
    async fn credentials_pick_security_and_port() {
        let mut c = creds();
        c.insert("security".into(), "carrier-pigeon".into());
        assert!(LettreMailer::new(&c).is_err());
        c.insert("security".into(), "none".into());
        c.insert("port".into(), "1025".into());
        assert!(LettreMailer::new(&c).is_ok());
        c.insert("port".into(), "smtp".into());
        assert!(LettreMailer::new(&c).is_err());
        let mut bad_from = creds();
        bad_from.insert("from_email".into(), "not an address".into());
        assert!(Smtp::with_mailer(&bad_from, Arc::new(FakeMailer::default())).is_err());
    }
}
