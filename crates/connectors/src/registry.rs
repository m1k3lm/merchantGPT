use std::sync::Arc;

use crate::magento::Magento;
use crate::prestashop::PrestaShop;
use crate::schema::CredentialField as F;
use crate::shop::ShopConnectorAdapter;
use crate::shopify::Shopify;
use crate::woocommerce::WooCommerce;
use crate::{
    Connector, ConnectorError, ConnectorKind, CredentialSchema, Credentials, HttpTransport,
    Platform,
};

/// The single place that knows every connector kind: what credentials it needs
/// and how to build it. The UI and core only go through this.
pub struct ConnectorRegistry;

const MAGENTO_HELP: &str = "In the Magento admin: System > Integrations > Add New Integration, give it API access to Catalog, Sales and Customers, save, then Activate and copy the Access Token. On Magento 2.4.4 or later also set Stores > Configuration > Services > OAuth > Consumer Settings > \"Allow OAuth Access Tokens to be used as standalone Bearer tokens\" to Yes.";

const SHOPIFY_HELP: &str = "Use one of two kinds of app. An existing custom app made in the Shopify admin (Settings > Apps and sales channels > Develop apps): copy its Admin API access token; Shopify stopped allowing new ones there in January 2026. Or an app created in the Shopify Dev Dashboard by the organization that owns the store: give it the scopes read_products, write_products, read_inventory, write_inventory, read_locations, read_orders, write_orders, read_customers, write_customers, read_discounts and write_discounts (plus read_all_orders for orders older than 60 days; abandoned checkouts also need the staff permission to manage them), release a version, install it on the store, and copy its client ID and secret. If the shop URL is a custom domain, also enter the store's myshopify.com domain.";

const PRESTASHOP_HELP: &str = "In the PrestaShop back office: Advanced Parameters > Webservice, set \"Enable PrestaShop's webservice\" to Yes, then Add new webservice key. Generate a key and give it these resources: products, stock_availables and specific_prices (all methods), orders, order_states, customers and currencies (GET), and order_histories (GET and POST). Updates need PrestaShop 8 or later. Prices are the shop's prices excluding tax.";

const SMTP_HELP: &str = "The mailbox the assistant sends from, for reminders and one-to-one follow-ups. With Gmail or Microsoft 365 use an app password. Send marketing only to customers who opted in.";
const BREVO_HELP: &str = "In Brevo: your name > SMTP & API > API keys > Generate a new API key. The sender email must be a verified sender in Brevo.";
const MAILCHIMP_HELP: &str = "In Mailchimp: Profile > Extras > API keys > Create A Key. The part after the dash (e.g. us21) is your data center; the key includes it.";
const KLAVIYO_HELP: &str = "In Klaviyo: Settings > Account > API keys > Create Private API Key with full access to Lists, Profiles, Campaigns, Templates and Subscriptions.";

impl ConnectorRegistry {
    pub fn kinds() -> Vec<ConnectorKind> {
        Platform::ALL
            .into_iter()
            .map(ConnectorKind::Shop)
            .chain([ConnectorKind::GoogleAds])
            .chain(ConnectorKind::MAIL)
            .collect()
    }

    pub fn credential_schema(kind: ConnectorKind) -> CredentialSchema {
        match kind {
            ConnectorKind::Shop(Platform::WooCommerce) => CredentialSchema::new(vec![
                F::text("consumer_key", "Consumer key"),
                F::secret("consumer_secret", "Consumer secret"),
            ]),
            ConnectorKind::Shop(Platform::Magento) => {
                CredentialSchema::new(vec![F::secret("access_token", "Integration access token")])
                    .with_help(MAGENTO_HELP)
            }
            ConnectorKind::Shop(Platform::PrestaShop) => CredentialSchema::new(vec![F::secret(
                crate::prestashop::WEBSERVICE_KEY,
                "Webservice key",
            )])
            .with_help(PRESTASHOP_HELP),
            ConnectorKind::GoogleAds => {
                use crate::google_ads as g;
                CredentialSchema::new(vec![
                    F::text(g::CLIENT_ID, "OAuth client ID"),
                    F::secret(g::CLIENT_SECRET, "OAuth client secret"),
                    F::secret(g::DEVELOPER_TOKEN, "Developer token"),
                    F::secret(g::REFRESH_TOKEN, "Refresh token"),
                    F::text(g::CUSTOMER_ID, "Customer ID"),
                    F::text(g::LOGIN_CUSTOMER_ID, "Manager customer ID").optional(),
                ])
            }
            ConnectorKind::Smtp => CredentialSchema::new(vec![
                F::text("host", "SMTP server, e.g. smtp.gmail.com"),
                F::text("port", "Port (587 STARTTLS, 465 TLS)").optional(),
                F::text("security", "Security: starttls (default), tls or none").optional(),
                F::text("username", "User name"),
                F::secret("password", "Password or app password"),
                F::text("from_email", "From address"),
                F::text("from_name", "From name").optional(),
            ])
            .with_help(SMTP_HELP),
            ConnectorKind::Brevo => CredentialSchema::new(vec![
                F::secret("api_key", "API key"),
                F::text("sender_email", "Sender email (verified in Brevo)"),
                F::text("sender_name", "Sender name").optional(),
            ])
            .with_help(BREVO_HELP),
            ConnectorKind::Mailchimp => CredentialSchema::new(vec![
                F::secret("api_key", "API key (ends in -usXX)"),
                F::text("from_name", "From name for campaigns"),
                F::text("reply_to", "Reply-to email for campaigns"),
            ])
            .with_help(MAILCHIMP_HELP),
            ConnectorKind::Klaviyo => CredentialSchema::new(vec![
                F::secret("api_key", "Private API key (pk_...)"),
                F::text("from_email", "From email for campaigns").optional(),
                F::text("from_name", "From name for campaigns").optional(),
            ])
            .with_help(KLAVIYO_HELP),
            ConnectorKind::Shop(Platform::Shopify) => {
                use crate::shopify as s;
                CredentialSchema::new(vec![
                    F::secret(s::ACCESS_TOKEN, "Admin API access token (custom app)").optional(),
                    F::text(s::CLIENT_ID, "Client ID (Dev Dashboard app)").optional(),
                    F::secret(s::CLIENT_SECRET, "Client secret (Dev Dashboard app)").optional(),
                    F::text(
                        s::SHOP_DOMAIN,
                        "myshopify.com domain (only for a custom shop URL)",
                    )
                    .optional(),
                ])
                .with_help(SHOPIFY_HELP)
            }
        }
    }

    pub fn build(
        kind: ConnectorKind,
        base_url: &str,
        credentials: &Credentials,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Arc<dyn Connector>, ConnectorError> {
        let missing = Self::credential_schema(kind).missing(credentials);
        if !missing.is_empty() {
            return Err(ConnectorError::InvalidCredentials(format!(
                "missing {}",
                missing.join(", ")
            )));
        }
        if ConnectorKind::MAIL.contains(&kind) {
            return crate::mail::build(kind, credentials, transport);
        }
        if kind == ConnectorKind::GoogleAds {
            return Ok(Arc::new(crate::google_ads::GoogleAds::from_credentials(
                credentials,
                transport,
            )?));
        }
        let shop: Arc<dyn crate::ShopConnector> = match kind {
            ConnectorKind::GoogleAds
            | ConnectorKind::Smtp
            | ConnectorKind::Brevo
            | ConnectorKind::Mailchimp
            | ConnectorKind::Klaviyo => unreachable!("handled above"),
            ConnectorKind::Shop(Platform::WooCommerce) => {
                Arc::new(WooCommerce::new(base_url, credentials, transport))
            }
            ConnectorKind::Shop(Platform::Magento) => {
                Arc::new(Magento::new(base_url, credentials, transport))
            }
            ConnectorKind::Shop(Platform::Shopify) => {
                Arc::new(Shopify::new(base_url, credentials, transport)?)
            }
            ConnectorKind::Shop(Platform::PrestaShop) => {
                Arc::new(PrestaShop::new(base_url, credentials, transport))
            }
        };
        Ok(Arc::new(ShopConnectorAdapter(shop)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FakeTransport;

    #[test]
    fn registry_has_schema_for_every_kind() {
        for kind in ConnectorRegistry::kinds() {
            let schema = ConnectorRegistry::credential_schema(kind);
            assert!(!schema.fields().is_empty(), "{kind:?}");
            assert!(schema.fields().iter().any(|f| f.secret), "{kind:?}");
        }
    }

    #[test]
    fn build_rejects_missing_credentials() {
        let kind = ConnectorKind::Shop(Platform::WooCommerce);
        let creds = Credentials::from([("consumer_key".into(), "ck".into())]);
        let err =
            ConnectorRegistry::build(kind, "https://s", &creds, Arc::new(FakeTransport::new()))
                .err()
                .unwrap();
        assert_eq!(
            err,
            ConnectorError::InvalidCredentials("missing Consumer secret".into())
        );
    }
}
