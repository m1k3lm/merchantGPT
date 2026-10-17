//! Normalised shapes shared by every shop connector. Platform payloads are
//! mapped into these at the connector boundary.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

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
