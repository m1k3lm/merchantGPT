use async_trait::async_trait;

use crate::domain::*;
use crate::{ConnectorError, Platform, ShopConnector};

/// Placeholder for platforms without a real client yet. Every call fails with
/// `NotImplemented`, so the agent can tell the merchant instead of guessing.
pub struct StubShop(pub Platform);

impl StubShop {
    fn err<T>(&self) -> Result<T, ConnectorError> {
        Err(ConnectorError::NotImplemented(format!(
            "the {} connector",
            self.0.display_name()
        )))
    }
}

#[async_trait]
impl ShopConnector for StubShop {
    fn platform(&self) -> Platform {
        self.0
    }
    async fn verify(&self) -> Result<(), ConnectorError> {
        self.err()
    }
    async fn list_products(&self, _: &ProductQuery) -> Result<Page<Product>, ConnectorError> {
        self.err()
    }
    async fn get_product(&self, _: &str) -> Result<Product, ConnectorError> {
        self.err()
    }
    async fn update_product(&self, _: &str, _: &ProductPatch) -> Result<Product, ConnectorError> {
        self.err()
    }
    async fn list_orders(&self, _: &OrderQuery) -> Result<Page<Order>, ConnectorError> {
        self.err()
    }
    async fn get_order(&self, _: &str) -> Result<Order, ConnectorError> {
        self.err()
    }
    async fn update_order_status(&self, _: &str, _: &str) -> Result<Order, ConnectorError> {
        self.err()
    }
    async fn list_customers(&self, _: &CustomerQuery) -> Result<Page<Customer>, ConnectorError> {
        self.err()
    }
    async fn sales_report(&self, _: &DateRange) -> Result<SalesReport, ConnectorError> {
        self.err()
    }
}
