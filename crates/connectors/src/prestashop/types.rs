//! Mapping webservice JSON into domain types. The webservice returns most
//! numbers as strings and multilang fields as `[{"id": 1, "value": "..."}]`.

use std::str::FromStr;

use rust_decimal::Decimal;
use serde_json::Value;

use crate::domain::*;
use crate::ConnectorError;

pub(crate) fn decimal(v: &Value) -> Result<Option<Decimal>, ConnectorError> {
    let text = match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Ok(None),
    };
    Decimal::from_str(&text)
        .map(|d| Some(d.normalize()))
        .map_err(|e| ConnectorError::InvalidResponse(format!("bad amount {text:?}: {e}")))
}

pub(crate) fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The value of a multilang field in its first language with text, which
/// is the language the shop was installed in.
pub(crate) fn text(v: &Value) -> Option<String> {
    match v {
        Value::Array(langs) => langs.iter().find_map(|l| string(&l["value"])),
        other => string(other),
    }
}

/// Language ids and values of a multilang field, in the shop's order.
pub(crate) fn languages(v: &Value) -> Vec<(u64, String)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            Some((
                int(&l["id"])? as u64,
                string(&l["value"]).unwrap_or_default(),
            ))
        })
        .collect()
}

/// Products with combinations keep a summed stock row; the real stock is per
/// combination.
pub(crate) fn has_combinations(p: &Value) -> bool {
    p["product_type"] == "combinations" || int(&p["cache_default_attribute"]).unwrap_or(0) > 0
}

/// Prices are the shop's base prices, which PrestaShop keeps excluding tax.
/// `final` is the computed price after the product's reductions.
pub(crate) fn product(p: &Value, stock: Option<i64>) -> Result<Product, ConnectorError> {
    let id = string(&p["id"])
        .ok_or_else(|| ConnectorError::InvalidResponse("product without id".into()))?;
    let regular = decimal(&p["price"])?;
    let price = decimal(&p["final"])?.or(regular);
    let on_sale = matches!((price, regular), (Some(f), Some(r)) if f < r);
    Ok(Product {
        category_ids: Vec::new(),
        id,
        name: text(&p["name"]).unwrap_or_default(),
        sku: string(&p["reference"]),
        status: Some(if int(&p["active"]) == Some(1) {
            "active".into()
        } else {
            "inactive".into()
        }),
        price,
        regular_price: regular,
        sale_price: if on_sale { price } else { None },
        stock_quantity: stock,
        stock_status: stock.map(|q| if q > 0 { "instock" } else { "outofstock" }.to_string()),
    })
}

pub(crate) fn active(status: &str) -> Result<bool, ConnectorError> {
    match status.trim().to_ascii_lowercase().as_str() {
        "active" | "enabled" | "enable" | "publish" | "published" | "1" => Ok(true),
        "inactive" | "disabled" | "disable" | "draft" | "private" | "0" => Ok(false),
        other => Err(ConnectorError::Api {
            status: 400,
            message: format!("unknown product status {other:?}; use active or inactive"),
        }),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OrderState {
    pub id: i64,
    pub name: String,
    pub paid: bool,
    pub shipped: bool,
}

pub(crate) fn order_state(s: &Value) -> Option<OrderState> {
    Some(OrderState {
        id: int(&s["id"])?,
        name: text(&s["name"]).unwrap_or_default(),
        paid: int(&s["paid"]) == Some(1),
        shipped: int(&s["shipped"]) == Some(1),
    })
}

fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Order states are named per shop and language, so a merchant's word is
/// matched against them: a state id, an exact name, a common word from other
/// platforms, or a unique part of a name.
pub(crate) fn match_states<'a>(
    word: &str,
    states: &'a [OrderState],
) -> Result<Vec<&'a OrderState>, ConnectorError> {
    let w = squash(word);
    if let Ok(id) = w.parse::<i64>() {
        return Ok(states.iter().filter(|s| s.id == id).collect());
    }
    let exact: Vec<_> = states.iter().filter(|s| squash(&s.name) == w).collect();
    if !exact.is_empty() {
        return Ok(exact);
    }
    let by = |pred: &dyn Fn(&OrderState) -> bool| states.iter().filter(|s| pred(s)).collect();
    let named = |part: &'static str| move |s: &OrderState| squash(&s.name).contains(part);
    let found: Vec<&OrderState> = match w.as_str() {
        "cancelled" | "canceled" | "cancel" => by(&named("cancel")),
        "completed" | "complete" | "delivered" => by(&named("deliver")),
        "processing" => by(&named("processing")),
        "paid" | "paymentaccepted" => by(&named("paymentaccepted")),
        "pending" | "unpaid" | "awaitingpayment" => {
            by(&|s: &OrderState| !s.paid && squash(&s.name).contains("waiting"))
        }
        _ => by(&|s: &OrderState| squash(&s.name).contains(&w)),
    };
    if found.is_empty() {
        let names: Vec<_> = states.iter().map(|s| s.name.as_str()).collect();
        return Err(ConnectorError::Api {
            status: 400,
            message: format!(
                "no order status matches {word:?}; this shop's statuses are: {}",
                names.join(", ")
            ),
        });
    }
    Ok(found)
}

pub(crate) struct OrderRefs<'a> {
    pub states: &'a [OrderState],
    pub currency: Option<String>,
    pub customer: Option<(String, String, String)>,
}

pub(crate) fn order(o: &Value, refs: &OrderRefs) -> Result<Order, ConnectorError> {
    let id = string(&o["id"])
        .ok_or_else(|| ConnectorError::InvalidResponse("order without id".into()))?;
    let state = int(&o["current_state"]);
    let status = refs
        .states
        .iter()
        .find(|s| Some(s.id) == state)
        .map(|s| s.name.clone())
        .or_else(|| state.map(|s| s.to_string()))
        .unwrap_or_default();
    let line_items = o["associations"]["order_rows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let qty = int(&r["product_quantity"]).unwrap_or(0);
            let unit = decimal(&r["unit_price_tax_incl"])?.unwrap_or_default();
            Ok(LineItem {
                product_id: string(&r["product_id"]),
                name: string(&r["product_name"]).unwrap_or_default(),
                quantity: qty,
                total: unit * Decimal::from(qty),
            })
        })
        .collect::<Result<_, ConnectorError>>()?;
    let (name, email) = match &refs.customer {
        Some((first, last, email)) => {
            let n = format!("{first} {last}").trim().to_string();
            (Some(n).filter(|n| !n.is_empty()), Some(email.clone()))
        }
        None => (None, None),
    };
    Ok(Order {
        number: string(&o["reference"]).unwrap_or_else(|| id.clone()),
        id,
        status,
        currency: refs.currency.clone().unwrap_or_default(),
        total: decimal(&o["total_paid_tax_incl"])?.unwrap_or_default(),
        created_at: string(&o["date_add"]),
        customer_id: string(&o["id_customer"]).filter(|c| c != "0"),
        customer_name: name,
        customer_email: email.filter(|e| !e.is_empty()),
        line_items,
    })
}

pub(crate) fn customer(c: &Value) -> Result<Customer, ConnectorError> {
    Ok(Customer {
        phone: None,
        accepts_marketing: None,
        id: string(&c["id"])
            .ok_or_else(|| ConnectorError::InvalidResponse("customer without id".into()))?,
        email: string(&c["email"]).unwrap_or_default(),
        first_name: string(&c["firstname"]).unwrap_or_default(),
        last_name: string(&c["lastname"]).unwrap_or_default(),
        orders_count: None,
        total_spent: None,
    })
}

/// The webservice's `{"errors": [{"code", "message"}]}`, or the raw body.
pub(crate) fn error_message(body: &str) -> String {
    let messages: Vec<String> = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["errors"].as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|e| e["message"].as_str().map(str::to_string))
        .collect();
    if messages.is_empty() {
        body.chars().take(300).collect()
    } else {
        messages.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn states() -> Vec<OrderState> {
        [
            (1, "Awaiting check payment", false),
            (2, "Payment accepted", true),
            (3, "Processing in progress", true),
            (5, "Delivered", true),
            (6, "Canceled", false),
            (9, "On backorder (paid)", true),
            (10, "Awaiting bank wire payment", false),
            (12, "On backorder (not paid)", false),
        ]
        .into_iter()
        .map(|(id, name, paid)| OrderState {
            id,
            name: name.into(),
            paid,
            shipped: false,
        })
        .collect()
    }

    fn ids(found: Vec<&OrderState>) -> Vec<i64> {
        found.into_iter().map(|s| s.id).collect()
    }

    #[test]
    fn merchant_words_match_shop_order_states() {
        let s = states();
        assert_eq!(ids(match_states("cancelled", &s).unwrap()), [6]);
        assert_eq!(ids(match_states("Payment accepted", &s).unwrap()), [2]);
        assert_eq!(ids(match_states("completed", &s).unwrap()), [5]);
        assert_eq!(ids(match_states("pending", &s).unwrap()), [1, 10]);
        assert_eq!(ids(match_states("backorder", &s).unwrap()), [9, 12]);
        assert_eq!(ids(match_states("3", &s).unwrap()), [3]);
        let err = match_states("teleported", &s).unwrap_err().to_string();
        assert!(
            err.contains("Payment accepted, Processing in progress"),
            "{err}"
        );
    }

    #[test]
    fn products_read_computed_sale_prices_and_first_language_names() {
        let p = product(
            &json!({"id": 1, "reference": "demo_1", "price": "23.900000", "final": "19.120000",
                "active": "1", "name": [{"id": 1, "value": "T-shirt"}, {"id": 2, "value": "Camiseta"}]}),
            Some(0),
        )
        .unwrap();
        assert_eq!(p.name, "T-shirt");
        assert_eq!(p.regular_price, Decimal::from_str("23.9").ok());
        assert_eq!(p.sale_price, Decimal::from_str("19.12").ok());
        assert_eq!(p.status.as_deref(), Some("active"));
        assert_eq!(p.stock_status.as_deref(), Some("outofstock"));
        let no_sale = product(
            &json!({"id": 2, "price": "10.000000", "final": "10.000000", "active": "0", "name": "Mug"}),
            None,
        )
        .unwrap();
        assert_eq!(
            (no_sale.sale_price, no_sale.status.as_deref()),
            (None, Some("inactive"))
        );
    }

    #[test]
    fn errors_join_webservice_messages() {
        assert_eq!(
            error_message(
                r#"{"errors":[{"code":26,"message":"Resource of type \"carriers\" is not allowed with this authentication key"}]}"#
            ),
            "Resource of type \"carriers\" is not allowed with this authentication key"
        );
        assert_eq!(error_message("401 Unauthorized"), "401 Unauthorized");
    }
}
