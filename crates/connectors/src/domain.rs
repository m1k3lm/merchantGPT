//! Normalised shapes shared by every shop connector. Platform payloads are
//! mapped into these at the connector boundary.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Models send ids as either strings or numbers.
pub fn string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "expected string or number, got {other}"
        ))),
    }
}

pub fn opt_string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected string or number, got {other}"
        ))),
    }
}

fn opt_ids<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    let Some(values) = Option::<Vec<Value>>::deserialize(d)? else {
        return Ok(None);
    };
    values
        .into_iter()
        .map(|v| match v {
            Value::String(s) => Ok(s),
            Value::Number(n) => Ok(n.to_string()),
            other => Err(serde::de::Error::custom(format!(
                "expected an id, got {other}"
            ))),
        })
        .collect::<Result<_, _>>()
        .map(Some)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub page: u32,
    pub total_pages: Option<u32>,
    pub total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub sku: Option<String>,
    pub status: Option<String>,
    pub price: Option<Decimal>,
    pub regular_price: Option<Decimal>,
    pub sale_price: Option<Decimal>,
    pub stock_quantity: Option<i64>,
    pub stock_status: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub category_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProductPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sku: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regular_price: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sale_price: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stock_quantity: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        default,
        deserialize_with = "opt_ids",
        skip_serializing_if = "Option::is_none"
    )]
    pub category_ids: Option<Vec<String>>,
}

impl ProductPatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProductQuery {
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub sku: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub per_page: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineItem {
    pub product_id: Option<String>,
    pub name: String,
    pub quantity: i64,
    pub total: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub number: String,
    pub status: String,
    pub currency: String,
    pub total: Decimal,
    pub created_at: Option<String>,
    pub customer_id: Option<String>,
    pub customer_name: Option<String>,
    pub customer_email: Option<String>,
    pub line_items: Vec<LineItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OrderQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub customer_id: Option<String>,
    #[serde(default)]
    pub after: Option<NaiveDate>,
    #[serde(default)]
    pub before: Option<NaiveDate>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub per_page: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Customer {
    pub id: String,
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    pub orders_count: Option<u64>,
    pub total_spent: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// Whether the customer opted in to marketing; `None` when the platform
    /// does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepts_marketing: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomerQuery {
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub per_page: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateRange {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SalesReport {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub total_sales: Decimal,
    pub net_sales: Decimal,
    pub orders: u64,
    pub items: u64,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomerPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepts_marketing: Option<bool>,
}

impl CustomerPatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Category {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Products in the category, when the platform reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CategoryPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        default,
        deserialize_with = "opt_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl CategoryPatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CategoryQuery {
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub per_page: Option<u32>,
}

/// A postal address. `kind` is the platform's slot or flag where it has one
/// (WooCommerce: `billing` / `shipping`). Countries are ISO 3166-1 alpha-2.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Address {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub company: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address2: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postcode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// Tax or national id (DNI, NIE, NIF, VAT) some countries require on addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_number: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AddressPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub company: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address2: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postcode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// Tax or national id (DNI, NIE, NIF, VAT) some countries require on addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_number: Option<String>,
}

impl AddressPatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewLineItem {
    #[serde(deserialize_with = "string_or_number")]
    pub product_id: String,
    pub quantity: i64,
    /// Overrides the catalog price where the platform allows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Decimal>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewOrder {
    #[serde(
        default,
        deserialize_with = "opt_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub customer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default)]
    pub line_items: Vec<NewLineItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<AddressPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shipping: Option<AddressPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderNote {
    pub note: String,
    /// Emails the note to the customer where the platform can.
    #[serde(default)]
    pub notify_customer: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coupon {
    pub id: String,
    pub code: String,
    /// `percent`, `fixed_cart` or `fixed_product`.
    pub kind: String,
    pub amount: Decimal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_amount: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CouponPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_amount: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl CouponPatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CouponQuery {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub per_page: Option<u32>,
}

/// A cart counts as abandoned once untouched this long without becoming an order.
pub const ABANDONED_AFTER_HOURS: i64 = 2;

/// A cart or checkout that has items but did not become an order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbandonedCart {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(default)]
    pub line_items: Vec<LineItem>,
    /// A link that restores the cart for the customer, where the platform has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CartQuery {
    /// Only carts last changed on or after this day.
    #[serde(default)]
    pub since: Option<NaiveDate>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub per_page: Option<u32>,
}

/// A request to the platform's own API. REST platforms use `path` (relative
/// to the API root), `query` and `body`; Shopify sends `body` as a GraphQL
/// document with `variables`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RawRequest {
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub query: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variables: Option<Value>,
}

fn default_method() -> String {
    "GET".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawResponse {
    pub status: u16,
    pub body: Value,
}
