//! Mapping Admin API payloads into domain types, and the merchant's words for
//! statuses into Shopify's search syntax and enums.

use std::str::FromStr;

use rust_decimal::Decimal;
use serde_json::Value;

use crate::domain::*;
use crate::ConnectorError;

const DEFAULT_VARIANT_TITLE: &str = "Default Title";

pub(crate) fn decimal(v: &Value) -> Result<Option<Decimal>, ConnectorError> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Decimal::from_str(s.trim())
            .map(|d| Some(d.normalize()))
            .map_err(|e| ConnectorError::InvalidResponse(format!("bad amount {s:?}: {e}"))),
        Value::Number(n) => Decimal::from_str(&n.to_string())
            .map(|d| Some(d.normalize()))
            .map_err(|e| ConnectorError::InvalidResponse(format!("bad amount {n}: {e}"))),
        _ => Ok(None),
    }
}

fn string(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn count(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Accepts a full gid or the numeric id shown in the Shopify admin's URLs.
pub(crate) fn gid(kind: &str, id: &str) -> Result<String, ConnectorError> {
    let id = id.trim();
    let prefix = format!("gid://shopify/{kind}/");
    let numeric = id.strip_prefix(&prefix).unwrap_or(id);
    if !numeric.is_empty() && numeric.bytes().all(|b| b.is_ascii_digit()) {
        Ok(format!("{prefix}{numeric}"))
    } else {
        Err(ConnectorError::NotFound(format!(
            "{id:?} is not a Shopify {kind} id (expected {prefix}<number>)"
        )))
    }
}

/// Quotes a value for Shopify's search syntax.
pub(crate) fn quoted(s: &str) -> String {
    format!(
        "\"{}\"",
        s.trim().replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// Variants are what carries a price, SKU and stock in Shopify, so each one
/// is a product here, named after its product plus the variant's options.
pub(crate) fn product(v: &Value) -> Result<Product, ConnectorError> {
    let id = string(&v["id"])
        .ok_or_else(|| ConnectorError::InvalidResponse("variant without id".into()))?;
    let title = string(&v["product"]["title"]).unwrap_or_default();
    let name = match string(&v["title"]) {
        Some(t) if t != DEFAULT_VARIANT_TITLE => format!("{title} - {t}"),
        _ => title,
    };
    let price = decimal(&v["price"])?;
    let compare_at = decimal(&v["compareAtPrice"])?;
    let on_sale = matches!((price, compare_at), (Some(p), Some(c)) if c > p);
    let tracked = v["inventoryItem"]["tracked"].as_bool().unwrap_or(false);
    let stock = v["inventoryQuantity"].as_i64().filter(|_| tracked);
    Ok(Product {
        id,
        name,
        sku: string(&v["sku"]),
        status: string(&v["product"]["status"]).map(|s| s.to_ascii_lowercase()),
        price,
        regular_price: if on_sale { compare_at } else { price },
        sale_price: if on_sale { price } else { None },
        stock_quantity: stock,
        stock_status: stock.map(|q| if q > 0 { "instock" } else { "outofstock" }.to_string()),
    })
}

/// Shopify has no single order status: it is cancelled, or a financial and a
/// fulfillment status, plus "closed" once archived.
pub(crate) fn order_status(o: &Value) -> String {
    if !o["cancelledAt"].is_null() {
        return "cancelled".into();
    }
    let mut parts: Vec<String> = ["displayFinancialStatus", "displayFulfillmentStatus"]
        .iter()
        .filter_map(|k| o[*k].as_str())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    if o["closed"].as_bool() == Some(true) {
        parts.push("closed".into());
    }
    parts.join(", ")
}

pub(crate) fn order(o: &Value) -> Result<Order, ConnectorError> {
    let id = string(&o["id"])
        .ok_or_else(|| ConnectorError::InvalidResponse("order without id".into()))?;
    let customer = &o["customer"];
    let names = |a: &Value| {
        let n = [string(&a["firstName"]), string(&a["lastName"])]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        Some(n).filter(|n| !n.is_empty())
    };
    let line_items = o["lineItems"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            Ok(LineItem {
                product_id: string(&i["variant"]["id"]),
                name: string(&i["name"]).unwrap_or_default(),
                quantity: i["currentQuantity"].as_i64().unwrap_or(0),
                total: decimal(&i["discountedTotalSet"]["shopMoney"]["amount"])?
                    .unwrap_or_default(),
            })
        })
        .collect::<Result<_, ConnectorError>>()?;
    let money = &o["totalPriceSet"]["shopMoney"];
    Ok(Order {
        number: string(&o["name"]).unwrap_or_else(|| id.clone()),
        status: order_status(o),
        currency: string(&money["currencyCode"]).unwrap_or_default(),
        total: decimal(&money["amount"])?.unwrap_or_default(),
        created_at: string(&o["createdAt"]),
        customer_id: string(&customer["id"]),
        customer_name: names(customer).or_else(|| names(&o["billingAddress"])),
        customer_email: string(&o["email"]),
        line_items,
        id,
    })
}

pub(crate) fn customer(c: &Value) -> Result<Customer, ConnectorError> {
    Ok(Customer {
        id: string(&c["id"])
            .ok_or_else(|| ConnectorError::InvalidResponse("customer without id".into()))?,
        email: string(&c["defaultEmailAddress"]["emailAddress"]).unwrap_or_default(),
        first_name: string(&c["firstName"]).unwrap_or_default(),
        last_name: string(&c["lastName"]).unwrap_or_default(),
        orders_count: count(&c["numberOfOrders"]),
        total_spent: decimal(&c["amountSpent"]["amount"])?,
    })
}

pub(crate) fn product_status(status: &str) -> Result<&'static str, ConnectorError> {
    match status.trim().to_ascii_lowercase().as_str() {
        "active" | "publish" | "published" | "enabled" | "enable" => Ok("ACTIVE"),
        "draft" | "private" | "disabled" | "disable" | "inactive" => Ok("DRAFT"),
        "archived" | "archive" => Ok("ARCHIVED"),
        "unlisted" => Ok("UNLISTED"),
        other => Err(ConnectorError::Api {
            status: 400,
            message: format!("unknown product status {other:?}; use active, draft or archived"),
        }),
    }
}

const FINANCIAL: [&str; 8] = [
    "paid",
    "pending",
    "authorized",
    "partially_paid",
    "partially_refunded",
    "refunded",
    "voided",
    "expired",
];
const FULFILLMENT: [&str; 8] = [
    "unshipped",
    "shipped",
    "fulfilled",
    "partial",
    "scheduled",
    "on_hold",
    "unfulfilled",
    "request_declined",
];

/// Turns a status filter into search terms. Accepts Shopify's own values,
/// the comma-separated form `order_status` produces, and common words from
/// other platforms.
pub(crate) fn order_status_filter(status: &str) -> Result<String, ConnectorError> {
    let mut terms = Vec::new();
    for word in status.split(',').map(|w| w.trim().to_ascii_lowercase()) {
        let word = word.replace(['-', ' '], "_");
        let term = match word.as_str() {
            "" => continue,
            "open" | "closed" | "cancelled" | "not_closed" => format!("status:{word}"),
            "canceled" => "status:cancelled".into(),
            "archived" => "status:closed".into(),
            "processing" => "status:open fulfillment_status:unfulfilled".into(),
            "completed" | "complete" => "fulfillment_status:fulfilled".into(),
            "partially_fulfilled" => "fulfillment_status:partial".into(),
            w if FINANCIAL.contains(&w) => format!("financial_status:{w}"),
            w if FULFILLMENT.contains(&w) => format!("fulfillment_status:{w}"),
            other => {
                return Err(ConnectorError::Api {
                    status: 400,
                    message: format!(
                        "unknown Shopify order status {other:?}; use open, closed, cancelled, a payment status ({}) or a fulfillment status ({})",
                        FINANCIAL.join(", "),
                        FULFILLMENT.join(", ")
                    ),
                })
            }
        };
        terms.push(term);
    }
    Ok(terms.join(" "))
}

/// The text of GraphQL `errors`, or of a mutation's `userErrors`.
pub(crate) fn messages(errors: &Value) -> Vec<String> {
    errors
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let msg = e["message"].as_str().unwrap_or("unknown error");
            match e["field"].as_array().filter(|f| !f.is_empty()) {
                Some(field) => {
                    let path: Vec<&str> = field.iter().filter_map(Value::as_str).collect();
                    format!("{}: {msg}", path.join("."))
                }
                None => msg.to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn variants_map_to_products_with_sale_prices_and_stock() {
        let p = product(&json!({
            "id": "gid://shopify/ProductVariant/11", "title": "Blue / XL", "sku": "HOOD-BLU-XL",
            "price": "29.90", "compareAtPrice": "39.90", "inventoryQuantity": 0,
            "inventoryItem": {"id": "gid://shopify/InventoryItem/7", "tracked": true},
            "product": {"id": "gid://shopify/Product/1", "title": "Hoodie", "status": "ACTIVE"}
        }))
        .unwrap();
        assert_eq!(p.name, "Hoodie - Blue / XL");
        assert_eq!(p.status.as_deref(), Some("active"));
        assert_eq!(
            (p.price, p.regular_price, p.sale_price),
            (Some(dec("29.9")), Some(dec("39.9")), Some(dec("29.9")))
        );
        assert_eq!(p.stock_status.as_deref(), Some("outofstock"));

        let single = product(&json!({
            "id": "gid://shopify/ProductVariant/12", "title": "Default Title", "price": "10.00",
            "compareAtPrice": null, "inventoryQuantity": 4,
            "inventoryItem": {"tracked": false},
            "product": {"title": "Mug", "status": "DRAFT"}
        }))
        .unwrap();
        assert_eq!(single.name, "Mug");
        assert_eq!(single.sale_price, None);
        assert_eq!(single.regular_price, Some(dec("10")));
        assert_eq!(single.stock_quantity, None, "untracked stock is unknown");
    }

    #[test]
    fn order_status_combines_payment_fulfillment_and_closed() {
        let o = json!({"cancelledAt": null, "closed": true,
            "displayFinancialStatus": "PAID", "displayFulfillmentStatus": "FULFILLED"});
        assert_eq!(order_status(&o), "paid, fulfilled, closed");
        let c =
            json!({"cancelledAt": "2026-10-01T10:00:00Z", "displayFinancialStatus": "REFUNDED"});
        assert_eq!(order_status(&c), "cancelled");
    }

    #[test]
    fn status_filters_accept_shopify_and_other_platform_words() {
        assert_eq!(
            order_status_filter("paid, unfulfilled").unwrap(),
            "financial_status:paid fulfillment_status:unfulfilled"
        );
        assert_eq!(
            order_status_filter("processing").unwrap(),
            "status:open fulfillment_status:unfulfilled"
        );
        assert_eq!(
            order_status_filter("On hold").unwrap(),
            "fulfillment_status:on_hold"
        );
        assert!(order_status_filter("shipped-ish").is_err());
    }

    #[test]
    fn ids_accept_numbers_and_reject_other_kinds() {
        assert_eq!(gid("Order", "42").unwrap(), "gid://shopify/Order/42");
        assert_eq!(
            gid("Order", "gid://shopify/Order/42").unwrap(),
            "gid://shopify/Order/42"
        );
        assert!(gid("ProductVariant", "gid://shopify/Product/1").is_err());
        assert!(gid("Order", "#1001").is_err());
    }

    #[test]
    fn search_values_are_quoted_and_escaped() {
        assert_eq!(quoted(r#" say "hi" "#), r#""say \"hi\"""#);
    }

    #[test]
    fn user_errors_name_their_field() {
        let e = json!([{"field": ["variants", "0", "price"], "message": "is invalid"}, {"message": "Throttled"}]);
        assert_eq!(messages(&e), ["variants.0.price: is invalid", "Throttled"]);
    }
}
