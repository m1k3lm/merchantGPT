//! Connectors to external services. A connector contributes tools to the agent;
//! shop platforms are one kind, Google Ads another; carriers are future kinds.

pub mod domain;
pub mod google_ads;
pub mod magento;
pub mod mail;
pub mod platform;
pub mod prestashop;
pub mod registry;
pub mod schema;
pub mod shop;
pub mod shop_tools;
pub mod shopify;
pub mod stub;
pub mod transport;
pub mod woocommerce;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tools::Tool;

pub use platform::Platform;
pub use registry::ConnectorRegistry;
pub use schema::{CredentialField, CredentialSchema, Credentials};
pub use shop::ShopConnector;
pub use transport::{HttpTransport, ReqwestTransport};

/// What a connector connects to. Stored as its `id()` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConnectorKind {
    Shop(Platform),
    GoogleAds,
    Smtp,
    Brevo,
    Mailchimp,
    Klaviyo,
}

impl ConnectorKind {
    pub fn id(self) -> String {
        match self {
            ConnectorKind::Shop(p) => format!("shop:{}", p.id()),
            ConnectorKind::GoogleAds => "ads:google".into(),
            ConnectorKind::Smtp => "mail:smtp".into(),
            ConnectorKind::Brevo => "mail:brevo".into(),
            ConnectorKind::Mailchimp => "mail:mailchimp".into(),
            ConnectorKind::Klaviyo => "mail:klaviyo".into(),
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        let (family, rest) = id.split_once(':')?;
        match family {
            "shop" => Platform::parse(rest).map(ConnectorKind::Shop),
            "ads" if rest == "google" => Some(ConnectorKind::GoogleAds),
            "mail" => Self::MAIL.into_iter().find(|k| k.id() == id),
            _ => None,
        }
    }

    pub fn display_name(self) -> String {
        match self {
            ConnectorKind::Shop(p) => format!("{} shop", p.display_name()),
            ConnectorKind::GoogleAds => "Google Ads".into(),
            ConnectorKind::Smtp => "Email (SMTP)".into(),
            ConnectorKind::Brevo => "Brevo".into(),
            ConnectorKind::Mailchimp => "Mailchimp".into(),
            ConnectorKind::Klaviyo => "Klaviyo".into(),
        }
    }

    /// A few letters for the shop list badge.
    pub fn badge(self) -> &'static str {
        match self {
            ConnectorKind::Shop(_) => "",
            ConnectorKind::GoogleAds => "Ads",
            ConnectorKind::Smtp => "Mail",
            ConnectorKind::Brevo => "Brevo",
            ConnectorKind::Mailchimp => "Mailchimp",
            ConnectorKind::Klaviyo => "Klaviyo",
        }
    }

    /// Email and marketing services, linked to a shop with a plain credential form.
    pub const MAIL: [ConnectorKind; 4] = [
        ConnectorKind::Smtp,
        ConnectorKind::Brevo,
        ConnectorKind::Mailchimp,
        ConnectorKind::Klaviyo,
    ];

    pub fn is_shop(self) -> bool {
        matches!(self, ConnectorKind::Shop(_))
    }
}

#[async_trait]
pub trait Connector: Send + Sync {
    fn kind(&self) -> ConnectorKind;
    /// Cheap authenticated call proving the credentials work.
    async fn verify(&self) -> Result<(), ConnectorError>;
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorError {
    #[error("{0} is not implemented yet")]
    NotImplemented(String),
    /// The platform's API cannot do this; the message says what to do instead.
    #[error("{0}")]
    Unsupported(String),
    #[error("invalid credentials: {0}")]
    InvalidCredentials(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("unexpected response: {0}")]
    InvalidResponse(String),
    #[error("could not reach the shop: {0}")]
    Transport(String),
}

impl From<ConnectorError> for tools::ToolError {
    fn from(e: ConnectorError) -> Self {
        tools::ToolError::Failed(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_kind_id_roundtrips() {
        for p in Platform::ALL {
            let k = ConnectorKind::Shop(p);
            assert_eq!(ConnectorKind::parse(&k.id()), Some(k));
        }
        assert_eq!(
            ConnectorKind::parse(&ConnectorKind::GoogleAds.id()),
            Some(ConnectorKind::GoogleAds)
        );
        for k in ConnectorKind::MAIL {
            assert_eq!(ConnectorKind::parse(&k.id()), Some(k));
        }
        assert_eq!(ConnectorKind::parse("ads:bing"), None);
        assert_eq!(ConnectorKind::parse("mail:pigeon"), None);
        assert_eq!(ConnectorKind::parse("nonsense"), None);
    }
}
