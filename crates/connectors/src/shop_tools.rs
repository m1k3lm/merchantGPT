//! Turns any `ShopConnector` into agent tools. Written once, so a new platform
//! only implements `ShopConnector` and gets every tool for free.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use tools::{parse_args, Mutability, Tool, ToolContext, ToolDefinition, ToolError};

use crate::domain::*;
use crate::ShopConnector;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShopOp {
    ListProducts,
    GetProduct,
    UpdateProduct,
    ListOrders,
    GetOrder,
    UpdateOrderStatus,
    ListCustomers,
    SalesReport,
}

const ALL_OPS: [ShopOp; 8] = [
    ShopOp::ListProducts,
    ShopOp::GetProduct,
    ShopOp::UpdateProduct,
    ShopOp::ListOrders,
    ShopOp::GetOrder,
    ShopOp::UpdateOrderStatus,
    ShopOp::ListCustomers,
    ShopOp::SalesReport,
];

pub fn shop_tools(shop: Arc<dyn ShopConnector>) -> Vec<Arc<dyn Tool>> {
    ALL_OPS
        .into_iter()
        .map(|op| {
            Arc::new(ShopTool {
                shop: shop.clone(),
                op,
            }) as Arc<dyn Tool>
        })
        .collect()
}

struct ShopTool {
    shop: Arc<dyn ShopConnector>,
    op: ShopOp,
}

fn paging() -> Value {
    json!({
        "page": {"type": "integer", "minimum": 1, "description": "1-based page number"},
        "per_page": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Items per page (default 10)"}
    })
}

fn object(mut props: Value, extra: Value, required: &[&str]) -> Value {
    if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
        p.extend(e.clone());
    }
    json!({"type": "object", "properties": props, "required": required})
}

fn id_schema(what: &str) -> Value {
    object(
        json!({"id": {"type": "string", "description": format!("{what} id")}}),
        json!({}),
        &["id"],
    )
}

impl ShopTool {
    fn spec(&self) -> (&'static str, &'static str, Value, Mutability) {
        use Mutability::*;
        match self.op {
            ShopOp::ListProducts => (
                "shop_list_products",
                "Search and list products in the shop catalog.",
                object(
                    json!({
                        "search": {"type": "string", "description": "Free-text search"},
                        "sku": {"type": "string"},
                        "status": {"type": "string", "description": "e.g. publish, draft, private"}
                    }),
                    paging(),
                    &[],
                ),
                Read,
            ),
            ShopOp::GetProduct => (
                "shop_get_product",
                "Get one product by id.",
                id_schema("Product"),
                Read,
            ),
            ShopOp::UpdateProduct => (
                "shop_update_product",
                "Update product fields (name, sku, status, prices, stock). Only send fields that change. Prices are decimal strings.",
                object(
                    json!({
                        "id": {"type": "string", "description": "Product id"},
                        "name": {"type": "string"},
                        "sku": {"type": "string"},
                        "status": {"type": "string"},
                        "regular_price": {"type": "string", "description": "Decimal, e.g. \"19.90\""},
                        "sale_price": {"type": "string", "description": "Decimal, e.g. \"14.90\""},
                        "stock_quantity": {"type": "integer"}
                    }),
                    json!({}),
                    &["id"],
                ),
                Mutate,
            ),
            ShopOp::ListOrders => (
                "shop_list_orders",
                "List orders, newest first, optionally filtered by status, customer or date.",
                object(
                    json!({
                        "status": {"type": "string", "description": "e.g. pending, processing, completed, cancelled, refunded"},
                        "customer_id": {"type": "string"},
                        "after": {"type": "string", "format": "date", "description": "YYYY-MM-DD, inclusive"},
                        "before": {"type": "string", "format": "date", "description": "YYYY-MM-DD, inclusive"}
                    }),
                    paging(),
                    &[],
                ),
                Read,
            ),
            ShopOp::GetOrder => ("shop_get_order", "Get one order by id, including line items.", id_schema("Order"), Read),
            ShopOp::UpdateOrderStatus => (
                "shop_update_order_status",
                "Change an order's status.",
                object(
                    json!({
                        "id": {"type": "string", "description": "Order id"},
                        "status": {"type": "string", "description": "New status, e.g. completed"}
                    }),
                    json!({}),
                    &["id", "status"],
                ),
                Mutate,
            ),
            ShopOp::ListCustomers => (
                "shop_list_customers",
                "Search and list customers.",
                object(
                    json!({
                        "search": {"type": "string"},
                        "email": {"type": "string"}
                    }),
                    paging(),
                    &[],
                ),
                Read,
            ),
            ShopOp::SalesReport => (
                "shop_sales_report",
                "Sales totals (gross, net, orders, items) for a date range.",
                object(
                    json!({
                        "from": {"type": "string", "format": "date", "description": "YYYY-MM-DD, inclusive"},
                        "to": {"type": "string", "format": "date", "description": "YYYY-MM-DD, inclusive"}
                    }),
                    json!({}),
                    &["from", "to"],
                ),
                Read,
            ),
        }
    }
}

/// Models send ids as either strings or numbers.
fn string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "expected string or number, got {other}"
        ))),
    }
}

#[derive(Deserialize)]
struct IdArgs {
    #[serde(deserialize_with = "string_or_number")]
    id: String,
}

#[derive(Deserialize)]
struct UpdateProductArgs {
    #[serde(deserialize_with = "string_or_number")]
    id: String,
    #[serde(flatten)]
    patch: ProductPatch,
}

#[derive(Deserialize)]
struct UpdateOrderStatusArgs {
    #[serde(deserialize_with = "string_or_number")]
    id: String,
    status: String,
}

fn to_json<T: Serialize>(v: T) -> Result<String, ToolError> {
    serde_json::to_string(&v).map_err(|e| ToolError::Failed(e.to_string()))
}

#[async_trait]
impl Tool for ShopTool {
    fn definition(&self) -> ToolDefinition {
        let (name, description, parameters, mutability) = self.spec();
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters,
            mutability,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let shop = &self.shop;
        match self.op {
            ShopOp::ListProducts => to_json(shop.list_products(&parse_args(args)?).await?),
            ShopOp::GetProduct => {
                let a: IdArgs = parse_args(args)?;
                to_json(shop.get_product(&a.id).await?)
            }
            ShopOp::UpdateProduct => {
                let a: UpdateProductArgs = parse_args(args)?;
                if a.patch.is_empty() {
                    return Err(ToolError::InvalidArguments("no fields to update".into()));
                }
                to_json(shop.update_product(&a.id, &a.patch).await?)
            }
            ShopOp::ListOrders => to_json(shop.list_orders(&parse_args(args)?).await?),
            ShopOp::GetOrder => {
                let a: IdArgs = parse_args(args)?;
                to_json(shop.get_order(&a.id).await?)
            }
            ShopOp::UpdateOrderStatus => {
                let a: UpdateOrderStatusArgs = parse_args(args)?;
                to_json(shop.update_order_status(&a.id, &a.status).await?)
            }
            ShopOp::ListCustomers => to_json(shop.list_customers(&parse_args(args)?).await?),
            ShopOp::SalesReport => {
                let r: DateRange = parse_args(args)?;
                to_json(shop.sales_report(&r).await?)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stub::StubShop;
    use crate::Platform;
    use tools::is_valid_tool_name;

    fn tools_for(p: Platform) -> Vec<Arc<dyn Tool>> {
        shop_tools(Arc::new(StubShop(p)))
    }

    fn find(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
        tools
            .iter()
            .find(|t| t.definition().name == name)
            .cloned()
            .unwrap()
    }

    #[test]
    fn shop_tools_exposes_read_and_mutate_tools_with_valid_names() {
        let defs: Vec<_> = tools_for(Platform::Shopify)
            .iter()
            .map(|t| t.definition())
            .collect();
        assert_eq!(defs.len(), ALL_OPS.len());
        for d in &defs {
            assert!(is_valid_tool_name(&d.name), "{}", d.name);
            assert_eq!(d.parameters["type"], "object", "{}", d.name);
        }
        let mutating: Vec<_> = defs
            .iter()
            .filter(|d| d.mutability == Mutability::Mutate)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(
            mutating,
            ["shop_update_product", "shop_update_order_status"]
        );
    }

    #[tokio::test]
    async fn stub_platform_tools_return_not_implemented() {
        let tools = tools_for(Platform::Magento);
        let err = find(&tools, "shop_list_orders")
            .call(json!({}), &ToolContext::default())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ToolError::Failed("the Magento connector is not implemented yet".into())
        );
    }

    #[tokio::test]
    async fn arguments_are_validated_before_reaching_the_connector() {
        let tools = tools_for(Platform::Magento);
        let ctx = ToolContext::default();
        let missing_id = find(&tools, "shop_get_product").call(json!({}), &ctx).await;
        assert!(matches!(missing_id, Err(ToolError::InvalidArguments(_))));
        let empty_patch = find(&tools, "shop_update_product")
            .call(json!({"id": 5}), &ctx)
            .await;
        assert_eq!(
            empty_patch,
            Err(ToolError::InvalidArguments("no fields to update".into()))
        );
        let numeric_id_with_price = find(&tools, "shop_update_product")
            .call(json!({"id": 5, "regular_price": "9.90"}), &ctx)
            .await;
        assert!(matches!(numeric_id_with_price, Err(ToolError::Failed(_))));
    }
}
