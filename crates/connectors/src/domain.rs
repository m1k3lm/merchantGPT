//! Normalised shapes shared by every connector. Platform-specific payloads are
//! mapped into these at the connector boundary.

#[derive(Debug, Clone, PartialEq)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub sku: Option<String>,
    pub price: f64,
    pub stock: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub id: String,
    pub status: String,
    pub total: f64,
    pub customer_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Customer {
    pub id: String,
    pub email: String,
    pub name: String,
}
