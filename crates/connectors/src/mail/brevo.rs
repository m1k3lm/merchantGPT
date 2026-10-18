//! Brevo API v3 (`https://api.brevo.com/v3`, `api-key` header): contact
//! lists, email campaigns and transactional email.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tools::{parse_args, Mutability, Tool, ToolContext, ToolDefinition, ToolError};

use super::{
    call, check_consent, check_contacts, check_emails, contacts_schema, purpose_schema, string_arg,
    text_to_html, to_json, Contact, Purpose,
};
use crate::transport::{HttpRequest, HttpTransport, Method};
use crate::{Connector, ConnectorError, ConnectorKind, Credentials};

pub const API_BASE: &str = "https://api.brevo.com/v3";

pub struct Client {
    base: String,
    key: String,
    sender_email: String,
    sender_name: Option<String>,
    transport: Arc<dyn HttpTransport>,
}

impl Client {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ConnectorError> {
        let mut req = HttpRequest::new(method, format!("{}{path}", self.base))
            .header("api-key", &self.key)
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req.json(&b);
        }
        call(self.transport.as_ref(), "Brevo", req).await
    }

    fn sender(&self) -> Value {
        match &self.sender_name {
            Some(name) => json!({"name": name, "email": self.sender_email}),
            None => json!({"email": self.sender_email}),
        }
    }
}

pub struct Brevo(Arc<Client>);

impl Brevo {
    pub fn new(credentials: &Credentials, transport: Arc<dyn HttpTransport>) -> Self {
        Self::with_base(API_BASE, credentials, transport)
    }

    pub fn with_base(
        base: &str,
        credentials: &Credentials,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self(Arc::new(Client {
            base: base.trim_end_matches('/').to_string(),
            key: string_arg(credentials, "api_key")
                .unwrap_or_default()
                .to_string(),
            sender_email: string_arg(credentials, "sender_email")
                .unwrap_or_default()
                .to_string(),
            sender_name: string_arg(credentials, "sender_name").map(str::to_string),
            transport,
        }))
    }
}

#[async_trait]
impl Connector for Brevo {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Brevo
    }

    async fn verify(&self) -> Result<(), ConnectorError> {
        self.0
            .request(Method::Get, "/account", None)
            .await
            .map(|_| ())
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        [
            Op::ListLists,
            Op::UpsertContacts,
            Op::CreateCampaign,
            Op::SendCampaign,
            Op::SendEmail,
        ]
        .into_iter()
        .map(|op| {
            Arc::new(BrevoTool {
                client: self.0.clone(),
                op,
            }) as Arc<dyn Tool>
        })
        .collect()
    }
}

#[derive(Clone, Copy)]
enum Op {
    ListLists,
    UpsertContacts,
    CreateCampaign,
    SendCampaign,
    SendEmail,
}

struct BrevoTool {
    client: Arc<Client>,
    op: Op,
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
}

#[derive(Deserialize)]
struct UpsertArgs {
    contacts: Vec<Contact>,
    #[serde(default)]
    list_ids: Vec<i64>,
    consent_confirmed: bool,
}

#[derive(Deserialize)]
struct CampaignArgs {
    name: String,
    subject: String,
    html: String,
    list_ids: Vec<i64>,
    #[serde(default)]
    scheduled_at: Option<String>,
}

#[derive(Deserialize)]
struct CampaignIdArgs {
    campaign_id: i64,
}

#[derive(Deserialize)]
struct SendArgs {
    to: Vec<String>,
    subject: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    purpose: Purpose,
    consent_confirmed: bool,
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

#[async_trait]
impl Tool for BrevoTool {
    fn definition(&self) -> ToolDefinition {
        let (name, description, parameters, mutability) = match self.op {
            Op::ListLists => (
                "brevo_list_lists",
                "List Brevo contact lists with their ids.",
                schema(
                    json!({"limit": {"type": "integer", "minimum": 1, "maximum": 50}, "offset": {"type": "integer", "minimum": 0}}),
                    &[],
                ),
                Mutability::Read,
            ),
            Op::UpsertContacts => (
                "brevo_upsert_contacts",
                "Create or update Brevo contacts (names go to FIRSTNAME/LASTNAME), optionally adding them to lists. Adding to lists needs consent_confirmed.",
                schema(
                    json!({
                        "contacts": contacts_schema(),
                        "list_ids": {"type": "array", "items": {"type": "integer"}},
                        "consent_confirmed": {"type": "boolean", "description": "true only if every contact opted in to marketing"}
                    }),
                    &["contacts", "consent_confirmed"],
                ),
                Mutability::Mutate,
            ),
            Op::CreateCampaign => (
                "brevo_create_campaign",
                "Create a Brevo email campaign for lists, as a draft unless scheduled_at is set.",
                schema(
                    json!({
                        "name": {"type": "string"}, "subject": {"type": "string"},
                        "html": {"type": "string"},
                        "list_ids": {"type": "array", "items": {"type": "integer"}, "minItems": 1},
                        "scheduled_at": {"type": "string", "description": "Optional, e.g. 2026-11-02T09:00:00+01:00"}
                    }),
                    &["name", "subject", "html", "list_ids"],
                ),
                Mutability::Mutate,
            ),
            Op::SendCampaign => (
                "brevo_send_campaign",
                "Send an existing Brevo email campaign now.",
                schema(json!({"campaign_id": {"type": "integer"}}), &["campaign_id"]),
                Mutability::Mutate,
            ),
            Op::SendEmail => {
                let mut props = json!({
                    "to": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": super::MAX_RECIPIENTS,
                        "description": "Recipients; each gets their own message"},
                    "subject": {"type": "string"},
                    "text": {"type": "string"}, "html": {"type": "string"},
                    "reply_to": {"type": "string"}
                });
                if let (Some(p), Some(e)) = (props.as_object_mut(), purpose_schema().as_object()) {
                    p.extend(e.clone());
                }
                (
                    "brevo_send_email",
                    "Send an individual email through Brevo (text or html).",
                    schema(props, &["to", "subject", "purpose", "consent_confirmed"]),
                    Mutability::Mutate,
                )
            }
        };
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters,
            mutability,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let c = &self.client;
        match self.op {
            Op::ListLists => {
                let a: ListArgs = parse_args(args)?;
                let path = format!(
                    "/contacts/lists?limit={}&offset={}",
                    a.limit.unwrap_or(50).clamp(1, 50),
                    a.offset.unwrap_or(0)
                );
                let v = c.request(Method::Get, &path, None).await?;
                let lists: Vec<Value> = v["lists"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|l| json!({"id": l["id"], "name": l["name"], "subscribers": l["uniqueSubscribers"]}))
                    .collect();
                to_json(json!({"lists": lists, "count": v["count"]}))
            }
            Op::UpsertContacts => {
                let a: UpsertArgs = parse_args(args)?;
                check_contacts(&a.contacts)?;
                if !a.list_ids.is_empty() && !a.consent_confirmed {
                    return Err(ToolError::InvalidArguments(
                        "adding contacts to lists needs consent_confirmed: only customers who opted in".into(),
                    ));
                }
                let mut done = 0;
                let mut failed = Vec::new();
                for contact in &a.contacts {
                    let mut attributes = serde_json::Map::new();
                    if let Some(f) = contact.first_name.as_deref().filter(|s| !s.is_empty()) {
                        attributes.insert("FIRSTNAME".into(), json!(f));
                    }
                    if let Some(l) = contact.last_name.as_deref().filter(|s| !s.is_empty()) {
                        attributes.insert("LASTNAME".into(), json!(l));
                    }
                    let mut body = json!({
                        "email": contact.email.trim(),
                        "updateEnabled": true,
                        "attributes": attributes
                    });
                    if !a.list_ids.is_empty() {
                        body["listIds"] = json!(a.list_ids);
                    }
                    match c.request(Method::Post, "/contacts", Some(body)).await {
                        Ok(_) => done += 1,
                        Err(e) => {
                            failed.push(json!({"email": contact.email, "error": e.to_string()}))
                        }
                    }
                }
                to_json(json!({"upserted": done, "failed": failed}))
            }
            Op::CreateCampaign => {
                let a: CampaignArgs = parse_args(args)?;
                if a.list_ids.is_empty() {
                    return Err(ToolError::InvalidArguments("list_ids is required".into()));
                }
                let mut body = json!({
                    "name": a.name,
                    "sender": c.sender(),
                    "subject": a.subject,
                    "htmlContent": a.html,
                    "recipients": {"listIds": a.list_ids}
                });
                if let Some(at) = a.scheduled_at.filter(|s| !s.trim().is_empty()) {
                    body["scheduledAt"] = json!(at);
                }
                let scheduled = body.get("scheduledAt").is_some();
                let v = c
                    .request(Method::Post, "/emailCampaigns", Some(body))
                    .await?;
                to_json(json!({"campaign_id": v["id"], "scheduled": scheduled}))
            }
            Op::SendCampaign => {
                let a: CampaignIdArgs = parse_args(args)?;
                c.request(
                    Method::Post,
                    &format!("/emailCampaigns/{}/sendNow", a.campaign_id),
                    None,
                )
                .await?;
                to_json(json!({"campaign_id": a.campaign_id, "sending": true}))
            }
            Op::SendEmail => {
                let a: SendArgs = parse_args(args)?;
                check_consent(a.purpose, a.consent_confirmed)?;
                let to: Vec<String> = a.to.iter().map(|t| t.trim().to_string()).collect();
                check_emails(&to, super::MAX_RECIPIENTS)?;
                let html = match (a.html.filter(|h| !h.trim().is_empty()), &a.text) {
                    (Some(h), _) => h,
                    (None, Some(t)) => text_to_html(t),
                    (None, None) => {
                        return Err(ToolError::InvalidArguments(
                            "text or html is required".into(),
                        ))
                    }
                };
                let mut sent = 0;
                let mut failed = Vec::new();
                // One request per recipient: Brevo shows every `to` address to all of them.
                for recipient in &to {
                    let mut body = json!({
                        "sender": c.sender(),
                        "to": [{"email": recipient}],
                        "subject": a.subject,
                        "htmlContent": html
                    });
                    if let Some(t) = &a.text {
                        body["textContent"] = json!(t);
                    }
                    if let Some(r) = a.reply_to.as_deref().filter(|r| !r.trim().is_empty()) {
                        body["replyTo"] = json!({"email": r.trim()});
                    }
                    match c.request(Method::Post, "/smtp/email", Some(body)).await {
                        Ok(_) => sent += 1,
                        Err(e) => failed.push(json!({"to": recipient, "error": e.to_string()})),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{FakeTransport, HttpResponse};

    fn brevo(t: &Arc<FakeTransport>) -> Brevo {
        let creds = Credentials::from([
            ("api_key".into(), "xkeysib-1".into()),
            ("sender_email".into(), "shop@example.com".into()),
            ("sender_name".into(), "Acme".into()),
        ]);
        Brevo::new(&creds, t.clone())
    }

    fn tool(b: &Brevo, name: &str) -> Arc<dyn Tool> {
        b.tools()
            .into_iter()
            .find(|t| t.definition().name == name)
            .unwrap()
    }

    fn body(t: &FakeTransport, i: usize) -> Value {
        serde_json::from_str(t.requests()[i].body.as_deref().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn verify_uses_the_api_key_header() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/v3/account",
            HttpResponse::new(200, r#"{"email":"a@b.c"}"#),
        );
        brevo(&t).verify().await.unwrap();
        assert_eq!(t.requests()[0].header_value("api-key"), Some("xkeysib-1"));
        let bad = Arc::new(FakeTransport::new());
        bad.on(
            Method::Get,
            "/v3/account",
            HttpResponse::new(401, r#"{"code":"unauthorized","message":"Key not found"}"#),
        );
        assert_eq!(
            brevo(&bad).verify().await.unwrap_err(),
            ConnectorError::Auth("Brevo: Key not found".into())
        );
    }

    #[tokio::test]
    async fn contacts_join_lists_only_with_consent() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/v3/contacts",
            HttpResponse::new(201, r#"{"id":21}"#),
        )
        .on(Method::Post, "/v3/contacts", HttpResponse::new(204, ""));
        let b = brevo(&t);
        let upsert = tool(&b, "brevo_upsert_contacts");
        let ctx = ToolContext::default();
        let refused = upsert
            .call(
                json!({"contacts": [{"email": "ana@example.com"}], "list_ids": [2], "consent_confirmed": false}),
                &ctx,
            )
            .await;
        assert!(matches!(refused, Err(ToolError::InvalidArguments(_))));
        let out = upsert
            .call(
                json!({"contacts": [{"email": "ana@example.com", "first_name": "Ana"}, {"email": "jo@example.com"}],
                    "list_ids": [2], "consent_confirmed": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out, r#"{"failed":[],"upserted":2}"#);
        assert_eq!(
            body(&t, 0),
            json!({"email": "ana@example.com", "updateEnabled": true,
                "attributes": {"FIRSTNAME": "Ana"}, "listIds": [2]})
        );
    }

    #[tokio::test]
    async fn campaigns_are_created_then_sent() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/v3/emailCampaigns",
            HttpResponse::new(201, r#"{"id":5}"#),
        );
        t.on(
            Method::Post,
            "/v3/emailCampaigns/5/sendNow",
            HttpResponse::new(204, ""),
        );
        let b = brevo(&t);
        let ctx = ToolContext::default();
        let out = tool(&b, "brevo_create_campaign")
            .call(
                json!({"name": "Black Friday", "subject": "-20%", "html": "<p>Deals inside</p>", "list_ids": [2]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains(r#""campaign_id":5"#), "{out}");
        assert_eq!(
            body(&t, 0),
            json!({"name": "Black Friday", "sender": {"name": "Acme", "email": "shop@example.com"},
                "subject": "-20%", "htmlContent": "<p>Deals inside</p>", "recipients": {"listIds": [2]}})
        );
        tool(&b, "brevo_send_campaign")
            .call(json!({"campaign_id": 5}), &ctx)
            .await
            .unwrap();
        assert_eq!(t.requests()[1].path(), "/v3/emailCampaigns/5/sendNow");
    }

    #[tokio::test]
    async fn transactional_email_goes_one_recipient_per_request() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/v3/smtp/email",
            HttpResponse::new(201, r#"{"messageId":"<1@x>"}"#),
        );
        let b = brevo(&t);
        let send = tool(&b, "brevo_send_email");
        let ctx = ToolContext::default();
        let out = send
            .call(
                json!({"to": ["ana@example.com", "jo@example.com"], "subject": "Your cart",
                    "text": "You left <3 items>", "reply_to": "help@example.com",
                    "purpose": "transactional", "consent_confirmed": false}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out, r#"{"failed":[],"sent":2}"#);
        let first = body(&t, 0);
        assert_eq!(first["to"], json!([{"email": "ana@example.com"}]));
        assert_eq!(first["replyTo"], json!({"email": "help@example.com"}));
        assert!(first["htmlContent"]
            .as_str()
            .unwrap()
            .contains("&lt;3 items&gt;"));
        let promo = send
            .call(
                json!({"to": ["ana@example.com"], "subject": "Sale", "text": "x",
                    "purpose": "marketing", "consent_confirmed": false}),
                &ctx,
            )
            .await;
        assert!(matches!(promo, Err(ToolError::InvalidArguments(_))));
    }
}
