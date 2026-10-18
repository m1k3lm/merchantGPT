//! Turns any `ShopConnector` into agent tools. Written once, so a new platform
//! only implements `ShopConnector` and gets every tool for free. Writes are
//! grouped per entity with an `action` argument: fewer definitions to send on
//! every turn, and the confirmation card shows the action.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tools::{parse_args, Mutability, Tool, ToolContext, ToolDefinition, ToolError};

use crate::domain::*;
use crate::ShopConnector;

/// Raw API responses beyond this are cut, so one call cannot flood the context.
const RAW_OUTPUT_LIMIT: usize = 20_000;

/// Location rows beyond this are summed into one "other" row.
const LOCATION_ROW_LIMIT: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShopOp {
    ListProducts,
    GetProduct,
    ListCategories,
    ListOrders,
    GetOrder,
    ListCustomers,
    GetCustomer,
    ListCoupons,
    ListAbandonedCarts,
    SalesReport,
    ApiGet,
    ManageProduct,
    ManageCategory,
    ManageCustomer,
    ManageAddress,
    ManageCoupon,
    ManageOrder,
    ApiWrite,
}

const ALL_OPS: [ShopOp; 18] = [
    ShopOp::ListProducts,
    ShopOp::GetProduct,
    ShopOp::ListCategories,
    ShopOp::ListOrders,
    ShopOp::GetOrder,
    ShopOp::ListCustomers,
    ShopOp::GetCustomer,
    ShopOp::ListCoupons,
    ShopOp::ListAbandonedCarts,
    ShopOp::SalesReport,
    ShopOp::ApiGet,
    ShopOp::ManageProduct,
    ShopOp::ManageCategory,
    ShopOp::ManageCustomer,
    ShopOp::ManageAddress,
    ShopOp::ManageCoupon,
    ShopOp::ManageOrder,
    ShopOp::ApiWrite,
];

impl ShopOp {
    fn name(self) -> &'static str {
        match self {
            ShopOp::ListProducts => "shop_list_products",
            ShopOp::GetProduct => "shop_get_product",
            ShopOp::ListCategories => "shop_list_categories",
            ShopOp::ListOrders => "shop_list_orders",
            ShopOp::GetOrder => "shop_get_order",
            ShopOp::ListCustomers => "shop_list_customers",
            ShopOp::GetCustomer => "shop_get_customer",
            ShopOp::ListCoupons => "shop_list_coupons",
            ShopOp::ListAbandonedCarts => "shop_list_abandoned_carts",
            ShopOp::SalesReport => "shop_sales_report",
            ShopOp::ApiGet => "shop_api_get",
            ShopOp::ManageProduct => "shop_manage_product",
            ShopOp::ManageCategory => "shop_manage_category",
            ShopOp::ManageCustomer => "shop_manage_customer",
            ShopOp::ManageAddress => "shop_manage_address",
            ShopOp::ManageCoupon => "shop_manage_coupon",
            ShopOp::ManageOrder => "shop_manage_order",
            ShopOp::ApiWrite => "shop_api_write",
        }
    }

    fn actions(self) -> &'static [&'static str] {
        match self {
            ShopOp::ManageOrder => &["create", "update_status", "add_note", "delete"],
            ShopOp::ManageProduct
            | ShopOp::ManageCategory
            | ShopOp::ManageCustomer
            | ShopOp::ManageAddress
            | ShopOp::ManageCoupon => &["create", "update", "delete"],
            _ => &[],
        }
    }
}

pub fn shop_tools(shop: Arc<dyn ShopConnector>) -> Vec<Arc<dyn Tool>> {
    ALL_OPS
        .into_iter()
        .filter_map(|op| {
            let actions: Vec<&'static str> = op
                .actions()
                .iter()
                .copied()
                .filter(|a| !is_hidden(shop.as_ref(), op, Some(a)))
                .collect();
            let hidden = is_hidden(shop.as_ref(), op, None)
                || (!op.actions().is_empty() && actions.is_empty());
            (!hidden).then(|| {
                Arc::new(ShopTool {
                    shop: shop.clone(),
                    op,
                    actions,
                }) as Arc<dyn Tool>
            })
        })
        .collect()
}

fn is_hidden(shop: &dyn ShopConnector, op: ShopOp, action: Option<&str>) -> bool {
    shop.hidden()
        .iter()
        .any(|h| match (h.split_once(':'), action) {
            (None, _) => *h == op.name(),
            (Some((tool, a)), Some(action)) => tool == op.name() && a == action,
            (Some(_), None) => false,
        })
}

struct ShopTool {
    shop: Arc<dyn ShopConnector>,
    op: ShopOp,
    actions: Vec<&'static str>,
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

fn address_props() -> Value {
    json!({
        "kind": {"type": "string", "description": "Platform slot where it has one, e.g. billing or shipping"},
        "first_name": {"type": "string"}, "last_name": {"type": "string"}, "company": {"type": "string"},
        "address1": {"type": "string"}, "address2": {"type": "string"}, "city": {"type": "string"},
        "postcode": {"type": "string"}, "state": {"type": "string"},
        "country": {"type": "string", "description": "ISO 3166-1 alpha-2, e.g. ES"},
        "phone": {"type": "string"},
        "id_number": {"type": "string", "description": "Tax or national id (DNI/NIE/NIF) where the country requires one"}
    })
}

fn decimal(description: &str) -> Value {
    json!({"type": "string", "description": format!("Decimal string, e.g. \"19.90\". {description}")})
}

impl ShopTool {
    fn manage(&self, props: Value, required: &[&str]) -> Value {
        let mut req = vec!["action"];
        req.extend(required);
        object(
            json!({"action": {"type": "string", "enum": self.actions}}),
            props,
            &req,
        )
    }

    fn spec(&self) -> (String, Value, Mutability) {
        use Mutability::*;
        let (description, parameters, mutability): (String, Value, Mutability) = match self.op {
            ShopOp::ListProducts => (
                "Search and list products in the shop catalog.".into(),
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
            ShopOp::GetProduct => ("Get one product by id.".into(), id_schema("Product"), Read),
            ShopOp::ListCategories => (
                "List product categories (Shopify: collections).".into(),
                object(json!({"search": {"type": "string"}}), paging(), &[]),
                Read,
            ),
            ShopOp::ListOrders => (
                "List orders, newest first, optionally filtered by status, customer or date.".into(),
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
            ShopOp::GetOrder => (
                "Get one order by id, including line items.".into(),
                id_schema("Order"),
                Read,
            ),
            ShopOp::ListCustomers => (
                "Search and list customers.".into(),
                object(
                    json!({"search": {"type": "string"}, "email": {"type": "string"}}),
                    paging(),
                    &[],
                ),
                Read,
            ),
            ShopOp::GetCustomer => (
                "Get one customer by id, with their addresses and marketing consent.".into(),
                id_schema("Customer"),
                Read,
            ),
            ShopOp::ListCoupons => (
                "List discount coupons.".into(),
                object(json!({"code": {"type": "string"}}), paging(), &[]),
                Read,
            ),
            ShopOp::ListAbandonedCarts => (
                "List carts or checkouts with items that did not become an order, with the customer's contact and a recovery link where the platform has one.".into(),
                object(
                    json!({"since": {"type": "string", "format": "date", "description": "Only carts changed on or after YYYY-MM-DD"}}),
                    paging(),
                    &[],
                ),
                Read,
            ),
            ShopOp::SalesReport => (
                "Sales totals (gross, net, orders, items) for a date range, or with `group_by` the number of orders and gross sales per country or per region (province/state), by shipping address. Use it for any per-country or per-province question instead of reading orders one by one.".into(),
                object(
                    json!({
                        "from": {"type": "string", "format": "date", "description": "YYYY-MM-DD, inclusive"},
                        "to": {"type": "string", "format": "date", "description": "YYYY-MM-DD, inclusive"},
                        "group_by": {"type": "string", "enum": ["country", "region"], "description": "Break down by country, or by country and region"}
                    }),
                    json!({}),
                    &["from", "to"],
                ),
                Read,
            ),
            ShopOp::ApiGet => (
                format!(
                    "Read anything the other tools do not cover from the shop's own API. {}",
                    self.shop.api_hint()
                ),
                object(raw_props(), json!({}), &[]),
                Read,
            ),
            ShopOp::ManageProduct => (
                "Create, update or delete a product. Update sends only the fields that change; create needs a name. A sale_price of 0 ends a sale.".into(),
                self.manage(
                    json!({
                        "id": {"type": "string", "description": "Product id (update, delete)"},
                        "name": {"type": "string"}, "sku": {"type": "string"}, "status": {"type": "string"},
                        "regular_price": decimal(""), "sale_price": decimal(""),
                        "stock_quantity": {"type": "integer"},
                        "description": {"type": "string", "description": "HTML allowed"},
                        "category_ids": {"type": "array", "items": {"type": "string"}, "description": "Replaces the product's categories"}
                    }),
                    &[],
                ),
                Mutate,
            ),
            ShopOp::ManageCategory => (
                "Create, update or delete a product category (Shopify: collection). Create needs a name.".into(),
                self.manage(
                    json!({
                        "id": {"type": "string", "description": "Category id (update, delete)"},
                        "name": {"type": "string"},
                        "parent_id": {"type": "string"},
                        "description": {"type": "string"}
                    }),
                    &[],
                ),
                Mutate,
            ),
            ShopOp::ManageCustomer => (
                "Create, update or delete a customer. Create needs an email. Set accepts_marketing only with the customer's consent.".into(),
                self.manage(
                    json!({
                        "id": {"type": "string", "description": "Customer id (update, delete)"},
                        "email": {"type": "string"}, "first_name": {"type": "string"}, "last_name": {"type": "string"},
                        "phone": {"type": "string"}, "accepts_marketing": {"type": "boolean"}
                    }),
                    &[],
                ),
                Mutate,
            ),
            ShopOp::ManageAddress => (
                "Create, update or delete one of a customer's addresses.".into(),
                self.manage(
                    object(
                        json!({
                            "customer_id": {"type": "string"},
                            "id": {"type": "string", "description": "Address id (update, delete)"}
                        }),
                        address_props(),
                        &[],
                    )["properties"]
                        .clone(),
                    &["customer_id"],
                ),
                Mutate,
            ),
            ShopOp::ManageCoupon => (
                "Create, update or delete a discount coupon. Create needs code, kind and amount.".into(),
                self.manage(
                    json!({
                        "id": {"type": "string", "description": "Coupon id (update, delete)"},
                        "code": {"type": "string"},
                        "kind": {"type": "string", "enum": ["percent", "fixed_cart", "fixed_product"]},
                        "amount": decimal("Percent or currency amount."),
                        "expires_on": {"type": "string", "format": "date"},
                        "usage_limit": {"type": "integer"},
                        "minimum_amount": decimal("Minimum order amount."),
                        "active": {"type": "boolean"},
                        "description": {"type": "string"}
                    }),
                    &[],
                ),
                Mutate,
            ),
            ShopOp::ManageOrder => (
                "Create an order, change its status, add a note (notify_customer emails it to the customer where the platform can), or delete it.".into(),
                self.manage(
                    json!({
                        "id": {"type": "string", "description": "Order id (update_status, add_note, delete)"},
                        "status": {"type": "string"},
                        "note": {"type": "string"},
                        "notify_customer": {"type": "boolean"},
                        "customer_id": {"type": "string", "description": "create: existing customer"},
                        "email": {"type": "string", "description": "create: guest email"},
                        "line_items": {"type": "array", "items": {"type": "object", "properties": {
                            "product_id": {"type": "string"},
                            "quantity": {"type": "integer", "minimum": 1},
                            "price": decimal("Overrides the catalog price where allowed.")
                        }, "required": ["product_id", "quantity"]}},
                        "billing": {"type": "object", "properties": address_props()},
                        "shipping": {"type": "object", "properties": address_props()}
                    }),
                    &[],
                ),
                Mutate,
            ),
            ShopOp::ApiWrite => (
                format!(
                    "Change anything the other tools do not cover through the shop's own API (POST, PUT, PATCH, DELETE). {}",
                    self.shop.api_hint()
                ),
                object(raw_props(), json!({}), &["method"]),
                Mutate,
            ),
        };
        (description, parameters, mutability)
    }
}

fn raw_props() -> Value {
    json!({
        "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"]},
        "path": {"type": "string", "description": "Relative to the API root, e.g. /products/12"},
        "query": {"type": "object", "additionalProperties": {"type": "string"}},
        "body": {"description": "JSON body, XML string, or GraphQL document, as the API expects"},
        "variables": {"type": "object", "description": "GraphQL variables"}
    })
}

#[derive(Deserialize)]
struct IdArgs {
    #[serde(deserialize_with = "string_or_number")]
    id: String,
}

#[derive(Deserialize)]
struct Manage<T> {
    action: String,
    #[serde(default, deserialize_with = "opt_string_or_number")]
    id: Option<String>,
    #[serde(flatten)]
    fields: T,
}

#[derive(Deserialize)]
struct ManageAddressArgs {
    #[serde(deserialize_with = "string_or_number")]
    customer_id: String,
    #[serde(flatten)]
    rest: Manage<AddressPatch>,
}

#[derive(Deserialize)]
struct ManageOrderArgs {
    action: String,
    #[serde(default, deserialize_with = "opt_string_or_number")]
    id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    notify_customer: bool,
    #[serde(flatten)]
    order: NewOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GroupBy {
    Country,
    Region,
}

#[derive(Deserialize)]
struct SalesArgs {
    #[serde(flatten)]
    range: DateRange,
    #[serde(default)]
    group_by: Option<GroupBy>,
}

fn by_location(report: LocationReport, by: GroupBy) -> Value {
    let mut rows = report.rows;
    if by == GroupBy::Country {
        let mut countries: Vec<LocationSales> = Vec::new();
        for r in rows {
            match countries.iter_mut().find(|c| c.country == r.country) {
                Some(c) => {
                    c.orders += r.orders;
                    c.total_sales += r.total_sales;
                }
                None => countries.push(LocationSales { region: None, ..r }),
            }
        }
        countries.sort_by_key(|r| std::cmp::Reverse(r.total_sales));
        rows = countries;
    }
    let orders: u64 = rows.iter().map(|r| r.orders).sum();
    let total_sales: rust_decimal::Decimal = rows.iter().map(|r| r.total_sales).sum();
    if rows.len() > LOCATION_ROW_LIMIT {
        let rest = rows.split_off(LOCATION_ROW_LIMIT - 1);
        rows.push(LocationSales {
            country: format!("other ({} places)", rest.len()),
            region: None,
            orders: rest.iter().map(|r| r.orders).sum(),
            total_sales: rest.iter().map(|r| r.total_sales).sum(),
        });
    }
    json!({
        "from": report.from,
        "to": report.to,
        "currency": report.currency,
        "group_by": if by == GroupBy::Country { "country" } else { "region" },
        "orders": orders,
        "total_sales": total_sales,
        "rows": rows,
    })
}

fn to_json<T: Serialize>(v: T) -> Result<String, ToolError> {
    serde_json::to_string(&v).map_err(|e| ToolError::Failed(e.to_string()))
}

fn need<T>(v: Option<T>, what: &str) -> Result<T, ToolError> {
    v.ok_or_else(|| ToolError::InvalidArguments(format!("{what} is required")))
}

fn changes(empty: bool) -> Result<(), ToolError> {
    if empty {
        Err(ToolError::InvalidArguments("no fields to change".into()))
    } else {
        Ok(())
    }
}

fn deleted(id: &str) -> Result<String, ToolError> {
    to_json(json!({"deleted": true, "id": id}))
}

/// Paths stay on the shop's API: credentials must never follow a request
/// to another host.
fn check_raw(req: &RawRequest) -> Result<(), ToolError> {
    let p = req.path.trim();
    let bad = p.contains("://")
        || p.starts_with("//")
        || p.contains('?')
        || p.contains('#')
        || p.contains('\\')
        || p.split('/').any(|seg| seg == ".." || seg == ".");
    if bad {
        return Err(ToolError::InvalidArguments(
            "path must be relative to the API root, without a host, `..`, `?` or `#`; put query parameters in `query`".into(),
        ));
    }
    Ok(())
}

fn capped(resp: RawResponse) -> Result<String, ToolError> {
    let text = to_json(&resp)?;
    if text.len() <= RAW_OUTPUT_LIMIT {
        return Ok(text);
    }
    let cut = (0..=RAW_OUTPUT_LIMIT)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    Ok(format!(
        "{}… [response cut at {RAW_OUTPUT_LIMIT} of {} characters; ask for fewer fields or a smaller page]",
        &text[..cut],
        text.len()
    ))
}

#[async_trait]
impl Tool for ShopTool {
    fn definition(&self) -> ToolDefinition {
        let (description, parameters, mutability) = self.spec();
        ToolDefinition {
            name: self.op.name().into(),
            description,
            parameters,
            mutability,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let shop = &self.shop;
        if let Some(action) = args.get("action").and_then(Value::as_str) {
            if !self.actions.is_empty() && !self.actions.contains(&action) {
                let known = self.op.actions().contains(&action);
                return Err(if known {
                    ToolError::Failed(format!(
                        "{} cannot {action} through this connector",
                        shop.platform().display_name()
                    ))
                } else {
                    ToolError::InvalidArguments(format!(
                        "action must be one of: {}",
                        self.actions.join(", ")
                    ))
                });
            }
        }
        match self.op {
            ShopOp::ListProducts => to_json(shop.list_products(&parse_args(args)?).await?),
            ShopOp::GetProduct => {
                let a: IdArgs = parse_args(args)?;
                to_json(shop.get_product(&a.id).await?)
            }
            ShopOp::ListCategories => to_json(shop.list_categories(&parse_args(args)?).await?),
            ShopOp::ListOrders => to_json(shop.list_orders(&parse_args(args)?).await?),
            ShopOp::GetOrder => {
                let a: IdArgs = parse_args(args)?;
                to_json(shop.get_order(&a.id).await?)
            }
            ShopOp::ListCustomers => to_json(shop.list_customers(&parse_args(args)?).await?),
            ShopOp::GetCustomer => {
                let a: IdArgs = parse_args(args)?;
                let customer = shop.get_customer(&a.id).await?;
                // Addresses are extra detail; a platform without them still answers.
                let addresses = shop.list_addresses(&a.id).await.ok();
                to_json(json!({"customer": customer, "addresses": addresses}))
            }
            ShopOp::ListCoupons => to_json(shop.list_coupons(&parse_args(args)?).await?),
            ShopOp::ListAbandonedCarts => {
                to_json(shop.list_abandoned_carts(&parse_args(args)?).await?)
            }
            ShopOp::SalesReport => {
                let a: SalesArgs = parse_args(args)?;
                match a.group_by {
                    None => to_json(shop.sales_report(&a.range).await?),
                    Some(by) => to_json(by_location(shop.sales_by_location(&a.range).await?, by)),
                }
            }
            ShopOp::ApiGet => {
                let mut req: RawRequest = parse_args(args)?;
                req.method = "GET".into();
                check_raw(&req)?;
                capped(shop.raw(&req).await?)
            }
            ShopOp::ApiWrite => {
                let mut req: RawRequest = parse_args(args)?;
                req.method = req.method.trim().to_ascii_uppercase();
                if !matches!(req.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
                    return Err(ToolError::InvalidArguments(
                        "method must be POST, PUT, PATCH or DELETE; read with shop_api_get".into(),
                    ));
                }
                check_raw(&req)?;
                capped(shop.raw(&req).await?)
            }
            ShopOp::ManageProduct => {
                let a: Manage<ProductPatch> = parse_args(args)?;
                match a.action.as_str() {
                    "create" => {
                        need(a.fields.name.as_ref(), "name")?;
                        to_json(shop.create_product(&a.fields).await?)
                    }
                    "update" => {
                        let id = need(a.id, "id")?;
                        changes(a.fields.is_empty())?;
                        to_json(shop.update_product(&id, &a.fields).await?)
                    }
                    _ => {
                        let id = need(a.id, "id")?;
                        shop.delete_product(&id).await?;
                        deleted(&id)
                    }
                }
            }
            ShopOp::ManageCategory => {
                let a: Manage<CategoryPatch> = parse_args(args)?;
                match a.action.as_str() {
                    "create" => {
                        need(a.fields.name.as_ref(), "name")?;
                        to_json(shop.create_category(&a.fields).await?)
                    }
                    "update" => {
                        let id = need(a.id, "id")?;
                        changes(a.fields.is_empty())?;
                        to_json(shop.update_category(&id, &a.fields).await?)
                    }
                    _ => {
                        let id = need(a.id, "id")?;
                        shop.delete_category(&id).await?;
                        deleted(&id)
                    }
                }
            }
            ShopOp::ManageCustomer => {
                let a: Manage<CustomerPatch> = parse_args(args)?;
                match a.action.as_str() {
                    "create" => {
                        need(a.fields.email.as_ref(), "email")?;
                        to_json(shop.create_customer(&a.fields).await?)
                    }
                    "update" => {
                        let id = need(a.id, "id")?;
                        changes(a.fields.is_empty())?;
                        to_json(shop.update_customer(&id, &a.fields).await?)
                    }
                    _ => {
                        let id = need(a.id, "id")?;
                        shop.delete_customer(&id).await?;
                        deleted(&id)
                    }
                }
            }
            ShopOp::ManageAddress => {
                let a: ManageAddressArgs = parse_args(args)?;
                let (customer, m) = (a.customer_id, a.rest);
                match m.action.as_str() {
                    "create" => {
                        changes(m.fields.is_empty())?;
                        to_json(shop.create_address(&customer, &m.fields).await?)
                    }
                    "update" => {
                        let id = need(m.id, "id")?;
                        changes(m.fields.is_empty())?;
                        to_json(shop.update_address(&customer, &id, &m.fields).await?)
                    }
                    _ => {
                        let id = need(m.id, "id")?;
                        shop.delete_address(&customer, &id).await?;
                        deleted(&id)
                    }
                }
            }
            ShopOp::ManageCoupon => {
                let a: Manage<CouponPatch> = parse_args(args)?;
                match a.action.as_str() {
                    "create" => {
                        need(a.fields.code.as_ref(), "code")?;
                        need(a.fields.kind.as_ref(), "kind")?;
                        need(a.fields.amount.as_ref(), "amount")?;
                        to_json(shop.create_coupon(&a.fields).await?)
                    }
                    "update" => {
                        let id = need(a.id, "id")?;
                        changes(a.fields.is_empty())?;
                        to_json(shop.update_coupon(&id, &a.fields).await?)
                    }
                    _ => {
                        let id = need(a.id, "id")?;
                        shop.delete_coupon(&id).await?;
                        deleted(&id)
                    }
                }
            }
            ShopOp::ManageOrder => {
                let a: ManageOrderArgs = parse_args(args)?;
                match a.action.as_str() {
                    "create" => {
                        if a.order.line_items.is_empty() {
                            return Err(ToolError::InvalidArguments(
                                "line_items is required".into(),
                            ));
                        }
                        let mut order = a.order;
                        order.status = a.status;
                        order.note = a.note;
                        to_json(shop.create_order(&order).await?)
                    }
                    "update_status" => {
                        let id = need(a.id, "id")?;
                        let status = need(a.status, "status")?;
                        to_json(shop.update_order_status(&id, &status).await?)
                    }
                    "add_note" => {
                        let id = need(a.id, "id")?;
                        let note = OrderNote {
                            note: need(a.note, "note")?,
                            notify_customer: a.notify_customer,
                        };
                        shop.add_order_note(&id, &note).await?;
                        to_json(
                            json!({"added": true, "order_id": id, "notified": note.notify_customer}),
                        )
                    }
                    _ => {
                        let id = need(a.id, "id")?;
                        shop.delete_order(&id).await?;
                        deleted(&id)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stub::StubShop;
    use crate::{ConnectorError, Platform};
    use std::sync::Mutex;
    use tools::is_valid_tool_name;

    fn tools_for(p: Platform) -> Vec<Arc<dyn Tool>> {
        shop_tools(Arc::new(StubShop(p)))
    }

    fn find(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
        tools
            .iter()
            .find(|t| t.definition().name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no tool {name}"))
    }

    /// Hides abandoned carts and order deletes, and records raw requests.
    struct Picky {
        raw: Mutex<Vec<RawRequest>>,
        body: Value,
    }

    #[async_trait]
    impl ShopConnector for Picky {
        fn platform(&self) -> Platform {
            Platform::Shopify
        }
        fn hidden(&self) -> &'static [&'static str] {
            &["shop_list_abandoned_carts", "shop_manage_order:delete"]
        }
        fn api_hint(&self) -> &'static str {
            "GraphQL Admin API."
        }
        async fn verify(&self) -> Result<(), ConnectorError> {
            Ok(())
        }
        async fn list_products(&self, _: &ProductQuery) -> Result<Page<Product>, ConnectorError> {
            unimplemented!()
        }
        async fn get_product(&self, _: &str) -> Result<Product, ConnectorError> {
            unimplemented!()
        }
        async fn update_product(
            &self,
            _: &str,
            _: &ProductPatch,
        ) -> Result<Product, ConnectorError> {
            unimplemented!()
        }
        async fn list_orders(&self, _: &OrderQuery) -> Result<Page<Order>, ConnectorError> {
            unimplemented!()
        }
        async fn get_order(&self, _: &str) -> Result<Order, ConnectorError> {
            unimplemented!()
        }
        async fn update_order_status(&self, _: &str, _: &str) -> Result<Order, ConnectorError> {
            unimplemented!()
        }
        async fn list_customers(
            &self,
            _: &CustomerQuery,
        ) -> Result<Page<Customer>, ConnectorError> {
            unimplemented!()
        }
        async fn sales_report(&self, _: &DateRange) -> Result<SalesReport, ConnectorError> {
            unimplemented!()
        }
        async fn sales_by_location(
            &self,
            range: &DateRange,
        ) -> Result<LocationReport, ConnectorError> {
            let mut tally = LocationTally::default();
            let eur = Some("EUR");
            for (country, region, total, n) in [
                (Some("ES"), Some("Madrid"), "100.50", 2),
                (Some("ES"), Some("Barcelona"), "80", 1),
                (Some("PT"), None, "120", 1),
                (None, None, "5", 1),
            ] {
                for _ in 0..n {
                    tally.add(country, region, total.parse().unwrap(), eur);
                }
            }
            Ok(tally.finish(range))
        }
        async fn delete_category(&self, _: &str) -> Result<(), ConnectorError> {
            Ok(())
        }
        async fn raw(&self, req: &RawRequest) -> Result<RawResponse, ConnectorError> {
            self.raw.lock().unwrap().push(req.clone());
            Ok(RawResponse {
                status: 200,
                body: self.body.clone(),
            })
        }
    }

    fn picky(body: Value) -> (Arc<Picky>, Vec<Arc<dyn Tool>>) {
        let shop = Arc::new(Picky {
            raw: Mutex::new(Vec::new()),
            body,
        });
        let tools = shop_tools(shop.clone());
        (shop, tools)
    }

    #[tokio::test]
    async fn sales_report_breaks_down_by_region_or_by_country() {
        let (_, tools) = picky(json!({}));
        let report = find(&tools, "shop_sales_report");
        let call = |by: &str| {
            let (report, args) = (
                report.clone(),
                json!({"from": "2026-10-01", "to": "2026-10-18", "group_by": by}),
            );
            async move { report.call(args, &ToolContext::default()).await }
        };
        let regions: Value = serde_json::from_str(&call("region").await.unwrap()).unwrap();
        assert_eq!(regions["orders"], 5);
        assert_eq!(regions["total_sales"], "406.00");
        assert_eq!(regions["currency"], "EUR");
        assert_eq!(regions["rows"][0]["region"], "Madrid");
        assert_eq!(regions["rows"][0]["orders"], 2);
        assert_eq!(regions["rows"][0]["total_sales"], "201.00");
        assert_eq!(regions["rows"][3]["country"], "unknown");

        let countries: Value = serde_json::from_str(&call("country").await.unwrap()).unwrap();
        let rows = countries["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            json!({"country": "ES", "orders": 3, "total_sales": "281.00"})
        );
        assert_eq!(rows[1]["country"], "PT");
        assert_eq!(countries["total_sales"], "406.00");

        assert!(call("city").await.is_err());
    }

    #[test]
    fn many_places_end_in_one_other_row() {
        let mut tally = LocationTally::default();
        for i in 0..150 {
            tally.add(Some("ES"), Some(&format!("r{i:03}")), (i + 1).into(), None);
        }
        let range = DateRange {
            from: "2026-10-01".parse().unwrap(),
            to: "2026-10-01".parse().unwrap(),
        };
        let v = by_location(tally.finish(&range), GroupBy::Region);
        let rows = v["rows"].as_array().unwrap();
        assert_eq!(rows.len(), LOCATION_ROW_LIMIT);
        assert_eq!(rows[0]["region"], "r149");
        assert_eq!(rows[LOCATION_ROW_LIMIT - 1]["country"], "other (51 places)");
        assert_eq!(v["orders"], 150);
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
            [
                "shop_manage_product",
                "shop_manage_category",
                "shop_manage_customer",
                "shop_manage_address",
                "shop_manage_coupon",
                "shop_manage_order",
                "shop_api_write"
            ]
        );
    }

    #[test]
    fn hidden_tools_and_actions_are_left_out() {
        let (_, tools) = picky(json!({}));
        assert!(tools
            .iter()
            .all(|t| t.definition().name != "shop_list_abandoned_carts"));
        let order = find(&tools, "shop_manage_order").definition();
        assert_eq!(
            order.parameters["properties"]["action"]["enum"],
            json!(["create", "update_status", "add_note"])
        );
        assert!(find(&tools, "shop_api_get")
            .definition()
            .description
            .ends_with("GraphQL Admin API."));
    }

    #[tokio::test]
    async fn hidden_actions_are_refused_when_called_anyway() {
        let (_, tools) = picky(json!({}));
        let err = find(&tools, "shop_manage_order")
            .call(
                json!({"action": "delete", "id": "1"}),
                &ToolContext::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ToolError::Failed("Shopify cannot delete through this connector".into())
        );
        let unknown = find(&tools, "shop_manage_category")
            .call(
                json!({"action": "explode", "id": "1"}),
                &ToolContext::default(),
            )
            .await;
        assert!(matches!(unknown, Err(ToolError::InvalidArguments(_))));
        let ok = find(&tools, "shop_manage_category")
            .call(
                json!({"action": "delete", "id": 7}),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(ok, r#"{"deleted":true,"id":"7"}"#);
    }

    #[tokio::test]
    async fn raw_requests_stay_on_the_shop_api() {
        let (shop, tools) = picky(json!({"ok": true}));
        let ctx = ToolContext::default();
        let get = find(&tools, "shop_api_get");
        for path in [
            "https://evil.test/steal",
            "//evil.test/x",
            "/products/../../admin",
            "/products?per_page=5",
        ] {
            let r = get.call(json!({"path": path}), &ctx).await;
            assert!(matches!(r, Err(ToolError::InvalidArguments(_))), "{path}");
        }
        let out = get
            .call(
                json!({"method": "DELETE", "path": "/products/categories", "query": {"per_page": "5"}}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out, r#"{"status":200,"body":{"ok":true}}"#);
        let sent = shop.raw.lock().unwrap().clone();
        assert_eq!(sent[0].method, "GET", "the read tool always reads");
        assert_eq!(sent[0].query["per_page"], "5");

        let write = find(&tools, "shop_api_write");
        let r = write
            .call(json!({"method": "get", "path": "/x"}), &ctx)
            .await;
        assert!(matches!(r, Err(ToolError::InvalidArguments(_))));
        write
            .call(
                json!({"method": "patch", "path": "x/1", "body": {"a": 1}}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(shop.raw.lock().unwrap()[1].method, "PATCH");
    }

    #[tokio::test]
    async fn raw_output_is_capped() {
        let (_, tools) = picky(json!({"blob": "é".repeat(RAW_OUTPUT_LIMIT)}));
        let out = find(&tools, "shop_api_get")
            .call(json!({"path": "/big"}), &ToolContext::default())
            .await
            .unwrap();
        assert!(out.len() < RAW_OUTPUT_LIMIT + 200);
        assert!(out.contains("response cut at"));
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
    async fn unimplemented_operations_say_the_platform_cannot_do_them() {
        let tools = tools_for(Platform::Magento);
        let err = find(&tools, "shop_list_coupons")
            .call(json!({}), &ToolContext::default())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ToolError::Failed("Magento does not support coupons through its API".into())
        );
    }

    #[tokio::test]
    async fn arguments_are_validated_before_reaching_the_connector() {
        let tools = tools_for(Platform::Magento);
        let ctx = ToolContext::default();
        let missing_id = find(&tools, "shop_get_product").call(json!({}), &ctx).await;
        assert!(matches!(missing_id, Err(ToolError::InvalidArguments(_))));
        let product = find(&tools, "shop_manage_product");
        let empty_patch = product
            .call(json!({"action": "update", "id": 5}), &ctx)
            .await;
        assert_eq!(
            empty_patch,
            Err(ToolError::InvalidArguments("no fields to change".into()))
        );
        let nameless = product
            .call(json!({"action": "create", "sku": "X"}), &ctx)
            .await;
        assert_eq!(
            nameless,
            Err(ToolError::InvalidArguments("name is required".into()))
        );
        let numeric_id_with_price = product
            .call(
                json!({"action": "update", "id": 5, "regular_price": "9.90"}),
                &ctx,
            )
            .await;
        assert!(matches!(numeric_id_with_price, Err(ToolError::Failed(_))));
        let no_lines = find(&tools, "shop_manage_order")
            .call(json!({"action": "create", "email": "a@b.c"}), &ctx)
            .await;
        assert!(matches!(no_lines, Err(ToolError::InvalidArguments(_))));
        let coupon = find(&tools, "shop_manage_coupon")
            .call(
                json!({"action": "create", "code": "SALE10", "amount": "10"}),
                &ctx,
            )
            .await;
        assert_eq!(
            coupon,
            Err(ToolError::InvalidArguments("kind is required".into()))
        );
    }
}
