//! Mailchimp Marketing API 3.0 with Basic auth (`anystring:<key>`). Keys end
//! in their data center (`-us21`), which picks the host; a key without one
//! uses the root host the current docs show.

use std::sync::Arc;

use async_trait::async_trait;
use base64::prelude::*;
use md5::{Digest, Md5};
use serde::Deserialize;
use serde_json::{json, Value};
use tools::{parse_args, Mutability, Tool, ToolContext, ToolDefinition, ToolError};

use super::{call, check_contacts, contacts_schema, string_arg, to_json, Contact};
use crate::transport::{HttpRequest, HttpTransport, Method};
use crate::{Connector, ConnectorError, ConnectorKind, Credentials};

pub struct Client {
    base: String,
    auth: String,
    from_name: String,
    reply_to: String,
    transport: Arc<dyn HttpTransport>,
}

/// `https://us21.api.mailchimp.com/3.0` for a key ending in `-us21`.
pub fn api_base(key: &str) -> Result<String, ConnectorError> {
    match key.rsplit_once('-') {
        Some((_, dc)) => {
            if dc.is_empty() || !dc.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(ConnectorError::InvalidCredentials(
                    "the Mailchimp API key should end in its data center, e.g. -us21".into(),
                ));
            }
            Ok(format!("https://{dc}.api.mailchimp.com/3.0"))
        }
        None => Ok("https://api.mailchimp.com/3.0".into()),
    }
}

/// Members are addressed by the MD5 of their lowercased email.
pub fn subscriber_hash(email: &str) -> String {
    Md5::digest(email.trim().to_lowercase().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Client {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ConnectorError> {
        let mut req = HttpRequest::new(method, format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req.json(&b);
        }
        call(self.transport.as_ref(), "Mailchimp", req).await
    }
}

pub struct Mailchimp(Arc<Client>);

impl Mailchimp {
    pub fn new(
        credentials: &Credentials,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, ConnectorError> {
        let key = string_arg(credentials, "api_key").unwrap_or_default();
        Self::with_base(&api_base(key)?, credentials, transport)
    }

    pub fn with_base(
        base: &str,
        credentials: &Credentials,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, ConnectorError> {
        let key = string_arg(credentials, "api_key").unwrap_or_default();
        Ok(Self(Arc::new(Client {
            base: base.trim_end_matches('/').to_string(),
            auth: format!(
                "Basic {}",
                BASE64_STANDARD.encode(format!("merchantgpt:{key}"))
            ),
            from_name: string_arg(credentials, "from_name")
                .unwrap_or_default()
                .to_string(),
            reply_to: string_arg(credentials, "reply_to")
                .unwrap_or_default()
                .to_string(),
            transport,
        })))
    }
}

#[async_trait]
impl Connector for Mailchimp {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Mailchimp
    }

    async fn verify(&self) -> Result<(), ConnectorError> {
        self.0.request(Method::Get, "/ping", None).await.map(|_| ())
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        [
            Op::ListAudiences,
            Op::UpsertMembers,
            Op::CreateCampaign,
            Op::SendCampaign,
        ]
        .into_iter()
        .map(|op| {
            Arc::new(MailchimpTool {
                client: self.0.clone(),
                op,
            }) as Arc<dyn Tool>
        })
        .collect()
    }
}

#[derive(Clone, Copy)]
enum Op {
    ListAudiences,
    UpsertMembers,
    CreateCampaign,
    SendCampaign,
}

struct MailchimpTool {
    client: Arc<Client>,
    op: Op,
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    count: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum NewStatus {
    Subscribed,
    Transactional,
    Pending,
}

impl NewStatus {
    fn as_str(self) -> &'static str {
        match self {
            NewStatus::Subscribed => "subscribed",
            NewStatus::Transactional => "transactional",
            NewStatus::Pending => "pending",
        }
    }
}

#[derive(Deserialize)]
struct UpsertArgs {
    audience_id: String,
    members: Vec<Contact>,
    status_if_new: NewStatus,
    consent_confirmed: bool,
}

#[derive(Deserialize)]
struct CampaignArgs {
    audience_id: String,
    subject: String,
    html: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    segment_id: Option<i64>,
}

#[derive(Deserialize)]
struct CampaignIdArgs {
    campaign_id: String,
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

#[async_trait]
impl Tool for MailchimpTool {
    fn definition(&self) -> ToolDefinition {
        let (name, description, parameters, mutability) = match self.op {
            Op::ListAudiences => (
                "mailchimp_list_audiences",
                "List Mailchimp audiences with their ids and member counts.",
                schema(
                    json!({"count": {"type": "integer", "minimum": 1, "maximum": 100}, "offset": {"type": "integer", "minimum": 0}}),
                    &[],
                ),
                Mutability::Read,
            ),
            Op::UpsertMembers => (
                "mailchimp_upsert_members",
                "Add or update audience members. status_if_new applies to new members: subscribed (needs consent_confirmed), pending (Mailchimp asks them to confirm) or transactional (no marketing).",
                schema(
                    json!({
                        "audience_id": {"type": "string"},
                        "members": contacts_schema(),
                        "status_if_new": {"type": "string", "enum": ["subscribed", "pending", "transactional"]},
                        "consent_confirmed": {"type": "boolean", "description": "true only if every member opted in to marketing"}
                    }),
                    &["audience_id", "members", "status_if_new", "consent_confirmed"],
                ),
                Mutability::Mutate,
            ),
            Op::CreateCampaign => (
                "mailchimp_create_campaign",
                "Create a draft Mailchimp email campaign for an audience (optionally a saved segment or tag id) with HTML content.",
                schema(
                    json!({
                        "audience_id": {"type": "string"}, "subject": {"type": "string"},
                        "html": {"type": "string"}, "title": {"type": "string"},
                        "segment_id": {"type": "integer"}
                    }),
                    &["audience_id", "subject", "html"],
                ),
                Mutability::Mutate,
            ),
            Op::SendCampaign => (
                "mailchimp_send_campaign",
                "Send a Mailchimp campaign now.",
                schema(json!({"campaign_id": {"type": "string"}}), &["campaign_id"]),
                Mutability::Mutate,
            ),
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
            Op::ListAudiences => {
                let a: ListArgs = parse_args(args)?;
                let path = format!(
                    "/lists?count={}&offset={}&fields=lists.id,lists.name,lists.stats.member_count,total_items",
                    a.count.unwrap_or(25).clamp(1, 100),
                    a.offset.unwrap_or(0)
                );
                let v = c.request(Method::Get, &path, None).await?;
                let lists: Vec<Value> = v["lists"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|l| json!({"id": l["id"], "name": l["name"], "members": l["stats"]["member_count"]}))
                    .collect();
                to_json(json!({"audiences": lists, "total": v["total_items"]}))
            }
            Op::UpsertMembers => {
                let a: UpsertArgs = parse_args(args)?;
                check_contacts(&a.members)?;
                if a.status_if_new == NewStatus::Subscribed && !a.consent_confirmed {
                    return Err(ToolError::InvalidArguments(
                        "subscribed needs consent_confirmed; use pending to let Mailchimp ask, or transactional".into(),
                    ));
                }
                let audience = path_segment(&a.audience_id)?;
                let mut done = 0;
                let mut failed = Vec::new();
                for m in &a.members {
                    let mut merge = serde_json::Map::new();
                    if let Some(f) = m.first_name.as_deref().filter(|s| !s.is_empty()) {
                        merge.insert("FNAME".into(), json!(f));
                    }
                    if let Some(l) = m.last_name.as_deref().filter(|s| !s.is_empty()) {
                        merge.insert("LNAME".into(), json!(l));
                    }
                    let mut body = json!({
                        "email_address": m.email.trim(),
                        "status_if_new": a.status_if_new.as_str()
                    });
                    if !merge.is_empty() {
                        body["merge_fields"] = Value::Object(merge);
                    }
                    let path = format!("/lists/{audience}/members/{}", subscriber_hash(&m.email));
                    match c.request(Method::Put, &path, Some(body)).await {
                        Ok(_) => done += 1,
                        Err(e) => failed.push(json!({"email": m.email, "error": e.to_string()})),
                    }
                }
                to_json(json!({"upserted": done, "failed": failed}))
            }
            Op::CreateCampaign => {
                let a: CampaignArgs = parse_args(args)?;
                if c.reply_to.is_empty() || c.from_name.is_empty() {
                    return Err(ToolError::Failed(
                        "set From name and Reply-to in the Mailchimp connector first".into(),
                    ));
                }
                let mut recipients = json!({"list_id": a.audience_id.trim()});
                if let Some(seg) = a.segment_id {
                    recipients["segment_opts"] = json!({"saved_segment_id": seg});
                }
                let body = json!({
                    "type": "regular",
                    "recipients": recipients,
                    "settings": {
                        "subject_line": a.subject,
                        "title": a.title.unwrap_or_else(|| a.subject.clone()),
                        "from_name": c.from_name,
                        "reply_to": c.reply_to
                    }
                });
                let created = c.request(Method::Post, "/campaigns", Some(body)).await?;
                let id = created["id"]
                    .as_str()
                    .ok_or_else(|| ToolError::Failed("Mailchimp returned no campaign id".into()))?
                    .to_string();
                c.request(
                    Method::Put,
                    &format!("/campaigns/{id}/content"),
                    Some(json!({"html": a.html})),
                )
                .await?;
                to_json(json!({"campaign_id": id, "web_id": created["web_id"], "status": "draft"}))
            }
            Op::SendCampaign => {
                let a: CampaignIdArgs = parse_args(args)?;
                let id = path_segment(&a.campaign_id)?;
                c.request(Method::Post, &format!("/campaigns/{id}/actions/send"), None)
                    .await?;
                to_json(json!({"campaign_id": id, "sending": true}))
            }
        }
    }
}

fn path_segment(id: &str) -> Result<&str, ToolError> {
    let id = id.trim();
    if !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(id)
    } else {
        Err(ToolError::InvalidArguments(format!(
            "{id:?} is not a Mailchimp id"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{FakeTransport, HttpResponse};

    fn creds() -> Credentials {
        Credentials::from([
            ("api_key".into(), "abc123-us21".into()),
            ("from_name".into(), "Acme".into()),
            ("reply_to".into(), "shop@example.com".into()),
        ])
    }

    fn mailchimp(t: &Arc<FakeTransport>) -> Mailchimp {
        Mailchimp::new(&creds(), t.clone()).unwrap()
    }

    fn tool(m: &Mailchimp, name: &str) -> Arc<dyn Tool> {
        m.tools()
            .into_iter()
            .find(|t| t.definition().name == name)
            .unwrap()
    }

    #[test]
    fn data_center_and_subscriber_hash() {
        assert_eq!(
            api_base("abc-us21").unwrap(),
            "https://us21.api.mailchimp.com/3.0"
        );
        assert_eq!(api_base("abc").unwrap(), "https://api.mailchimp.com/3.0");
        assert!(api_base("abc-us21/../x").is_err());
        // md5("urist.mcvankab@freddiesjokes.com"), Mailchimp's documented example.
        assert_eq!(
            subscriber_hash(" Urist.McVankab@FreddiesJokes.com "),
            "62eeb292278cc15f5817cb78f7790b08"
        );
    }

    #[tokio::test]
    async fn verify_pings_the_data_center_with_basic_auth() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/3.0/ping",
            HttpResponse::new(200, r#"{"health_status":"Everything's Chimpy!"}"#),
        );
        mailchimp(&t).verify().await.unwrap();
        let req = &t.requests()[0];
        assert!(req
            .url
            .starts_with("https://us21.api.mailchimp.com/3.0/ping"));
        assert_eq!(
            req.header_value("authorization"),
            Some(
                format!(
                    "Basic {}",
                    BASE64_STANDARD.encode("merchantgpt:abc123-us21")
                )
                .as_str()
            )
        );
    }

    #[tokio::test]
    async fn members_are_upserted_by_hash_and_subscribing_needs_consent() {
        let t = Arc::new(FakeTransport::new());
        let hash = subscriber_hash("ana@example.com");
        t.on(
            Method::Put,
            &format!("/3.0/lists/a1b2/members/{hash}"),
            HttpResponse::new(200, r#"{"id":"x"}"#),
        );
        let m = mailchimp(&t);
        let upsert = tool(&m, "mailchimp_upsert_members");
        let ctx = ToolContext::default();
        let refused = upsert
            .call(
                json!({"audience_id": "a1b2", "members": [{"email": "ana@example.com"}],
                    "status_if_new": "subscribed", "consent_confirmed": false}),
                &ctx,
            )
            .await;
        assert!(matches!(refused, Err(ToolError::InvalidArguments(_))));
        let out = upsert
            .call(
                json!({"audience_id": "a1b2", "members": [{"email": "Ana@Example.com", "first_name": "Ana"}],
                    "status_if_new": "pending", "consent_confirmed": false}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out, r#"{"failed":[],"upserted":1}"#);
        let body: Value = serde_json::from_str(t.requests()[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"email_address": "Ana@Example.com", "status_if_new": "pending", "merge_fields": {"FNAME": "Ana"}})
        );
    }

    #[tokio::test]
    async fn campaigns_get_settings_from_credentials_and_html_content() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/3.0/campaigns",
            HttpResponse::new(200, r#"{"id":"c42","web_id":7}"#),
        );
        t.on(
            Method::Put,
            "/3.0/campaigns/c42/content",
            HttpResponse::new(200, "{}"),
        );
        t.on(
            Method::Post,
            "/3.0/campaigns/c42/actions/send",
            HttpResponse::new(204, ""),
        );
        let m = mailchimp(&t);
        let ctx = ToolContext::default();
        let out = tool(&m, "mailchimp_create_campaign")
            .call(
                json!({"audience_id": "a1b2", "subject": "Back in stock", "html": "<p>Hi</p>", "segment_id": 9}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains(r#""campaign_id":"c42""#), "{out}");
        let create: Value = serde_json::from_str(t.requests()[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            create,
            json!({"type": "regular",
                "recipients": {"list_id": "a1b2", "segment_opts": {"saved_segment_id": 9}},
                "settings": {"subject_line": "Back in stock", "title": "Back in stock",
                    "from_name": "Acme", "reply_to": "shop@example.com"}})
        );
        let content: Value =
            serde_json::from_str(t.requests()[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(content, json!({"html": "<p>Hi</p>"}));
        tool(&m, "mailchimp_send_campaign")
            .call(json!({"campaign_id": "c42"}), &ctx)
            .await
            .unwrap();
        let bad = tool(&m, "mailchimp_send_campaign")
            .call(json!({"campaign_id": "../lists"}), &ctx)
            .await;
        assert!(matches!(bad, Err(ToolError::InvalidArguments(_))));
    }
}
