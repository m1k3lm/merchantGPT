//! Connectors to external services. A connector contributes tools to the agent;
//! shop platforms are one kind, ad platforms and carriers are future kinds.

pub mod domain;
pub mod platform;
pub mod registry;
pub mod schema;
pub mod shop;
pub mod shop_tools;
pub mod stub;
pub mod transport;

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
}

impl ConnectorKind {
    pub fn id(self) -> String {
        match self {
            ConnectorKind::Shop(p) => format!("shop:{}", p.id()),
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        let (family, rest) = id.split_once(':')?;
        match family {
            "shop" => Platform::parse(rest).map(ConnectorKind::Shop),
            _ => None,
        }
    }

    pub fn display_name(self) -> String {
        match self {
            ConnectorKind::Shop(p) => format!("{} shop", p.display_name()),
        }
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
    #[error("transport error: {0}")]
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
        assert_eq!(ConnectorKind::parse("ads:google"), None);
        assert_eq!(ConnectorKind::parse("nonsense"), None);
    }
}
