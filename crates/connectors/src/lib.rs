//! Platform-agnostic connector trait, credential schemas and shared domain types.
//! One implementation per e-commerce platform lives in a submodule.

pub mod domain;
pub mod schema;

use schema::CredentialSchema;

/// Supported e-commerce platforms. Adding a platform means adding a variant
/// here and a connector implementation; the UI derives everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    WooCommerce,
    Magento,
    PrestaShop,
    Shopify,
}

impl Platform {
    pub const ALL: [Platform; 4] = [
        Platform::WooCommerce,
        Platform::Magento,
        Platform::PrestaShop,
        Platform::Shopify,
    ];

    pub fn display_name(self) -> &'static str {
        match self {
            Platform::WooCommerce => "WooCommerce",
            Platform::Magento => "Magento",
            Platform::PrestaShop => "PrestaShop",
            Platform::Shopify => "Shopify",
        }
    }

    /// The credentials this platform's API needs. The UI renders the
    /// credential form from this, so connectors never touch UI code.
    pub fn credential_schema(self) -> CredentialSchema {
        use schema::CredentialField as F;
        match self {
            Platform::WooCommerce => CredentialSchema::new(vec![
                F::text("consumer_key", "Consumer key"),
                F::secret("consumer_secret", "Consumer secret"),
            ]),
            Platform::Magento => {
                CredentialSchema::new(vec![F::secret("access_token", "Integration access token")])
            }
            Platform::PrestaShop => {
                CredentialSchema::new(vec![F::secret("webservice_key", "Webservice key")])
            }
            Platform::Shopify => {
                CredentialSchema::new(vec![F::secret("access_token", "Admin API access token")])
            }
        }
    }
}

/// Operations every shop connector must support. Each method maps to the
/// platform's own API and returns normalised domain types.
pub trait ShopConnector {
    fn platform(&self) -> Platform;
    fn list_products(&self) -> Result<Vec<domain::Product>, ConnectorError>;
    fn list_orders(&self) -> Result<Vec<domain::Order>, ConnectorError>;
    fn list_customers(&self) -> Result<Vec<domain::Customer>, ConnectorError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectorError {
    NotImplemented(Platform),
    Transport(String),
    Auth(String),
}

impl std::fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectorError::NotImplemented(p) => {
                write!(f, "{} connector is not implemented yet", p.display_name())
            }
            ConnectorError::Transport(m) => write!(f, "transport error: {m}"),
            ConnectorError::Auth(m) => write!(f, "authentication error: {m}"),
        }
    }
}

impl std::error::Error for ConnectorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_platform_has_a_non_empty_credential_schema() {
        for p in Platform::ALL {
            assert!(!p.credential_schema().fields().is_empty(), "{p:?}");
        }
    }

    #[test]
    fn secrets_are_marked_as_such() {
        let schema = Platform::WooCommerce.credential_schema();
        let secret = schema
            .fields()
            .iter()
            .find(|f| f.key == "consumer_secret")
            .unwrap();
        assert!(secret.secret);
    }
}
