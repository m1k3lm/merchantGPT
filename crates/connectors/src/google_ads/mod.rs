//! Google Ads connector: drafts Search campaigns (always created PAUSED) and
//! reports on existing ones. Authorization is Google OAuth (see `oauth`); the
//! stored credentials are the app's OAuth client and developer token plus the
//! merchant's refresh token and chosen customer account.

pub mod campaigns;
pub mod client;
pub mod oauth;
mod tools_impl;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tools::Tool;

use crate::{Connector, ConnectorError, ConnectorKind, Credentials, HttpTransport};
pub use client::{format_customer_id, normalize_customer_id, AdsAuth, AdsClient};
pub use oauth::OAuthClient;

pub const CLIENT_ID: &str = "client_id";
pub const CLIENT_SECRET: &str = "client_secret";
pub const DEVELOPER_TOKEN: &str = "developer_token";
pub const REFRESH_TOKEN: &str = "refresh_token";
pub const CUSTOMER_ID: &str = "customer_id";
pub const LOGIN_CUSTOMER_ID: &str = "login_customer_id";

pub(crate) struct AdsContext {
    pub client: AdsClient,
    pub customer_id: String,
    pub login_customer_id: Option<String>,
}

pub struct GoogleAds {
    ctx: Arc<AdsContext>,
}

fn get<'a>(c: &'a Credentials, key: &str) -> Option<&'a str> {
    c.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

impl GoogleAds {
    pub fn from_credentials(
        creds: &Credentials,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, ConnectorError> {
        let need = |k: &str| {
            get(creds, k)
                .map(str::to_string)
                .ok_or_else(|| ConnectorError::InvalidCredentials(format!("missing {k}")))
        };
        let auth = AdsAuth {
            client: OAuthClient {
                client_id: need(CLIENT_ID)?,
                client_secret: need(CLIENT_SECRET)?,
            },
            developer_token: need(DEVELOPER_TOKEN)?,
            refresh_token: need(REFRESH_TOKEN)?,
        };
        let login_customer_id = get(creds, LOGIN_CUSTOMER_ID)
            .map(normalize_customer_id)
            .transpose()?;
        Ok(Self {
            ctx: Arc::new(AdsContext {
                client: AdsClient::new(auth, transport),
                customer_id: normalize_customer_id(&need(CUSTOMER_ID)?)?,
                login_customer_id,
            }),
        })
    }
}

#[async_trait]
impl Connector for GoogleAds {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::GoogleAds
    }

    async fn verify(&self) -> Result<(), ConnectorError> {
        self.ctx
            .client
            .search(
                self.ctx.login_customer_id.as_deref(),
                &self.ctx.customer_id,
                "SELECT customer.id FROM customer LIMIT 1",
            )
            .await
            .map(|_| ())
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        tools_impl::ads_tools(self.ctx.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdsAccount {
    pub customer_id: String,
    pub name: String,
    pub currency: String,
    pub manager: bool,
    pub test_account: bool,
    /// Set for client accounts reached through a manager account.
    pub login_customer_id: Option<String>,
}

fn account(row: &Value, key: &str, login: Option<&str>) -> Option<AdsAccount> {
    let c = &row[key];
    Some(AdsAccount {
        customer_id: c["id"]
            .as_str()
            .map(str::to_string)
            .or_else(|| c["id"].as_u64().map(|n| n.to_string()))?,
        name: c["descriptiveName"]
            .as_str()
            .unwrap_or("(unnamed account)")
            .to_string(),
        currency: c["currencyCode"].as_str().unwrap_or_default().to_string(),
        manager: c["manager"].as_bool().unwrap_or(false),
        test_account: c["testAccount"].as_bool().unwrap_or(false),
        login_customer_id: login.map(str::to_string),
    })
}

/// Every account the signed-in Google user can use, including the client
/// accounts directly under any manager account they can access.
pub async fn list_accounts(client: &AdsClient) -> Result<Vec<AdsAccount>, ConnectorError> {
    let mut out: Vec<AdsAccount> = Vec::new();
    for id in client.accessible_customers().await?.into_iter().take(50) {
        let rows = match client
            .search(
                None,
                &id,
                "SELECT customer.id, customer.descriptive_name, customer.currency_code, customer.manager, customer.test_account FROM customer LIMIT 1",
            )
            .await
        {
            Ok(rows) => rows,
            // Cancelled or suspended accounts still appear in the list; skip them.
            Err(_) => continue,
        };
        let Some(acc) = rows.first().and_then(|r| account(r, "customer", None)) else {
            continue;
        };
        let manager = acc.manager.then(|| acc.customer_id.clone());
        out.push(acc);
        if let Some(manager_id) = manager {
            let clients = client
                .search(
                    Some(&manager_id),
                    &manager_id,
                    "SELECT customer_client.id, customer_client.descriptive_name, customer_client.currency_code, customer_client.manager, customer_client.test_account FROM customer_client WHERE customer_client.level = 1 AND customer_client.status = 'ENABLED'",
                )
                .await
                .unwrap_or_default();
            out.extend(
                clients
                    .iter()
                    .filter_map(|r| account(r, "customerClient", Some(&manager_id))),
            );
        }
    }
    out.sort_by(|a, b| (a.manager, &a.name).cmp(&(b.manager, &b.name)));
    out.dedup_by(|a, b| a.customer_id == b.customer_id);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{FakeTransport, HttpResponse, Method};

    fn token(t: &FakeTransport) {
        t.on(
            Method::Post,
            "/token",
            HttpResponse::new(200, r#"{"access_token":"a","expires_in":3600}"#),
        );
    }

    pub(crate) fn creds() -> Credentials {
        Credentials::from([
            (CLIENT_ID.into(), "cid".into()),
            (CLIENT_SECRET.into(), "csecret".into()),
            (DEVELOPER_TOKEN.into(), "devtok".into()),
            (REFRESH_TOKEN.into(), "1//r".into()),
            (CUSTOMER_ID.into(), "123-456-7890".into()),
        ])
    }

    #[tokio::test]
    async fn accounts_include_clients_of_managers() {
        let t = Arc::new(FakeTransport::new());
        token(&t);
        t.on(
            Method::Get,
            "/v26/customers:listAccessibleCustomers",
            HttpResponse::new(
                200,
                r#"{"resourceNames":["customers/1111111111","customers/9999999999"]}"#,
            ),
        );
        t.on(
            Method::Post,
            "/v26/customers/1111111111/googleAds:search",
            HttpResponse::new(200, r#"{"results":[{"customer":{"id":"1111111111","descriptiveName":"Shop ES","currencyCode":"EUR","manager":false,"testAccount":false}}]}"#),
        );
        t.on(Method::Post, "/v26/customers/9999999999/googleAds:search", HttpResponse::new(
            200,
            r#"{"results":[{"customer":{"id":"9999999999","descriptiveName":"Agency MCC","currencyCode":"EUR","manager":true}}]}"#,
        ))
        .on(Method::Post, "/v26/customers/9999999999/googleAds:search", HttpResponse::new(
            200,
            r#"{"results":[{"customerClient":{"id":"2222222222","descriptiveName":"Shop PT","currencyCode":"EUR","manager":false}}]}"#,
        ));
        let accounts = list_accounts(&AdsClient::new(
            AdsAuth {
                client: OAuthClient {
                    client_id: "c".into(),
                    client_secret: "s".into(),
                },
                developer_token: "d".into(),
                refresh_token: "r".into(),
            },
            t.clone(),
        ))
        .await
        .unwrap();
        let names: Vec<_> = accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Shop ES", "Shop PT", "Agency MCC"]);
        assert_eq!(accounts[1].login_customer_id.as_deref(), Some("9999999999"));
        assert!(accounts[2].manager);
    }

    #[test]
    fn credentials_are_validated() {
        let t: Arc<dyn HttpTransport> = Arc::new(FakeTransport::new());
        assert!(GoogleAds::from_credentials(&creds(), t.clone()).is_ok());
        let mut bad = creds();
        bad.insert(CUSTOMER_ID.into(), "12".into());
        assert!(matches!(
            GoogleAds::from_credentials(&bad, t.clone()),
            Err(ConnectorError::InvalidCredentials(_))
        ));
        let mut missing = creds();
        missing.remove(REFRESH_TOKEN);
        assert!(GoogleAds::from_credentials(&missing, t).is_err());
    }
}
