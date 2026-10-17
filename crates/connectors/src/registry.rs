use std::sync::Arc;

use crate::schema::CredentialField as F;
use crate::shop::ShopConnectorAdapter;
use crate::stub::StubShop;
use crate::woocommerce::WooCommerce;
use crate::{
    Connector, ConnectorError, ConnectorKind, CredentialSchema, Credentials, HttpTransport,
    Platform,
};

/// The single place that knows every connector kind: what credentials it needs
/// and how to build it. The UI and core only go through this.
pub struct ConnectorRegistry;

impl ConnectorRegistry {
    pub fn kinds() -> Vec<ConnectorKind> {
        Platform::ALL.into_iter().map(ConnectorKind::Shop).collect()
    }

    pub fn credential_schema(kind: ConnectorKind) -> CredentialSchema {
        match kind {
            ConnectorKind::Shop(Platform::WooCommerce) => CredentialSchema::new(vec![
                F::text("consumer_key", "Consumer key"),
                F::secret("consumer_secret", "Consumer secret"),
            ]),
            ConnectorKind::Shop(Platform::Magento) => {
                CredentialSchema::new(vec![F::secret("access_token", "Integration access token")])
            }
            ConnectorKind::Shop(Platform::PrestaShop) => {
                CredentialSchema::new(vec![F::secret("webservice_key", "Webservice key")])
            }
            ConnectorKind::Shop(Platform::Shopify) => {
                CredentialSchema::new(vec![F::secret("access_token", "Admin API access token")])
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
        let shop: Arc<dyn crate::ShopConnector> = match kind {
            ConnectorKind::Shop(Platform::WooCommerce) => {
                Arc::new(WooCommerce::new(base_url, credentials, transport))
            }
            ConnectorKind::Shop(p) => Arc::new(StubShop(p)),
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
