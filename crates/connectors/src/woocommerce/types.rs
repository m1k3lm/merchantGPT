//! WooCommerce REST v3 payloads and their mapping into domain types. Money
//! arrives as strings ("12.50", or "" when unset).

use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::domain::*;
use crate::ConnectorError;

fn id<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s,
        _ => String::new(),
    })
}

fn opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

pub(crate) fn money(s: Option<&str>) -> Result<Option<Decimal>, ConnectorError> {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => Decimal::from_str(v)
            .map(Some)
            .map_err(|e| ConnectorError::InvalidResponse(format!("bad amount {v:?}: {e}"))),
    }
}

fn money_or_zero(s: &str) -> Result<Decimal, ConnectorError> {
    Ok(money(Some(s))?.unwrap_or_default())
}

#[derive(Deserialize)]
pub(crate) struct WooProduct {
    #[serde(deserialize_with = "id")]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "opt_string")]
    sku: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    status: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    price: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    regular_price: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    sale_price: Option<String>,
    #[serde(default)]
    stock_quantity: Option<i64>,
    #[serde(default, deserialize_with = "opt_string")]
    stock_status: Option<String>,
    #[serde(default)]
    categories: Vec<WooRef>,
}

#[derive(Deserialize)]
struct WooRef {
    #[serde(deserialize_with = "id")]
    id: String,
}

impl TryFrom<WooProduct> for Product {
    type Error = ConnectorError;
    fn try_from(p: WooProduct) -> Result<Self, ConnectorError> {
        Ok(Product {
            category_ids: p.categories.into_iter().map(|c| c.id).collect(),
            price: money(p.price.as_deref())?,
            regular_price: money(p.regular_price.as_deref())?,
            sale_price: money(p.sale_price.as_deref())?,
            id: p.id,
            name: p.name,
            sku: p.sku,
            status: p.status,
            stock_quantity: p.stock_quantity,
            stock_status: p.stock_status,
        })
    }
}

#[derive(Deserialize, Default)]
struct WooBilling {
    #[serde(default)]
    first_name: String,
    #[serde(default)]
    last_name: String,
    #[serde(default, deserialize_with = "opt_string")]
    email: Option<String>,
}

#[derive(Deserialize)]
struct WooLineItem {
    #[serde(default, deserialize_with = "opt_string")]
    product_id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    quantity: i64,
    #[serde(default)]
    total: String,
}

#[derive(Deserialize)]
pub(crate) struct WooOrder {
    #[serde(deserialize_with = "id")]
    id: String,
    #[serde(default, deserialize_with = "opt_string")]
    number: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    total: String,
    #[serde(default, deserialize_with = "opt_string")]
    date_created: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    customer_id: Option<String>,
    #[serde(default)]
    billing: WooBilling,
    #[serde(default)]
    line_items: Vec<WooLineItem>,
}

impl TryFrom<WooOrder> for Order {
    type Error = ConnectorError;
    fn try_from(o: WooOrder) -> Result<Self, ConnectorError> {
        let name = format!("{} {}", o.billing.first_name, o.billing.last_name)
            .trim()
            .to_string();
        let line_items = o
            .line_items
            .into_iter()
            .map(|l| {
                Ok(LineItem {
                    product_id: l.product_id.filter(|p| p != "0"),
                    name: l.name,
                    quantity: l.quantity,
                    total: money_or_zero(&l.total)?,
                })
            })
            .collect::<Result<_, ConnectorError>>()?;
        Ok(Order {
            number: o.number.unwrap_or_else(|| o.id.clone()),
            total: money_or_zero(&o.total)?,
            id: o.id,
            status: o.status,
            currency: o.currency,
            created_at: o.date_created,
            // 0 means a guest checkout.
            customer_id: o.customer_id.filter(|c| c != "0"),
            customer_name: Some(name).filter(|n| !n.is_empty()),
            customer_email: o.billing.email,
            line_items,
        })
    }
}

/// A customer's `billing` or `shipping` block. Every field is a string and
/// an unset one is "".
#[derive(Deserialize, Default)]
pub(crate) struct WooAddress {
    #[serde(default)]
    first_name: String,
    #[serde(default)]
    last_name: String,
    #[serde(default)]
    company: String,
    #[serde(default)]
    address_1: String,
    #[serde(default)]
    address_2: String,
    #[serde(default)]
    city: String,
    #[serde(default)]
    postcode: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    country: String,
    #[serde(default)]
    phone: String,
}

fn non_empty(s: String) -> Option<String> {
    Some(s).filter(|s| !s.trim().is_empty())
}

impl WooAddress {
    /// The non-empty fields, to copy a saved address into an order.
    pub(crate) fn as_patch(&self) -> AddressPatch {
        let f = |s: &String| non_empty(s.clone());
        AddressPatch {
            kind: None,
            first_name: f(&self.first_name),
            last_name: f(&self.last_name),
            company: f(&self.company),
            address1: f(&self.address_1),
            address2: f(&self.address_2),
            city: f(&self.city),
            postcode: f(&self.postcode),
            state: f(&self.state),
            country: f(&self.country),
            phone: f(&self.phone),
            id_number: None,
        }
    }

    /// `None` when the slot holds nothing.
    pub(crate) fn into_address(self, slot: &str) -> Option<Address> {
        let a = Address {
            id: slot.into(),
            kind: Some(slot.into()),
            first_name: non_empty(self.first_name),
            last_name: non_empty(self.last_name),
            company: non_empty(self.company),
            address1: non_empty(self.address_1),
            address2: non_empty(self.address_2),
            city: non_empty(self.city),
            postcode: non_empty(self.postcode),
            state: non_empty(self.state),
            country: non_empty(self.country),
            phone: non_empty(self.phone),
            id_number: None,
        };
        let blank = Address {
            id: a.id.clone(),
            kind: a.kind.clone(),
            ..Default::default()
        };
        (a != blank).then_some(a)
    }
}

#[derive(Deserialize)]
pub(crate) struct WooCustomer {
    #[serde(deserialize_with = "id")]
    id: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    first_name: String,
    #[serde(default)]
    last_name: String,
    #[serde(default)]
    orders_count: Option<u64>,
    #[serde(default, deserialize_with = "opt_string")]
    total_spent: Option<String>,
    #[serde(default)]
    pub(crate) billing: WooAddress,
    #[serde(default)]
    pub(crate) shipping: WooAddress,
}

impl WooCustomer {
    pub(crate) fn email(&self) -> Option<&String> {
        Some(&self.email).filter(|e| !e.trim().is_empty())
    }
}

impl TryFrom<WooCustomer> for Customer {
    type Error = ConnectorError;
    fn try_from(c: WooCustomer) -> Result<Self, ConnectorError> {
        Ok(Customer {
            phone: non_empty(c.billing.phone),
            accepts_marketing: None,
            total_spent: money(c.total_spent.as_deref())?,
            id: c.id,
            email: c.email,
            first_name: c.first_name,
            last_name: c.last_name,
            orders_count: c.orders_count,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct WooSalesReport {
    #[serde(default)]
    total_sales: Value,
    #[serde(default)]
    net_sales: Value,
    #[serde(default)]
    total_orders: Value,
    #[serde(default)]
    total_items: Value,
}

fn value_decimal(v: &Value) -> Result<Decimal, ConnectorError> {
    match v {
        Value::String(s) => money_or_zero(s),
        Value::Number(n) => money_or_zero(&n.to_string()),
        _ => Ok(Decimal::ZERO),
    }
}

fn value_u64(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

#[derive(Deserialize)]
pub(crate) struct WooCategory {
    #[serde(deserialize_with = "id")]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "opt_string")]
    parent: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    description: Option<String>,
    #[serde(default)]
    count: Option<u64>,
}

impl TryFrom<WooCategory> for Category {
    type Error = ConnectorError;
    fn try_from(c: WooCategory) -> Result<Self, ConnectorError> {
        Ok(Category {
            id: c.id,
            name: c.name,
            // 0 means a top-level category.
            parent_id: c.parent.filter(|p| p != "0"),
            description: c.description,
            count: c.count,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct WooCoupon {
    #[serde(deserialize_with = "id")]
    id: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    discount_type: String,
    #[serde(default)]
    amount: String,
    #[serde(default, deserialize_with = "opt_string")]
    date_expires: Option<String>,
    #[serde(default)]
    usage_limit: Option<u64>,
    #[serde(default)]
    usage_count: Option<u64>,
    #[serde(default, deserialize_with = "opt_string")]
    minimum_amount: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    status: Option<String>,
    #[serde(default, deserialize_with = "opt_string")]
    description: Option<String>,
}

impl TryFrom<WooCoupon> for Coupon {
    type Error = ConnectorError;
    fn try_from(c: WooCoupon) -> Result<Self, ConnectorError> {
        let expires_on = c
            .date_expires
            .as_deref()
            .and_then(|d| chrono::NaiveDate::parse_from_str(d.get(..10)?, "%Y-%m-%d").ok());
        Ok(Coupon {
            id: c.id,
            code: c.code,
            kind: c.discount_type,
            amount: money_or_zero(&c.amount)?,
            expires_on,
            usage_limit: c.usage_limit,
            usage_count: c.usage_count,
            minimum_amount: money(c.minimum_amount.as_deref())?.filter(|m| !m.is_zero()),
            // Only a published coupon applies; drafts do not.
            active: c.status.map(|s| s == "publish"),
            description: c.description,
        })
    }
}

impl WooSalesReport {
    pub(crate) fn into_report(self, range: &DateRange) -> Result<SalesReport, ConnectorError> {
        Ok(SalesReport {
            from: range.from,
            to: range.to,
            total_sales: value_decimal(&self.total_sales)?,
            net_sales: value_decimal(&self.net_sales)?,
            orders: value_u64(&self.total_orders),
            items: value_u64(&self.total_items),
            currency: None,
        })
    }
}
