//! Mapping Magento REST payloads into domain types. Magento is loose with
//! types (numbers as strings, booleans as 0/1), so payloads are read as JSON
//! values rather than strict structs.

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
        .or_else(|_| Decimal::from_scientific(&text))
        .map(|d| Some(d.normalize()))
        .map_err(|e| ConnectorError::InvalidResponse(format!("bad amount {text:?}: {e}")))
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        Value::String(s) => matches!(s.as_str(), "1" | "true"),
        _ => false,
    }
}

fn string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn custom_attribute<'a>(p: &'a Value, code: &str) -> Option<&'a Value> {
    p["custom_attributes"]
        .as_array()?
        .iter()
        .find(|a| a["attribute_code"] == code)
        .map(|a| &a["value"])
}

pub(crate) fn status_label(code: &Value) -> Option<String> {
    match number(code).map(|n| n as i64) {
        Some(1) => Some("enabled".into()),
        Some(2) => Some("disabled".into()),
        _ => string(code),
    }
}

/// Products are identified by SKU, which is how Magento's API addresses them.
pub(crate) fn product(p: &Value) -> Result<Product, ConnectorError> {
    let sku = string(&p["sku"])
        .ok_or_else(|| ConnectorError::InvalidResponse("product without sku".into()))?;
    let price = decimal(&p["price"])?;
    let stock = &p["extension_attributes"]["stock_item"];
    Ok(Product {
        id: sku.clone(),
        name: string(&p["name"]).unwrap_or_default(),
        sku: Some(sku),
        status: status_label(&p["status"]),
        price,
        regular_price: price,
        sale_price: match custom_attribute(p, "special_price") {
            Some(v) => decimal(v)?,
            None => None,
        },
        stock_quantity: number(&stock["qty"]).map(|q| q.round() as i64),
        stock_status: stock.get("is_in_stock").map(|v| {
            if truthy(v) {
                "instock".to_string()
            } else {
                "outofstock".to_string()
            }
        }),
    })
}

pub(crate) fn order(o: &Value) -> Result<Order, ConnectorError> {
    let id = string(&o["entity_id"])
        .ok_or_else(|| ConnectorError::InvalidResponse("order without entity_id".into()))?;
    let guest = truthy(&o["customer_is_guest"]);
    let billing = &o["billing_address"];
    let first = string(&o["customer_firstname"]).or_else(|| string(&billing["firstname"]));
    let last = string(&o["customer_lastname"]).or_else(|| string(&billing["lastname"]));
    let name = [first, last]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let line_items = o["items"]
        .as_array()
        .into_iter()
        .flatten()
        // Configurable products appear twice: keep the parent line only.
        .filter(|i| i["parent_item_id"].is_null())
        .map(|i| {
            Ok(LineItem {
                product_id: string(&i["product_id"]),
                name: string(&i["name"]).unwrap_or_default(),
                quantity: number(&i["qty_ordered"]).unwrap_or(0.0).round() as i64,
                total: decimal(&i["row_total"])?.unwrap_or_default(),
            })
        })
        .collect::<Result<_, ConnectorError>>()?;
    Ok(Order {
        number: string(&o["increment_id"]).unwrap_or_else(|| id.clone()),
        id,
        status: string(&o["status"]).unwrap_or_default(),
        currency: string(&o["order_currency_code"]).unwrap_or_default(),
        total: decimal(&o["grand_total"])?.unwrap_or_default(),
        created_at: string(&o["created_at"]),
        customer_id: if guest {
            None
        } else {
            string(&o["customer_id"])
        },
        customer_name: Some(name).filter(|n| !n.is_empty()),
        customer_email: string(&o["customer_email"]),
        line_items,
    })
}

pub(crate) fn customer(c: &Value) -> Result<Customer, ConnectorError> {
    Ok(Customer {
        id: string(&c["id"])
            .ok_or_else(|| ConnectorError::InvalidResponse("customer without id".into()))?,
        email: string(&c["email"]).unwrap_or_default(),
        first_name: string(&c["firstname"]).unwrap_or_default(),
        last_name: string(&c["lastname"]).unwrap_or_default(),
        orders_count: None,
        total_spent: None,
    })
}

/// Magento messages use `%name` / `%1` placeholders filled from `parameters`.
pub(crate) fn error_message(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.chars().take(300).collect();
    };
    let mut msg = v["message"].as_str().unwrap_or("unknown error").to_string();
    match &v["parameters"] {
        Value::Object(map) => {
            for (k, val) in map {
                msg = msg.replace(&format!("%{k}"), &string(val).unwrap_or_default());
            }
        }
        Value::Array(list) => {
            for (i, val) in list.iter().enumerate().rev() {
                msg = msg.replace(&format!("%{}", i + 1), &string(val).unwrap_or_default());
            }
        }
        _ => {}
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn error_placeholders_are_filled() {
        assert_eq!(
            error_message(
                r#"{"message":"The consumer isn't authorized to access %resources.","parameters":{"resources":"Magento_Sales::sales"}}"#
            ),
            "The consumer isn't authorized to access Magento_Sales::sales."
        );
        assert_eq!(
            error_message(
                r#"{"message":"No such entity with %fieldName = %fieldValue","parameters":{"fieldName":"orderId","fieldValue":"9"}}"#
            ),
            "No such entity with orderId = 9"
        );
        assert_eq!(
            error_message(
                r#"{"message":"Invalid value of \"%1\" provided for the %2 field.","parameters":["x","sku"]}"#
            ),
            "Invalid value of \"x\" provided for the sku field."
        );
    }

    #[test]
    fn decimals_accept_numbers_and_strings() {
        assert_eq!(
            decimal(&json!(39.9)).unwrap(),
            Decimal::from_str("39.9").ok()
        );
        assert_eq!(
            decimal(&json!("14.9000")).unwrap(),
            Decimal::from_str("14.9").ok()
        );
        assert_eq!(decimal(&json!(null)).unwrap(), None);
    }
}
