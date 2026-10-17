//! Google Ads REST API client: caches the OAuth access token, refreshes it
//! from the stored refresh token, and turns Google's error envelopes into
//! readable messages.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::oauth::{self, Endpoints, OAuthClient};
use crate::transport::{HttpRequest, HttpTransport, Method};
use crate::ConnectorError;

pub const API_BASE: &str = "https://googleads.googleapis.com/v26";
const MAX_SEARCH_PAGES: usize = 10;

#[derive(Clone, PartialEq, Eq)]
pub struct AdsAuth {
    pub client: OAuthClient,
    pub developer_token: String,
    pub refresh_token: String,
}

impl std::fmt::Debug for AdsAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdsAuth")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

pub struct AdsClient {
    auth: AdsAuth,
    endpoints: Endpoints,
    api_base: String,
    transport: Arc<dyn HttpTransport>,
    token: tokio::sync::Mutex<Option<(String, Instant)>>,
}

/// Customer ids are shown as 123-456-7890 but the API wants digits only.
pub fn normalize_customer_id(id: &str) -> Result<String, ConnectorError> {
    let digits: String = id.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 10
        && id
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == ' ')
    {
        Ok(digits)
    } else {
        Err(ConnectorError::InvalidCredentials(format!(
            "{id:?} is not a Google Ads customer id (10 digits, e.g. 123-456-7890)"
        )))
    }
}

pub fn format_customer_id(id: &str) -> String {
    if id.len() == 10 {
        format!("{}-{}-{}", &id[..3], &id[3..6], &id[6..])
    } else {
        id.to_string()
    }
}

/// Google Ads errors carry the useful text in `details[].errors[].message`.
fn error_message(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.chars().take(300).collect();
    };
    let detailed: Vec<String> = v["error"]["details"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|d| d["errors"].as_array().cloned().unwrap_or_default())
        .filter_map(|e| e["message"].as_str().map(str::to_string))
        .collect();
    if detailed.is_empty() {
        v["error"]["message"]
            .as_str()
            .unwrap_or("unknown Google Ads error")
            .to_string()
    } else {
        detailed.join("; ")
    }
}

impl AdsClient {
    pub fn new(auth: AdsAuth, transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            auth,
            endpoints: Endpoints::default(),
            api_base: API_BASE.into(),
            transport,
            token: tokio::sync::Mutex::new(None),
        }
    }

    pub fn auth(&self) -> &AdsAuth {
        &self.auth
    }

    async fn access_token(&self, force: bool) -> Result<String, ConnectorError> {
        let mut cached = self.token.lock().await;
        if let Some((token, expires)) = cached.as_ref() {
            if !force && Instant::now() + Duration::from_secs(60) < *expires {
                return Ok(token.clone());
            }
        }
        let t = oauth::refresh(
            self.transport.as_ref(),
            &self.endpoints,
            &self.auth.client,
            &self.auth.refresh_token,
        )
        .await?;
        *cached = Some((
            t.access_token.clone(),
            Instant::now() + Duration::from_secs(t.expires_in),
        ));
        Ok(t.access_token)
    }

    async fn call(
        &self,
        login_customer_id: Option<&str>,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ConnectorError> {
        for attempt in 0..2 {
            let token = self.access_token(attempt > 0).await?;
            let mut req = HttpRequest::new(method, format!("{}{path}", self.api_base))
                .header("Authorization", format!("Bearer {token}"))
                .header("developer-token", &self.auth.developer_token);
            if let Some(login) = login_customer_id {
                req = req.header("login-customer-id", login);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = self.transport.send(req).await?;
            // A token revoked or expired early: refresh once and retry.
            if resp.status == 401 && attempt == 0 {
                continue;
            }
            if !resp.is_success() {
                let message = error_message(&resp.body);
                return Err(match resp.status {
                    401 | 403 => ConnectorError::Auth(message),
                    404 => ConnectorError::NotFound(message),
                    status => ConnectorError::Api { status, message },
                });
            }
            return serde_json::from_str(&resp.body)
                .map_err(|e| ConnectorError::InvalidResponse(format!("Google Ads: {e}")));
        }
        unreachable!("the second attempt always returns")
    }

    pub async fn accessible_customers(&self) -> Result<Vec<String>, ConnectorError> {
        let v = self
            .call(
                None,
                Method::Get,
                "/customers:listAccessibleCustomers",
                None,
            )
            .await?;
        Ok(v["resourceNames"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str()?.strip_prefix("customers/").map(str::to_string))
            .collect())
    }

    /// Runs a GAQL query, following pages up to a limit.
    pub async fn search(
        &self,
        login_customer_id: Option<&str>,
        customer_id: &str,
        query: &str,
    ) -> Result<Vec<Value>, ConnectorError> {
        let path = format!("/customers/{customer_id}/googleAds:search");
        let mut rows = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_SEARCH_PAGES {
            let mut body = json!({"query": query});
            if let Some(t) = &page_token {
                body["pageToken"] = json!(t);
            }
            let v = self
                .call(login_customer_id, Method::Post, &path, Some(&body))
                .await?;
            rows.extend(v["results"].as_array().cloned().unwrap_or_default());
            page_token = v["nextPageToken"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if page_token.is_none() {
                break;
            }
        }
        Ok(rows)
    }

    /// Applies all operations atomically: either everything is created or nothing.
    pub async fn mutate(
        &self,
        login_customer_id: Option<&str>,
        customer_id: &str,
        operations: Vec<Value>,
        validate_only: bool,
    ) -> Result<Value, ConnectorError> {
        let body = json!({"mutateOperations": operations, "validateOnly": validate_only});
        self.call(
            login_customer_id,
            Method::Post,
            &format!("/customers/{customer_id}/googleAds:mutate"),
            Some(&body),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) fn with_api_base(mut self, base: &str) -> Self {
        self.api_base = base.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{FakeTransport, HttpResponse};

    pub(crate) fn auth() -> AdsAuth {
        AdsAuth {
            client: OAuthClient {
                client_id: "cid".into(),
                client_secret: "csecret".into(),
            },
            developer_token: "devtok".into(),
            refresh_token: "1//refresh".into(),
        }
    }

    #[test]
    fn customer_ids_accept_dashes_and_reject_garbage() {
        assert_eq!(normalize_customer_id("123-456-7890").unwrap(), "1234567890");
        assert!(normalize_customer_id("123").is_err());
        assert!(normalize_customer_id("12a4567890").is_err());
        assert_eq!(format_customer_id("1234567890"), "123-456-7890");
    }

    #[tokio::test]
    async fn access_token_is_cached_and_refreshed_when_expired_or_rejected() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/token",
            HttpResponse::new(200, r#"{"access_token":"ya29.one","expires_in":3600}"#),
        );
        t.on(
            Method::Get,
            "/v26/customers:listAccessibleCustomers",
            HttpResponse::new(200, r#"{"resourceNames":["customers/1234567890"]}"#),
        );
        let c = AdsClient::new(auth(), t.clone());
        assert_eq!(c.accessible_customers().await.unwrap(), ["1234567890"]);
        c.accessible_customers().await.unwrap();
        let tokens =
            |t: &FakeTransport| t.requests().iter().filter(|r| r.path() == "/token").count();
        assert_eq!(tokens(&t), 1, "cached between calls");
        let api = t
            .requests()
            .into_iter()
            .find(|r| r.path() != "/token")
            .unwrap();
        assert_eq!(api.header_value("authorization"), Some("Bearer ya29.one"));
        assert_eq!(api.header_value("developer-token"), Some("devtok"));

        let t2 = Arc::new(FakeTransport::new());
        t2.on(
            Method::Post,
            "/token",
            HttpResponse::new(200, r#"{"access_token":"ya29.short","expires_in":0}"#),
        );
        t2.on(
            Method::Get,
            "/v26/customers:listAccessibleCustomers",
            HttpResponse::new(401, r#"{"error":{"code":401,"message":"Request had invalid authentication credentials."}}"#),
        )
        .on(
            Method::Get,
            "/v26/customers:listAccessibleCustomers",
            HttpResponse::new(200, r#"{"resourceNames":[]}"#),
        );
        let c2 = AdsClient::new(auth(), t2.clone());
        c2.accessible_customers().await.unwrap();
        assert_eq!(tokens(&t2), 2, "expired token refreshed after the 401");
    }

    #[tokio::test]
    async fn google_ads_failures_are_readable() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/token",
            HttpResponse::new(200, r#"{"access_token":"a","expires_in":3600}"#),
        );
        t.on(
            Method::Post,
            "/v26/customers/1234567890/googleAds:search",
            HttpResponse::new(
                403,
                r#"{"error":{"code":403,"message":"The caller does not have permission","status":"PERMISSION_DENIED","details":[{"@type":"type.googleapis.com/google.ads.googleads.v26.errors.GoogleAdsFailure","errors":[{"errorCode":{"authorizationError":"DEVELOPER_TOKEN_NOT_APPROVED"},"message":"The developer token is only approved for use with test accounts."}]}]}}"#,
            ),
        );
        let err = AdsClient::new(auth(), t)
            .search(None, "1234567890", "SELECT customer.id FROM customer")
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ConnectorError::Auth(
                "The developer token is only approved for use with test accounts.".into()
            )
        );
    }

    #[tokio::test]
    async fn search_follows_pages_and_sends_login_customer() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/token",
            HttpResponse::new(200, r#"{"access_token":"a","expires_in":3600}"#),
        );
        t.on(
            Method::Post,
            "/v26/customers/1111111111/googleAds:search",
            HttpResponse::new(200, r#"{"results":[{"n":1}],"nextPageToken":"p2"}"#),
        )
        .on(
            Method::Post,
            "/v26/customers/1111111111/googleAds:search",
            HttpResponse::new(200, r#"{"results":[{"n":2}]}"#),
        );
        let rows = AdsClient::new(auth(), t.clone())
            .with_api_base(API_BASE)
            .search(Some("9999999999"), "1111111111", "SELECT x FROM y")
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        let calls: Vec<_> = t
            .requests()
            .into_iter()
            .filter(|r| r.path() != "/token")
            .collect();
        assert_eq!(
            calls[0].header_value("login-customer-id"),
            Some("9999999999")
        );
        assert!(calls[1]
            .body
            .as_deref()
            .unwrap()
            .contains("\"pageToken\":\"p2\""));
    }
}
