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

    /// Tools (`shop_list_abandoned_carts`) or tool actions (`shop_manage_order:delete`)
    /// this platform cannot do; they are left out of what the model sees.
    fn hidden(&self) -> &'static [&'static str] {
        &[]
    }

    /// One line describing the raw API for `shop_api_get` / `shop_api_write`.
    fn api_hint(&self) -> &'static str {
        ""
    }

    async fn create_product(&self, _product: &ProductPatch) -> Result<Product, ConnectorError> {
        Err(unsupported(self.platform(), "creating products"))
    }
    async fn delete_product(&self, _id: &str) -> Result<(), ConnectorError> {
        Err(unsupported(self.platform(), "deleting products"))
    }

    async fn list_categories(&self, _q: &CategoryQuery) -> Result<Page<Category>, ConnectorError> {
        Err(unsupported(self.platform(), "categories"))
    }
    async fn create_category(&self, _c: &CategoryPatch) -> Result<Category, ConnectorError> {
        Err(unsupported(self.platform(), "creating categories"))
    }
    async fn update_category(
        &self,
        _id: &str,
        _c: &CategoryPatch,
    ) -> Result<Category, ConnectorError> {
        Err(unsupported(self.platform(), "updating categories"))
    }
    async fn delete_category(&self, _id: &str) -> Result<(), ConnectorError> {
        Err(unsupported(self.platform(), "deleting categories"))
    }

    async fn get_customer(&self, _id: &str) -> Result<Customer, ConnectorError> {
        Err(unsupported(self.platform(), "reading one customer"))
    }
    async fn create_customer(&self, _c: &CustomerPatch) -> Result<Customer, ConnectorError> {
        Err(unsupported(self.platform(), "creating customers"))
    }
    async fn update_customer(
        &self,
        _id: &str,
        _c: &CustomerPatch,
    ) -> Result<Customer, ConnectorError> {
        Err(unsupported(self.platform(), "updating customers"))
    }
    async fn delete_customer(&self, _id: &str) -> Result<(), ConnectorError> {
        Err(unsupported(self.platform(), "deleting customers"))
    }

    async fn list_addresses(&self, _customer_id: &str) -> Result<Vec<Address>, ConnectorError> {
        Err(unsupported(self.platform(), "addresses"))
    }
    async fn create_address(
        &self,
        _customer_id: &str,
        _a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        Err(unsupported(self.platform(), "creating addresses"))
    }
    async fn update_address(
        &self,
        _customer_id: &str,
        _id: &str,
        _a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        Err(unsupported(self.platform(), "updating addresses"))
    }
    async fn delete_address(&self, _customer_id: &str, _id: &str) -> Result<(), ConnectorError> {
        Err(unsupported(self.platform(), "deleting addresses"))
    }

    async fn create_order(&self, _o: &NewOrder) -> Result<Order, ConnectorError> {
        Err(unsupported(self.platform(), "creating orders"))
    }
    async fn add_order_note(&self, _id: &str, _note: &OrderNote) -> Result<(), ConnectorError> {
        Err(unsupported(self.platform(), "order notes"))
    }
    async fn delete_order(&self, _id: &str) -> Result<(), ConnectorError> {
        Err(unsupported(self.platform(), "deleting orders"))
    }

    async fn list_coupons(&self, _q: &CouponQuery) -> Result<Page<Coupon>, ConnectorError> {
        Err(unsupported(self.platform(), "coupons"))
    }
    async fn create_coupon(&self, _c: &CouponPatch) -> Result<Coupon, ConnectorError> {
        Err(unsupported(self.platform(), "creating coupons"))
    }
    async fn update_coupon(&self, _id: &str, _c: &CouponPatch) -> Result<Coupon, ConnectorError> {
        Err(unsupported(self.platform(), "updating coupons"))
    }
    async fn delete_coupon(&self, _id: &str) -> Result<(), ConnectorError> {
        Err(unsupported(self.platform(), "deleting coupons"))
    }

    async fn list_abandoned_carts(
        &self,
        _q: &CartQuery,
    ) -> Result<Page<AbandonedCart>, ConnectorError> {
        Err(unsupported(self.platform(), "abandoned carts"))
    }

    async fn raw(&self, _req: &RawRequest) -> Result<RawResponse, ConnectorError> {
        Err(unsupported(self.platform(), "raw API requests"))
    }
}

/// For platforms whose addresses have no field for a tax or national id.
pub fn refuse_id_number(
    platform: Platform,
    a: Option<&AddressPatch>,
) -> Result<(), ConnectorError> {
    if a.is_some_and(|a| a.id_number.is_some()) {
        return Err(unsupported(
            platform,
            "an id number (DNI/NIF) on addresses; put it in the company or address2 field if the merchant wants it on the address",
        ));
    }
    Ok(())
}

pub fn unsupported(platform: Platform, what: &str) -> ConnectorError {
    ConnectorError::Unsupported(format!(
        "{} does not support {what} through its API",
        platform.display_name()
    ))
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
