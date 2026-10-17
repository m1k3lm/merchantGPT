use std::sync::Arc;

use async_trait::async_trait;
use tools::Tool;

use crate::domain::*;
use crate::shop_tools::shop_tools;
use crate::{Connector, ConnectorError, ConnectorKind, Platform};

/// Shop operations every platform implements. Returned values are normalised
/// domain types; the `ShopTools` adapter exposes them to the agent.
#[async_trait]
pub trait ShopConnector: Send + Sync {
    fn platform(&self) -> Platform;
    async fn verify(&self) -> Result<(), ConnectorError>;
    async fn list_products(&self, q: &ProductQuery) -> Result<Page<Product>, ConnectorError>;
    async fn get_product(&self, id: &str) -> Result<Product, ConnectorError>;
    async fn update_product(
        &self,
        id: &str,
        patch: &ProductPatch,
    ) -> Result<Product, ConnectorError>;
    async fn list_orders(&self, q: &OrderQuery) -> Result<Page<Order>, ConnectorError>;
    async fn get_order(&self, id: &str) -> Result<Order, ConnectorError>;
    async fn update_order_status(&self, id: &str, status: &str) -> Result<Order, ConnectorError>;
    async fn list_customers(&self, q: &CustomerQuery) -> Result<Page<Customer>, ConnectorError>;
    async fn sales_report(&self, range: &DateRange) -> Result<SalesReport, ConnectorError>;
}

/// Lifts any `ShopConnector` into a generic `Connector`.
pub struct ShopConnectorAdapter(pub Arc<dyn ShopConnector>);

#[async_trait]
impl Connector for ShopConnectorAdapter {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Shop(self.0.platform())
    }

    async fn verify(&self) -> Result<(), ConnectorError> {
        self.0.verify().await
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        shop_tools(self.0.clone())
    }
}
