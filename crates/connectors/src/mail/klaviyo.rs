//! Klaviyo API (`https://a.klaviyo.com/api`, JSON:API, private key) pinned to
//! one revision. Campaigns take their HTML through a code template created
//! first, then referenced by id.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tools::{parse_args, Mutability, Tool, ToolContext, ToolDefinition, ToolError};

use super::{call, check_contacts, contacts_schema, string_arg, to_json, Contact};
use crate::transport::{HttpRequest, HttpTransport, Method};
use crate::{Connector, ConnectorError, ConnectorKind, Credentials};

pub const API_BASE: &str = "https://a.klaviyo.com/api";
pub const REVISION: &str = "2026-10-15";
const JSON_API: &str = "application/vnd.api+json";

pub struct Client {
    base: String,
    key: String,
    from_email: Option<String>,
    from_name: Option<String>,
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
            .header("Authorization", format!("Klaviyo-API-Key {}", self.key))
            .header("revision", REVISION)
            .header("Accept", JSON_API);
        if let Some(b) = body {
            req.body = Some(b.to_string());
            req = req.header("Content-Type", JSON_API);
        }
        call(self.transport.as_ref(), "Klaviyo", req).await
    }
}

pub struct Klaviyo(Arc<Client>);

impl Klaviyo {
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
            from_email: string_arg(credentials, "from_email").map(str::to_string),
            from_name: string_arg(credentials, "from_name").map(str::to_string),
            transport,
        }))
    }
}

#[async_trait]
impl Connector for Klaviyo {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Klaviyo
    }

    async fn verify(&self) -> Result<(), ConnectorError> {
        self.0
            .request(Method::Get, "/accounts", None)
            .await
            .map(|_| ())
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        [
            Op::ListLists,
            Op::Subscribe,
            Op::AddToList,
            Op::CreateCampaign,
            Op::SendCampaign,
        ]
        .into_iter()
        .map(|op| {
            Arc::new(KlaviyoTool {
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
    Subscribe,
    AddToList,
    CreateCampaign,
    SendCampaign,
}

struct KlaviyoTool {
    client: Arc<Client>,
    op: Op,
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Deserialize)]
struct SubscribeArgs {
    list_id: String,
    profiles: Vec<Contact>,
    consent_confirmed: bool,
}

#[derive(Deserialize)]
struct AddArgs {
    list_id: String,
    profiles: Vec<Contact>,
}

#[derive(Deserialize)]
struct CampaignArgs {
    name: String,
    list_id: String,
    subject: String,
    html: String,
    #[serde(default)]
    preview_text: Option<String>,
}

#[derive(Deserialize)]
struct CampaignIdArgs {
    campaign_id: String,
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

fn id(raw: &str, what: &str) -> Result<String, ToolError> {
    let raw = raw.trim();
    if !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(raw.to_string())
    } else {
        Err(ToolError::InvalidArguments(format!(
            "{raw:?} is not a Klaviyo {what} id"
        )))
    }
}

fn profile(c: &Contact, with_names: bool) -> Value {
    let mut attributes = json!({"email": c.email.trim()});
    if with_names {
        if let Some(f) = c.first_name.as_deref().filter(|s| !s.is_empty()) {
            attributes["first_name"] = json!(f);
        }
        if let Some(l) = c.last_name.as_deref().filter(|s| !s.is_empty()) {
            attributes["last_name"] = json!(l);
        }
    }
    json!({"type": "profile", "attributes": attributes})
}

/// The `page[cursor]` value of a JSON:API `links.next` URL.
fn next_cursor(v: &Value) -> Option<String> {
    let next = v["links"]["next"].as_str()?;
    reqwest::Url::parse(next)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == "page[cursor]")
        .map(|(_, v)| v.into_owned())
}

#[async_trait]
impl Tool for KlaviyoTool {
    fn definition(&self) -> ToolDefinition {
        let (name, description, parameters, mutability) = match self.op {
            Op::ListLists => (
                "klaviyo_list_lists",
                "List Klaviyo lists with their ids (10 per page; pass next_cursor for more).",
                schema(json!({"cursor": {"type": "string"}}), &[]),
                Mutability::Read,
            ),
            Op::Subscribe => (
                "klaviyo_subscribe_profiles",
                "Subscribe profiles (by email) to email marketing on a list. Needs consent_confirmed; the list's double opt-in setting applies.",
                schema(
                    json!({
                        "list_id": {"type": "string"},
                        "profiles": contacts_schema(),
                        "consent_confirmed": {"type": "boolean", "description": "true only if every profile opted in to marketing"}
                    }),
                    &["list_id", "profiles", "consent_confirmed"],
                ),
                Mutability::Mutate,
            ),
            Op::AddToList => (
                "klaviyo_add_profiles_to_list",
                "Create or update profiles and add them to a list without changing their marketing consent.",
                schema(
                    json!({"list_id": {"type": "string"}, "profiles": contacts_schema()}),
                    &["list_id", "profiles"],
                ),
                Mutability::Mutate,
            ),
            Op::CreateCampaign => (
                "klaviyo_create_campaign",
                "Create a draft Klaviyo email campaign for a list with HTML content.",
                schema(
                    json!({
                        "name": {"type": "string"}, "list_id": {"type": "string"},
                        "subject": {"type": "string"}, "html": {"type": "string"},
                        "preview_text": {"type": "string"}
                    }),
                    &["name", "list_id", "subject", "html"],
                ),
                Mutability::Mutate,
            ),
            Op::SendCampaign => (
                "klaviyo_send_campaign",
                "Send a Klaviyo campaign now.",
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
            Op::ListLists => {
                let a: ListArgs = parse_args(args)?;
                let mut url = reqwest::Url::parse("http://x/lists").expect("static URL");
                url.query_pairs_mut()
                    .append_pair("fields[list]", "name,created");
                if let Some(cur) = a.cursor.filter(|s| !s.is_empty()) {
                    url.query_pairs_mut().append_pair("page[cursor]", &cur);
                }
                let path = format!("/lists?{}", url.query().unwrap_or_default());
                let v = c.request(Method::Get, &path, None).await?;
                let lists: Vec<Value> = v["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|l| json!({"id": l["id"], "name": l["attributes"]["name"]}))
                    .collect();
                to_json(json!({"lists": lists, "next_cursor": next_cursor(&v)}))
            }
            Op::Subscribe => {
                let a: SubscribeArgs = parse_args(args)?;
                if !a.consent_confirmed {
                    return Err(ToolError::InvalidArguments(
                        "subscribing needs consent_confirmed; use klaviyo_add_profiles_to_list to add without marketing consent".into(),
                    ));
                }
                check_contacts(&a.profiles)?;
                let list = id(&a.list_id, "list")?;
                let profiles: Vec<Value> = a
                    .profiles
                    .iter()
                    .map(|p| {
                        let mut v = profile(p, false);
                        v["attributes"]["subscriptions"] =
                            json!({"email": {"marketing": {"consent": "SUBSCRIBED"}}});
                        v
                    })
                    .collect();
                let body = json!({"data": {
                    "type": "profile-subscription-bulk-create-job",
                    "attributes": {"custom_source": "merchantGPT", "profiles": {"data": profiles}},
                    "relationships": {"list": {"data": {"type": "list", "id": list}}}
                }});
                c.request(
                    Method::Post,
                    "/profile-subscription-bulk-create-jobs",
                    Some(body),
                )
                .await?;
                to_json(json!({"queued": a.profiles.len(), "list_id": list}))
            }
            Op::AddToList => {
                let a: AddArgs = parse_args(args)?;
                check_contacts(&a.profiles)?;
                let list = id(&a.list_id, "list")?;
                let profiles: Vec<Value> = a.profiles.iter().map(|p| profile(p, true)).collect();
                let body = json!({"data": {
                    "type": "profile-bulk-import-job",
                    "attributes": {"profiles": {"data": profiles}},
                    "relationships": {"lists": {"data": [{"type": "list", "id": list}]}}
                }});
                let v = c
                    .request(Method::Post, "/profile-bulk-import-jobs", Some(body))
                    .await?;
                to_json(json!({"queued": a.profiles.len(), "job_id": v["data"]["id"]}))
            }
            Op::CreateCampaign => {
                let a: CampaignArgs = parse_args(args)?;
                let from_email = c.from_email.clone().ok_or_else(|| {
                    ToolError::Failed("set a From email in the Klaviyo connector first".into())
                })?;
                let list = id(&a.list_id, "list")?;
                let template = c
                    .request(
                        Method::Post,
                        "/templates",
                        Some(json!({"data": {"type": "template", "attributes": {
                            "name": format!("{} (merchantGPT)", a.name),
                            "editor_type": "CODE",
                            "html": a.html
                        }}})),
                    )
                    .await?;
                let template_id = template["data"]["id"]
                    .as_str()
                    .ok_or_else(|| ToolError::Failed("Klaviyo returned no template id".into()))?;
                let mut details = json!({
                    "channel": "email",
                    "template_id": template_id,
                    "from_email": from_email,
                    "subject": a.subject
                });
                if let Some(n) = &c.from_name {
                    details["from_label"] = json!(n);
                }
                if let Some(p) = a.preview_text.filter(|p| !p.is_empty()) {
                    details["preview_text"] = json!(p);
                }
                let body = json!({"data": {"type": "campaign", "attributes": {
                    "definition": {
                        "name": a.name,
                        "send_settings": {"send_strategy": "simple", "send_timezone": "UTC"}
                    },
                    "audiences": [{"temporary_id": "audience", "included": [list]}],
                    "messages": [{
                        "audience_temporary_id": "audience",
                        "variations": [{"definition": {"details": details}}]
                    }]
                }}});
                let v = c.request(Method::Post, "/campaigns", Some(body)).await?;
                to_json(
                    json!({"campaign_id": v["data"]["id"], "template_id": template_id, "status": "draft"}),
                )
            }
            Op::SendCampaign => {
                let a: CampaignIdArgs = parse_args(args)?;
                let campaign = id(&a.campaign_id, "campaign")?;
                let body = json!({"data": {"type": "campaign-send-job", "id": campaign}});
                let v = c
                    .request(Method::Post, "/campaign-send-jobs", Some(body))
                    .await?;
                to_json(
                    json!({"campaign_id": campaign, "status": v["data"]["attributes"]["status"]}),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{FakeTransport, HttpResponse};

    fn klaviyo(t: &Arc<FakeTransport>, from: bool) -> Klaviyo {
        let mut creds = Credentials::from([("api_key".into(), "pk_test".into())]);
        if from {
            creds.insert("from_email".into(), "shop@example.com".into());
            creds.insert("from_name".into(), "Acme".into());
        }
        Klaviyo::new(&creds, t.clone())
    }

    fn tool(k: &Klaviyo, name: &str) -> Arc<dyn Tool> {
        k.tools()
            .into_iter()
            .find(|t| t.definition().name == name)
            .unwrap()
    }

    fn body(t: &FakeTransport, i: usize) -> Value {
        serde_json::from_str(t.requests()[i].body.as_deref().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn verify_sends_key_and_pinned_revision() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/api/accounts",
            HttpResponse::new(200, r#"{"data":[]}"#),
        );
        klaviyo(&t, false).verify().await.unwrap();
        let req = &t.requests()[0];
        assert_eq!(
            req.header_value("authorization"),
            Some("Klaviyo-API-Key pk_test")
        );
        assert_eq!(req.header_value("revision"), Some(REVISION));
    }

    #[tokio::test]
    async fn lists_page_with_cursors() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/api/lists",
            HttpResponse::new(
                200,
                r#"{"data":[{"type":"list","id":"Y6nRLr","attributes":{"name":"Newsletter"}}],
                    "links":{"self":"x","next":"https://a.klaviyo.com/api/lists?page%5Bcursor%5D=bmV4dA"}}"#,
            ),
        );
        let out = tool(&klaviyo(&t, false), "klaviyo_list_lists")
            .call(json!({"cursor": "abc"}), &ToolContext::default())
            .await
            .unwrap();
        assert_eq!(
            out,
            r#"{"lists":[{"id":"Y6nRLr","name":"Newsletter"}],"next_cursor":"bmV4dA"}"#
        );
        let q = t.requests()[0].query_pairs();
        assert!(q.contains(&("page[cursor]".into(), "abc".into())), "{q:?}");
    }

    #[tokio::test]
    async fn subscribing_needs_consent_while_adding_to_a_list_does_not() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/api/profile-subscription-bulk-create-jobs",
            HttpResponse::new(202, ""),
        );
        t.on(
            Method::Post,
            "/api/profile-bulk-import-jobs",
            HttpResponse::new(
                202,
                r#"{"data":{"type":"profile-bulk-import-job","id":"job1"}}"#,
            ),
        );
        let k = klaviyo(&t, false);
        let ctx = ToolContext::default();
        let profiles = json!([{"email": "ana@example.com", "first_name": "Ana"}]);
        let refused = tool(&k, "klaviyo_subscribe_profiles")
            .call(
                json!({"list_id": "Y6nRLr", "profiles": profiles, "consent_confirmed": false}),
                &ctx,
            )
            .await;
        assert!(matches!(refused, Err(ToolError::InvalidArguments(_))));
        tool(&k, "klaviyo_subscribe_profiles")
            .call(
                json!({"list_id": "Y6nRLr", "profiles": profiles, "consent_confirmed": true}),
                &ctx,
            )
            .await
            .unwrap();
        let sub = body(&t, 0);
        assert_eq!(sub["data"]["type"], "profile-subscription-bulk-create-job");
        assert_eq!(
            sub["data"]["attributes"]["profiles"]["data"][0]["attributes"],
            json!({"email": "ana@example.com", "subscriptions": {"email": {"marketing": {"consent": "SUBSCRIBED"}}}})
        );
        assert_eq!(sub["data"]["relationships"]["list"]["data"]["id"], "Y6nRLr");
        assert_eq!(
            t.requests()[0].header_value("content-type"),
            Some("application/vnd.api+json")
        );
        let out = tool(&k, "klaviyo_add_profiles_to_list")
            .call(json!({"list_id": "Y6nRLr", "profiles": profiles}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("job1"));
        let import = body(&t, 1);
        assert_eq!(
            import["data"]["attributes"]["profiles"]["data"][0]["attributes"],
            json!({"email": "ana@example.com", "first_name": "Ana"})
        );
    }

    #[tokio::test]
    async fn campaigns_create_a_template_then_reference_it() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/api/templates",
            HttpResponse::new(201, r#"{"data":{"type":"template","id":"T1"}}"#),
        );
        t.on(
            Method::Post,
            "/api/campaigns",
            HttpResponse::new(201, r#"{"data":{"type":"campaign","id":"C1"}}"#),
        );
        t.on(
            Method::Post,
            "/api/campaign-send-jobs",
            HttpResponse::new(202, r#"{"data":{"type":"campaign-send-job","id":"C1","attributes":{"status":"queued"}}}"#),
        );
        let ctx = ToolContext::default();
        let no_from = tool(&klaviyo(&t, false), "klaviyo_create_campaign")
            .call(
                json!({"name": "N", "list_id": "L1", "subject": "S", "html": "<p>x</p>"}),
                &ctx,
            )
            .await;
        assert!(matches!(no_from, Err(ToolError::Failed(_))));
        let k = klaviyo(&t, true);
        let out = tool(&k, "klaviyo_create_campaign")
            .call(json!({"name": "Winback", "list_id": "L1", "subject": "We miss you", "html": "<p>Hi</p>"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains(r#""campaign_id":"C1""#), "{out}");
        assert_eq!(
            body(&t, 0)["data"]["attributes"],
            json!({"name": "Winback (merchantGPT)", "editor_type": "CODE", "html": "<p>Hi</p>"})
        );
        let campaign = body(&t, 1);
        let attrs = &campaign["data"]["attributes"];
        assert_eq!(attrs["audiences"][0]["included"], json!(["L1"]));
        assert_eq!(
            attrs["messages"][0]["variations"][0]["definition"]["details"],
            json!({"channel": "email", "template_id": "T1", "from_email": "shop@example.com",
                "from_label": "Acme", "subject": "We miss you"})
        );
        let sent = tool(&k, "klaviyo_send_campaign")
            .call(json!({"campaign_id": "C1"}), &ctx)
            .await
            .unwrap();
        assert_eq!(sent, r#"{"campaign_id":"C1","status":"queued"}"#);
        assert_eq!(
            body(&t, 2),
            json!({"data": {"type": "campaign-send-job", "id": "C1"}})
        );
    }
}
