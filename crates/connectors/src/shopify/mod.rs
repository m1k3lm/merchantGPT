//! Shopify GraphQL Admin API connector. Authenticates with a legacy custom
//! app's Admin API access token, or with a Dev Dashboard app's client ID and
//! secret through the client credentials grant (24-hour tokens, renewed here).
//! Variants are the products: they carry Shopify's price, SKU and stock.

mod queries;
mod types;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Days;
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};

use crate::domain::*;
use crate::transport::{HttpRequest, HttpResponse, HttpTransport, Method};
use crate::{ConnectorError, Credentials, Platform, ShopConnector};
use types::{gid, quoted};

pub const API_VERSION: &str = "2026-10";
pub const ACCESS_TOKEN: &str = "access_token";
pub const CLIENT_ID: &str = "client_id";
pub const CLIENT_SECRET: &str = "client_secret";
pub const SHOP_DOMAIN: &str = "shop_domain";

const DEFAULT_PER_PAGE: u32 = 10;
const MAX_PER_PAGE: u32 = 100;
/// Orders carry line items, which multiply the query cost.
const MAX_ORDERS_PER_PAGE: u32 = 50;
const LIST_LINE_ITEMS: u32 = 10;
const ORDER_LINE_ITEMS: u32 = 100;
/// Later pages are reached by walking cursors, one request per page.
const MAX_PAGE: u32 = 20;
const MAX_REPORT_ORDERS: u64 = 5_000;
const MAX_ATTEMPTS: u32 = 3;

enum Auth {
    Token(String),
    Client {
        id: String,
        secret: String,
        cached: tokio::sync::Mutex<Option<(String, Instant)>>,
    },
}

pub struct Shopify {
    base: String,
    auth: Auth,
    transport: Arc<dyn HttpTransport>,
    retry_delay: Duration,
}

impl std::fmt::Debug for Shopify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shopify").field("base", &self.base).finish()
    }
}

/// The Admin API is served from the store's myshopify.com domain; a bare
/// store name is completed to it.
fn origin(domain_or_url: &str) -> Result<String, ConnectorError> {
    let s = domain_or_url.trim().trim_end_matches('/');
    let with_scheme = if s.contains("://") {
        s.to_string()
    } else if s.contains('.') || s.contains(':') {
        format!("https://{s}")
    } else {
        format!("https://{s}.myshopify.com")
    };
    reqwest::Url::parse(&with_scheme)
        .ok()
        .filter(|u| u.host_str().is_some())
        .map(|u| u.origin().ascii_serialization())
        .ok_or_else(|| ConnectorError::InvalidCredentials(format!("{s:?} is not a shop address")))
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                crate::google_ads::oauth::encode(k),
                crate::google_ads::oauth::encode(v)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn idempotency_key() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS random source available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn error_text(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.chars().take(300).collect();
    };
    if let Some(list) = v["errors"].as_array() {
        return types::messages(&Value::Array(list.clone())).join("; ");
    }
    ["errors", "error_description", "error"]
        .iter()
        .find_map(|k| v[*k].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| body.chars().take(300).collect())
}

fn check_user_errors(payload: &Value, key: &str) -> Result<(), ConnectorError> {
    let errors = types::messages(&payload[key]);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConnectorError::Api {
            status: 422,
            message: errors.join("; "),
        })
    }
}

fn bad_request(message: impl Into<String>) -> ConnectorError {
    ConnectorError::Api {
        status: 400,
        message: message.into(),
    }
}

fn filter(terms: Vec<String>) -> Option<String> {
    let q = terms.join(" ");
    (!q.trim().is_empty()).then_some(q)
}

fn per_page(requested: Option<u32>, max: u32) -> u32 {
    requested.unwrap_or(DEFAULT_PER_PAGE).clamp(1, max)
}

/// Shopify shows a sale as `price` below `compareAtPrice`. A sale price of 0
/// ends the sale.
fn price_changes(current: &Product, patch: &ProductPatch) -> Map<String, Value> {
    let mut out = Map::new();
    let regular = patch
        .regular_price
        .or(current.regular_price)
        .unwrap_or_default();
    match patch.sale_price {
        Some(s) if s.is_zero() || s >= regular => {
            let price = if s.is_zero() { regular } else { s };
            out.insert("price".into(), json!(price.to_string()));
            out.insert("compareAtPrice".into(), Value::Null);
        }
        Some(s) => {
            out.insert("price".into(), json!(s.to_string()));
            out.insert("compareAtPrice".into(), json!(regular.to_string()));
        }
        None if patch.regular_price.is_some() => {
            if current.sale_price.is_some() {
                out.insert("compareAtPrice".into(), json!(regular.to_string()));
            } else {
                out.insert("price".into(), json!(regular.to_string()));
            }
        }
        None => {}
    }
    out
}

impl Shopify {
    pub fn new(
        shop_url: &str,
        credentials: &Credentials,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, ConnectorError> {
        let get = |k: &str| {
            credentials
                .get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let auth = match (get(ACCESS_TOKEN), get(CLIENT_ID), get(CLIENT_SECRET)) {
            (Some(token), _, _) => Auth::Token(token),
            (None, Some(id), Some(secret)) => Auth::Client {
                id,
                secret,
                cached: tokio::sync::Mutex::new(None),
            },
            _ => {
                return Err(ConnectorError::InvalidCredentials(
                    "enter an Admin API access token, or the app's client ID and client secret"
                        .into(),
                ))
            }
        };
        Ok(Self {
            base: origin(get(SHOP_DOMAIN).as_deref().unwrap_or(shop_url))?,
            auth,
            transport,
            retry_delay: Duration::from_secs(1),
        })
    }

    #[cfg(test)]
    fn without_delays(mut self) -> Self {
        self.retry_delay = Duration::ZERO;
        self
    }

    async fn token(&self, renew: bool) -> Result<String, ConnectorError> {
        let (id, secret, cached) = match &self.auth {
            Auth::Token(t) => return Ok(t.clone()),
            Auth::Client { id, secret, cached } => (id, secret, cached),
        };
        let mut cached = cached.lock().await;
        if let Some((token, expires)) = cached.as_ref() {
            if !renew && Instant::now() + Duration::from_secs(60) < *expires {
                return Ok(token.clone());
            }
        }
        let mut req = HttpRequest::new(
            Method::Post,
            format!("{}/admin/oauth/access_token", self.base),
        )
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json");
        req.body = Some(form(&[
            ("grant_type", "client_credentials"),
            ("client_id", id),
            ("client_secret", secret),
        ]));
        let resp = self.transport.send(req).await?;
        let v: Value = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
        let token = v["access_token"].as_str().filter(|_| resp.is_success());
        let Some(token) = token else {
            return Err(ConnectorError::Auth(format!(
                "Shopify refused the app's client ID and secret ({}). They only work for a Dev Dashboard app of the same organization as the store, installed on it.",
                error_text(&resp.body)
            )));
        };
        let ttl = v["expires_in"].as_u64().unwrap_or(3600);
        *cached = Some((token.to_string(), Instant::now() + Duration::from_secs(ttl)));
        Ok(token.to_string())
    }

    async fn pause(&self, attempt: u32) {
        tokio::time::sleep(self.retry_delay * attempt).await;
    }

    /// Runs a query or mutation and returns its `data`. Throttled requests are
    /// never executed by Shopify, so they are retried even for mutations;
    /// transport failures and 5xx only for queries.
    async fn graphql(&self, document: &str, variables: Value) -> Result<Value, ConnectorError> {
        let mutation = document.trim_start().starts_with("mutation");
        let body = json!({"query": document, "variables": variables});
        let url = format!("{}/admin/api/{API_VERSION}/graphql.json", self.base);
        let mut renew = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let retry = attempt < MAX_ATTEMPTS;
            let req = HttpRequest::new(Method::Post, &url)
                .header("X-Shopify-Access-Token", self.token(renew).await?)
                .header("Accept", "application/json")
                .json(&body);
            let resp: HttpResponse = match self.transport.send(req).await {
                Err(ConnectorError::Transport(_)) if retry && !mutation => {
                    self.pause(attempt).await;
                    continue;
                }
                r => r?,
            };
            match resp.status {
                200..=299 => {}
                401 if matches!(self.auth, Auth::Client { .. }) && !renew => {
                    renew = true;
                    continue;
                }
                401 | 403 => return Err(ConnectorError::Auth(error_text(&resp.body))),
                404 => {
                    return Err(ConnectorError::NotFound(format!(
                        "no Shopify Admin API at {}; if the shop uses its own domain, enter its myshopify.com domain",
                        self.base
                    )))
                }
                429 if retry => {
                    self.pause(attempt).await;
                    continue;
                }
                s if s >= 500 && retry && !mutation => {
                    self.pause(attempt).await;
                    continue;
                }
                status => {
                    return Err(ConnectorError::Api {
                        status,
                        message: error_text(&resp.body),
                    })
                }
            }
            let v: Value = serde_json::from_str(&resp.body).map_err(|_| {
                ConnectorError::InvalidResponse(format!(
                    "{} did not answer like the Shopify Admin API; if the shop uses its own domain, enter its myshopify.com domain",
                    self.base
                ))
            })?;
            let errors = v["errors"].as_array().cloned().unwrap_or_default();
            if errors.is_empty() {
                return Ok(v["data"].clone());
            }
            let code = |c: &str| errors.iter().any(|e| e["extensions"]["code"] == c);
            if code("THROTTLED") && retry {
                self.pause(attempt).await;
                continue;
            }
            let message = types::messages(&Value::Array(errors.clone())).join("; ");
            return Err(if code("ACCESS_DENIED") {
                ConnectorError::Auth(message)
            } else {
                bad_request(message)
            });
        }
    }

    /// Fetches page `page` of a connection, walking cursors to reach it.
    /// `total` comes from a count query when the filter allows one.
    #[allow(clippy::too_many_arguments)]
    async fn page<T>(
        &self,
        document: &str,
        connection: &str,
        query: Option<String>,
        extra: Value,
        page: Option<u32>,
        per_page: u32,
        total: Option<u64>,
        map: impl Fn(&Value) -> Result<T, ConnectorError>,
    ) -> Result<Page<T>, ConnectorError> {
        let page = page.unwrap_or(1).max(1);
        if page > MAX_PAGE {
            return Err(bad_request(format!(
                "page {page} is too deep for Shopify's cursor pagination (at most {MAX_PAGE}); use more per page or a narrower search"
            )));
        }
        let total_pages = total.map(|t| t.div_ceil(u64::from(per_page)) as u32);
        let empty = |walked: u32| Page {
            items: Vec::new(),
            page,
            total_pages: total_pages.or(Some(walked)),
            total,
        };
        let mut after: Option<String> = None;
        let walk = queries::cursor(connection);
        for walked in 1..page {
            let vars = json!({"first": per_page, "after": after, "query": query});
            let info = self.graphql(&walk, vars).await?[connection]["pageInfo"].clone();
            if info["hasNextPage"] != true {
                return Ok(empty(walked));
            }
            after = info["endCursor"].as_str().map(str::to_string);
        }
        let mut vars = json!({"first": per_page, "after": after, "query": query});
        if let (Value::Object(v), Value::Object(e)) = (&mut vars, extra) {
            v.extend(e);
        }
        let data = self.graphql(document, vars).await?;
        let items: Vec<T> = data[connection]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(map)
            .collect::<Result<_, _>>()?;
        let last = data[connection]["pageInfo"]["hasNextPage"] != true;
        let seen = u64::from(page - 1) * u64::from(per_page) + items.len() as u64;
        Ok(Page {
            items,
            page,
            total_pages: total_pages.or(last.then_some(page)),
            total: total.or(last.then_some(seen)),
        })
    }

    async fn count(&self, document: &str, field: &str, query: Option<&str>) -> Option<u64> {
        let data = self.graphql(document, json!({"query": query})).await.ok()?;
        data[field]["count"].as_u64()
    }

    async fn variant(&self, id: &str) -> Result<(Product, Value), ConnectorError> {
        let id = gid("ProductVariant", id)?;
        let data = self.graphql(&queries::variant(), json!({"id": id})).await?;
        let v = &data["productVariant"];
        if v.is_null() {
            return Err(ConnectorError::NotFound(format!("product variant {id}")));
        }
        Ok((types::product(v)?, v.clone()))
    }

    async fn set_stock(&self, variant: &Value, quantity: i64) -> Result<(), ConnectorError> {
        let levels = variant["inventoryItem"]["inventoryLevels"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let location = match levels.as_slice() {
            [one] => one["location"]["id"].clone(),
            [] => return Err(bad_request(
                "this variant is not stocked at any location; add it to a location in the Shopify admin first",
            )),
            _ => return Err(bad_request(
                "this variant is stocked at several locations; set the quantity per location in the Shopify admin",
            )),
        };
        let data = self
            .graphql(
                queries::SET_STOCK,
                json!({
                    "input": {
                        "name": "available",
                        "reason": "correction",
                        "quantities": [{
                            "inventoryItemId": variant["inventoryItem"]["id"],
                            "locationId": location,
                            "quantity": quantity,
                            "changeFromQuantity": null
                        }]
                    },
                    "idempotencyKey": idempotency_key()
                }),
            )
            .await?;
        check_user_errors(&data["inventorySetQuantities"], "userErrors")
    }

    async fn order_mutation(
        &self,
        document: &str,
        field: &str,
        variables: Value,
    ) -> Result<(), ConnectorError> {
        let data = self.graphql(document, variables).await?;
        let key = if field == "orderCancel" {
            "orderCancelUserErrors"
        } else {
            "userErrors"
        };
        check_user_errors(&data[field], key)
    }
}

#[async_trait]
impl ShopConnector for Shopify {
    fn platform(&self) -> Platform {
        Platform::Shopify
    }

    async fn verify(&self) -> Result<(), ConnectorError> {
        self.graphql(queries::SHOP, json!({})).await.map(|_| ())
    }

    async fn list_products(&self, q: &ProductQuery) -> Result<Page<Product>, ConnectorError> {
        let mut terms = Vec::new();
        if let Some(s) = q.search.as_deref().filter(|s| !s.trim().is_empty()) {
            terms.push(quoted(s));
        }
        if let Some(sku) = q.sku.as_deref().filter(|s| !s.trim().is_empty()) {
            terms.push(format!("sku:{}", quoted(sku)));
        }
        if let Some(st) = q.status.as_deref().filter(|s| !s.trim().is_empty()) {
            let status = types::product_status(st)?.to_ascii_lowercase();
            terms.push(format!("product_status:{status}"));
        }
        let query = filter(terms);
        let size = per_page(q.per_page, MAX_PER_PAGE);
        // The variants count takes no search filter.
        let total = match query {
            None => {
                self.count(queries::VARIANTS_COUNT, "productVariantsCount", None)
                    .await
            }
            Some(_) => None,
        };
        self.page(
            &queries::variants(),
            "productVariants",
            query,
            json!({}),
            q.page,
            size,
            total,
            types::product,
        )
        .await
    }

    async fn get_product(&self, id: &str) -> Result<Product, ConnectorError> {
        Ok(self.variant(id).await?.0)
    }

    /// Name and status belong to the variant's product, so they change every
    /// variant of it.
    async fn update_product(
        &self,
        id: &str,
        patch: &ProductPatch,
    ) -> Result<Product, ConnectorError> {
        if patch.is_empty() {
            return Err(bad_request("nothing to change"));
        }
        let (current, raw) = self.variant(id).await?;
        let product_id = raw["product"]["id"].clone();
        if patch.name.is_some() || patch.status.is_some() {
            let mut product = Map::new();
            product.insert("id".into(), product_id.clone());
            if let Some(name) = &patch.name {
                product.insert("title".into(), json!(name));
            }
            if let Some(status) = &patch.status {
                product.insert("status".into(), json!(types::product_status(status)?));
            }
            let data = self
                .graphql(queries::PRODUCT_UPDATE, json!({"product": product}))
                .await?;
            check_user_errors(&data["productUpdate"], "userErrors")?;
        }
        let mut variant = price_changes(&current, patch);
        let mut item = Map::new();
        if let Some(sku) = &patch.sku {
            item.insert("sku".into(), json!(sku.trim()));
        }
        if patch.stock_quantity.is_some() && raw["inventoryItem"]["tracked"] != true {
            item.insert("tracked".into(), json!(true));
        }
        if !item.is_empty() {
            variant.insert("inventoryItem".into(), Value::Object(item));
        }
        if !variant.is_empty() {
            variant.insert("id".into(), raw["id"].clone());
            let data = self
                .graphql(
                    queries::VARIANTS_UPDATE,
                    json!({"productId": product_id, "variants": [variant]}),
                )
                .await?;
            check_user_errors(&data["productVariantsBulkUpdate"], "userErrors")?;
        }
        if let Some(q) = patch.stock_quantity {
            self.set_stock(&raw, q).await?;
        }
        self.get_product(&current.id).await
    }

    async fn list_orders(&self, q: &OrderQuery) -> Result<Page<Order>, ConnectorError> {
        let mut terms = Vec::new();
        if let Some(s) = q.status.as_deref().filter(|s| !s.trim().is_empty()) {
            terms.push(types::order_status_filter(s)?);
        }
        if let Some(c) = q.customer_id.as_deref().filter(|s| !s.trim().is_empty()) {
            let id = gid("Customer", c)?;
            terms.push(format!(
                "customer_id:{}",
                id.rsplit('/').next().unwrap_or_default()
            ));
        }
        if let Some(after) = q.after {
            terms.push(format!("created_at:>={after}"));
        }
        if let Some(before) = q.before.and_then(|d| d.checked_add_days(Days::new(1))) {
            terms.push(format!("created_at:<{before}"));
        }
        let query = filter(terms);
        let total = self
            .count(queries::ORDERS_COUNT, "ordersCount", query.as_deref())
            .await;
        self.page(
            &queries::orders(),
            "orders",
            query,
            json!({"lines": LIST_LINE_ITEMS}),
            q.page,
            per_page(q.per_page, MAX_ORDERS_PER_PAGE),
            total,
            types::order,
        )
        .await
    }

    /// Accepts the order id or its name as merchants see it, e.g. `#1001`.
    async fn get_order(&self, id: &str) -> Result<Order, ConnectorError> {
        let id = id.trim();
        if id.starts_with('#') {
            let vars = json!({"first": 1, "query": format!("name:{}", quoted(id)), "lines": ORDER_LINE_ITEMS});
            let data = self.graphql(&queries::orders(), vars).await?;
            return match data["orders"]["nodes"].get(0) {
                Some(o) => types::order(o),
                None => Err(ConnectorError::NotFound(format!("order {id}"))),
            };
        }
        let gid = gid("Order", id)?;
        let data = self
            .graphql(
                &queries::order(),
                json!({"id": gid, "lines": ORDER_LINE_ITEMS}),
            )
            .await?;
        if data["order"].is_null() {
            return Err(ConnectorError::NotFound(format!(
                "order {id}; use the id from shop_list_orders or the order name, e.g. #1001"
            )));
        }
        types::order(&data["order"])
    }

    async fn update_order_status(&self, id: &str, status: &str) -> Result<Order, ConnectorError> {
        let target = status.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        let (document, field) = match target.as_str() {
            "cancelled" | "canceled" | "cancel" => (queries::ORDER_CANCEL, "orderCancel"),
            "closed" | "close" | "archived" | "archive" => (queries::ORDER_CLOSE, "orderClose"),
            "open" | "reopen" | "reopened" | "unarchive" | "unarchived" => {
                (queries::ORDER_OPEN, "orderOpen")
            }
            "paid" | "mark_as_paid" | "mark_paid" => (queries::ORDER_MARK_PAID, "orderMarkAsPaid"),
            "completed" | "complete" | "fulfilled" | "shipped" => {
                return Err(bad_request(
                    "Shopify completes orders by fulfilling them (shipping the items), which this assistant cannot do yet; fulfil the order in the Shopify admin",
                ))
            }
            other => {
                return Err(bad_request(format!(
                    "Shopify orders can be cancelled, closed (archived), reopened or marked as paid, not set to {other:?}"
                )))
            }
        };
        let order = self.get_order(id).await?;
        if field != "orderCancel" {
            self.order_mutation(document, field, json!({"input": {"id": order.id}}))
                .await?;
            return self.get_order(&order.id).await;
        }
        // No refund: authorized payments are voided, captured ones stay for
        // the merchant to refund in the Shopify admin.
        let vars = json!({
            "orderId": order.id,
            "reason": "OTHER",
            "restock": true,
            "staffNote": "Cancelled from merchantGPT"
        });
        self.order_mutation(document, field, vars).await?;
        // Cancellation runs as a background job on Shopify's side.
        for attempt in 1..MAX_ATTEMPTS {
            let now = self.get_order(&order.id).await?;
            if now.status == "cancelled" {
                return Ok(now);
            }
            self.pause(attempt).await;
        }
        self.get_order(&order.id).await
    }

    async fn list_customers(&self, q: &CustomerQuery) -> Result<Page<Customer>, ConnectorError> {
        let mut terms = Vec::new();
        if let Some(s) = q.search.as_deref().filter(|s| !s.trim().is_empty()) {
            terms.push(quoted(s));
        }
        if let Some(e) = q.email.as_deref().filter(|s| !s.trim().is_empty()) {
            terms.push(format!("email:{}", quoted(e)));
        }
        let query = filter(terms);
        // The customers count only filters by dates and ids.
        let total = match query {
            None => {
                self.count(queries::CUSTOMERS_COUNT, "customersCount", None)
                    .await
            }
            Some(_) => None,
        };
        self.page(
            queries::CUSTOMERS,
            "customers",
            query,
            json!({}),
            q.page,
            per_page(q.per_page, MAX_PER_PAGE),
            total,
            types::customer,
        )
        .await
    }

    /// Sums the range's orders, skipping cancelled ones. Net sales are the
    /// current subtotals: after discounts, returns and edits, before shipping.
    async fn sales_report(&self, range: &DateRange) -> Result<SalesReport, ConnectorError> {
        if range.to < range.from {
            return Err(bad_request("`to` is before `from`"));
        }
        let end = range
            .to
            .checked_add_days(Days::new(1))
            .ok_or_else(|| bad_request("date out of range"))?;
        let query = format!("created_at:>={} created_at:<{end}", range.from);
        let count = self
            .graphql(queries::ORDERS_COUNT, json!({"query": query}))
            .await?["ordersCount"]["count"]
            .as_u64()
            .unwrap_or(0);
        if count > MAX_REPORT_ORDERS {
            return Err(bad_request(format!(
                "{count} orders in this range; ask for a shorter range (at most {MAX_REPORT_ORDERS} orders)"
            )));
        }
        let mut report = SalesReport {
            from: range.from,
            to: range.to,
            total_sales: Decimal::ZERO,
            net_sales: Decimal::ZERO,
            orders: 0,
            items: 0,
            currency: None,
        };
        let mut after: Option<String> = None;
        loop {
            let data = self
                .graphql(queries::REPORT, json!({"after": after, "query": query}))
                .await?;
            let orders = &data["orders"];
            for o in orders["nodes"].as_array().into_iter().flatten() {
                if !o["cancelledAt"].is_null() {
                    continue;
                }
                let money = &o["totalPriceSet"]["shopMoney"];
                report.total_sales += types::decimal(&money["amount"])?.unwrap_or_default();
                report.net_sales +=
                    types::decimal(&o["currentSubtotalPriceSet"]["shopMoney"]["amount"])?
                        .unwrap_or_default();
                report.items += o["currentSubtotalLineItemsQuantity"].as_u64().unwrap_or(0);
                report.orders += 1;
                if report.currency.is_none() {
                    report.currency = money["currencyCode"].as_str().map(str::to_string);
                }
            }
            if orders["pageInfo"]["hasNextPage"] != true {
                break;
            }
            after = orders["pageInfo"]["endCursor"].as_str().map(str::to_string);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FakeTransport;
    use chrono::NaiveDate;
    use std::str::FromStr;

    const GQL: &str = "/admin/api/2026-10/graphql.json";

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn token_shop(t: &Arc<FakeTransport>) -> Shopify {
        let creds = Credentials::from([(ACCESS_TOKEN.into(), "shpat_x".into())]);
        Shopify::new("https://acme.myshopify.com/", &creds, t.clone())
            .unwrap()
            .without_delays()
    }

    fn data(t: &FakeTransport, data: Value) {
        t.on(
            Method::Post,
            GQL,
            HttpResponse::new(200, json!({ "data": data }).to_string()),
        );
    }

    /// The GraphQL bodies sent, in order.
    fn sent(t: &FakeTransport) -> Vec<Value> {
        t.requests()
            .iter()
            .filter(|r| r.path() == GQL)
            .map(|r| serde_json::from_str(r.body.as_deref().unwrap()).unwrap())
            .collect()
    }

    fn variant(id: u64, price: &str, compare_at: Value, levels: usize) -> Value {
        json!({
            "id": format!("gid://shopify/ProductVariant/{id}"), "title": "Blue", "sku": "HOOD-BLU",
            "price": price, "compareAtPrice": compare_at, "inventoryQuantity": 3,
            "inventoryItem": {
                "id": "gid://shopify/InventoryItem/70", "tracked": true,
                "inventoryLevels": {"nodes": (0..levels)
                    .map(|n| json!({"location": {"id": format!("gid://shopify/Location/{n}")}}))
                    .collect::<Vec<_>>()}
            },
            "product": {"id": "gid://shopify/Product/9", "title": "Hoodie", "status": "ACTIVE"}
        })
    }

    fn order(id: u64, name: &str) -> Value {
        json!({
            "id": format!("gid://shopify/Order/{id}"), "name": name,
            "createdAt": "2026-10-10T09:00:00Z", "cancelledAt": null, "closed": false,
            "displayFinancialStatus": "PAID", "displayFulfillmentStatus": "UNFULFILLED",
            "totalPriceSet": {"shopMoney": {"amount": "84.50", "currencyCode": "EUR"}},
            "email": "ana@example.com", "customer": null,
            "billingAddress": {"firstName": "Ana", "lastName": "García"},
            "lineItems": {"nodes": [{"name": "Hoodie - Blue", "currentQuantity": 2,
                "discountedTotalSet": {"shopMoney": {"amount": "79.80"}},
                "variant": {"id": "gid://shopify/ProductVariant/11"}}]}
        })
    }

    #[test]
    fn credentials_need_a_token_or_a_client_pair() {
        let t: Arc<dyn HttpTransport> = Arc::new(FakeTransport::new());
        let only_id = Credentials::from([(CLIENT_ID.into(), "id".into())]);
        assert!(matches!(
            Shopify::new("https://acme.myshopify.com", &only_id, t.clone()),
            Err(ConnectorError::InvalidCredentials(_))
        ));
        let creds = Credentials::from([
            (CLIENT_ID.into(), "id".into()),
            (CLIENT_SECRET.into(), "secret".into()),
            (SHOP_DOMAIN.into(), "acme".into()),
        ]);
        let s = Shopify::new("https://www.acme.com/shop", &creds, t.clone()).unwrap();
        assert_eq!(s.base, "https://acme.myshopify.com");
        let token = Credentials::from([(ACCESS_TOKEN.into(), "shpat_x".into())]);
        let s = Shopify::new("acme.myshopify.com/admin", &token, t).unwrap();
        assert_eq!(s.base, "https://acme.myshopify.com");
    }

    #[tokio::test]
    async fn client_credentials_token_is_cached_and_renewed_after_a_401() {
        let t = Arc::new(FakeTransport::new());
        let token = |tok: &str| {
            HttpResponse::new(
                200,
                json!({"access_token": tok, "scope": "read_products", "expires_in": 86399})
                    .to_string(),
            )
        };
        t.on(Method::Post, "/admin/oauth/access_token", token("t1"))
            .on(Method::Post, "/admin/oauth/access_token", token("t2"));
        data(&t, json!({"shop": {"name": "Acme"}}));
        t.on(
            Method::Post,
            GQL,
            HttpResponse::new(401, r#"{"errors":"[API] Invalid API key or access token"}"#),
        );
        data(&t, json!({"shop": {"name": "Acme"}}));
        let creds = Credentials::from([
            (CLIENT_ID.into(), "cid".into()),
            (CLIENT_SECRET.into(), "s&cret".into()),
        ]);
        let s = Shopify::new("https://acme.myshopify.com", &creds, t.clone()).unwrap();
        s.verify().await.unwrap();
        s.verify().await.unwrap();

        let reqs = t.requests();
        let token_reqs: Vec<_> = reqs
            .iter()
            .filter(|r| r.path() == "/admin/oauth/access_token")
            .collect();
        assert_eq!(token_reqs.len(), 2);
        assert_eq!(
            token_reqs[0].body.as_deref(),
            Some("grant_type=client_credentials&client_id=cid&client_secret=s%26cret")
        );
        let used: Vec<_> = reqs
            .iter()
            .filter(|r| r.path() == GQL)
            .map(|r| r.header_value("x-shopify-access-token").unwrap())
            .collect();
        assert_eq!(used, ["t1", "t1", "t2"]);
    }

    #[tokio::test]
    async fn refused_client_credentials_explain_the_organization_rule() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/admin/oauth/access_token",
            HttpResponse::new(400, r#"{"error":"shop_not_permitted","error_description":"Client credentials cannot be performed on this shop."}"#),
        );
        let creds = Credentials::from([
            (CLIENT_ID.into(), "cid".into()),
            (CLIENT_SECRET.into(), "sec".into()),
        ]);
        let s = Shopify::new("https://acme.myshopify.com", &creds, t).unwrap();
        let ConnectorError::Auth(msg) = s.verify().await.unwrap_err() else {
            panic!()
        };
        assert!(msg.contains("cannot be performed on this shop"), "{msg}");
        assert!(msg.contains("same organization"), "{msg}");
    }

    #[tokio::test]
    async fn later_product_pages_walk_cursors_and_filters_skip_the_count() {
        let t = Arc::new(FakeTransport::new());
        data(
            &t,
            json!({"productVariants": {"pageInfo": {"hasNextPage": true, "endCursor": "c1"}}}),
        );
        data(
            &t,
            json!({"productVariants": {
                "nodes": [variant(11, "29.90", json!("39.90"), 1)],
                "pageInfo": {"hasNextPage": false, "endCursor": "c2"}
            }}),
        );
        let page = token_shop(&t)
            .list_products(&ProductQuery {
                search: Some("blue hoodie".into()),
                status: Some("publish".into()),
                page: Some(2),
                per_page: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            (page.page, page.total_pages, page.total),
            (2, Some(2), Some(6))
        );
        assert_eq!(page.items[0].id, "gid://shopify/ProductVariant/11");
        assert_eq!(page.items[0].sale_price, Some(dec("29.9")));

        let bodies = sent(&t);
        assert_eq!(bodies.len(), 2, "no count query with a search filter");
        assert_eq!(
            bodies[0]["variables"]["query"],
            "\"blue hoodie\" product_status:active"
        );
        assert_eq!(bodies[1]["variables"]["after"], "c1");
        assert_eq!(bodies[1]["variables"]["first"], 5);
        assert_eq!(
            t.requests()[0].header_value("x-shopify-access-token"),
            Some("shpat_x")
        );
    }

    #[tokio::test]
    async fn unfiltered_lists_report_totals_from_the_count() {
        let t = Arc::new(FakeTransport::new());
        data(&t, json!({"productVariantsCount": {"count": 23}}));
        data(
            &t,
            json!({"productVariants": {
                "nodes": [variant(11, "10.00", Value::Null, 1)],
                "pageInfo": {"hasNextPage": true, "endCursor": "c1"}
            }}),
        );
        let page = token_shop(&t)
            .list_products(&ProductQuery::default())
            .await
            .unwrap();
        assert_eq!((page.total, page.total_pages), (Some(23), Some(3)));
    }

    #[tokio::test]
    async fn update_product_touches_product_variant_and_stock() {
        let t = Arc::new(FakeTransport::new());
        data(
            &t,
            json!({"productVariant": variant(11, "39.90", Value::Null, 1)}),
        );
        data(&t, json!({"productUpdate": {"userErrors": []}}));
        data(&t, json!({"productVariantsBulkUpdate": {"userErrors": []}}));
        data(&t, json!({"inventorySetQuantities": {"userErrors": []}}));
        data(
            &t,
            json!({"productVariant": variant(11, "29.90", json!("39.90"), 1)}),
        );
        let p = token_shop(&t)
            .update_product(
                "11",
                &ProductPatch {
                    status: Some("draft".into()),
                    sale_price: Some(dec("29.90")),
                    sku: Some("HOOD-BLUE".into()),
                    stock_quantity: Some(8),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(p.sale_price, Some(dec("29.9")));

        let b = sent(&t);
        assert_eq!(
            b[1]["variables"]["product"],
            json!({"id": "gid://shopify/Product/9", "status": "DRAFT"})
        );
        assert_eq!(
            b[2]["variables"]["variants"],
            json!([{
                "id": "gid://shopify/ProductVariant/11", "price": "29.90",
                "compareAtPrice": "39.9", "inventoryItem": {"sku": "HOOD-BLUE"}
            }])
        );
        let stock = &b[3]["variables"];
        assert!(b[3]["query"]
            .as_str()
            .unwrap()
            .contains("@idempotent(key: $idempotencyKey)"));
        assert_eq!(stock["idempotencyKey"].as_str().unwrap().len(), 32);
        assert_eq!(
            stock["input"]["quantities"],
            json!([{"inventoryItemId": "gid://shopify/InventoryItem/70",
                "locationId": "gid://shopify/Location/0", "quantity": 8, "changeFromQuantity": null}])
        );
    }

    #[test]
    fn prices_follow_shopifys_compare_at_model() {
        let product = |price: &str, sale: Option<&str>| Product {
            id: "v".into(),
            name: "n".into(),
            sku: None,
            status: None,
            price: Some(dec(sale.unwrap_or(price))),
            regular_price: Some(dec(price)),
            sale_price: sale.map(dec),
            stock_quantity: None,
            stock_status: None,
        };
        let patch = |regular: Option<&str>, sale: Option<&str>| ProductPatch {
            regular_price: regular.map(dec),
            sale_price: sale.map(dec),
            ..Default::default()
        };
        let on_sale = product("40", Some("30"));
        assert_eq!(
            Value::Object(price_changes(&on_sale, &patch(Some("45"), None))),
            json!({"compareAtPrice": "45"}),
            "a new regular price keeps the sale"
        );
        assert_eq!(
            Value::Object(price_changes(&on_sale, &patch(None, Some("0")))),
            json!({"price": "40", "compareAtPrice": null}),
            "a zero sale price ends the sale"
        );
        assert_eq!(
            Value::Object(price_changes(
                &product("40", None),
                &patch(Some("42"), None)
            )),
            json!({"price": "42"})
        );
    }

    #[tokio::test]
    async fn stock_at_several_locations_is_refused() {
        let t = Arc::new(FakeTransport::new());
        data(
            &t,
            json!({"productVariant": variant(11, "10.00", Value::Null, 2)}),
        );
        let err = token_shop(&t)
            .update_product(
                "gid://shopify/ProductVariant/11",
                &ProductPatch {
                    stock_quantity: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ConnectorError::Api { status: 400, .. }),
            "{err}"
        );
        assert_eq!(sent(&t).len(), 1, "nothing was written");
    }

    #[tokio::test]
    async fn orders_filter_by_status_dates_and_customer() {
        let t = Arc::new(FakeTransport::new());
        data(&t, json!({"ordersCount": {"count": 1}}));
        data(
            &t,
            json!({"orders": {"nodes": [order(5, "#1001")],
            "pageInfo": {"hasNextPage": false, "endCursor": "c"}}}),
        );
        let page = token_shop(&t)
            .list_orders(&OrderQuery {
                status: Some("processing".into()),
                customer_id: Some("gid://shopify/Customer/77".into()),
                after: NaiveDate::from_ymd_opt(2026, 10, 1),
                before: NaiveDate::from_ymd_opt(2026, 10, 16),
                ..Default::default()
            })
            .await
            .unwrap();
        let o = &page.items[0];
        assert_eq!(
            (o.number.as_str(), o.status.as_str()),
            ("#1001", "paid, unfulfilled")
        );
        assert_eq!(o.customer_name.as_deref(), Some("Ana García"));
        assert_eq!(o.total, dec("84.5"));
        assert_eq!(o.line_items[0].quantity, 2);

        let b = sent(&t);
        let q = "status:open fulfillment_status:unfulfilled customer_id:77 created_at:>=2026-10-01 created_at:<2026-10-17";
        assert_eq!(b[0]["variables"]["query"], q);
        assert_eq!(b[1]["variables"]["query"], q);
        assert_eq!(b[1]["variables"]["lines"], LIST_LINE_ITEMS);
    }

    #[tokio::test]
    async fn orders_are_found_by_name() {
        let t = Arc::new(FakeTransport::new());
        data(
            &t,
            json!({"orders": {"nodes": [order(5, "#1001")], "pageInfo": {}}}),
        );
        let o = token_shop(&t).get_order("#1001").await.unwrap();
        assert_eq!(o.id, "gid://shopify/Order/5");
        assert_eq!(sent(&t)[0]["variables"]["query"], "name:\"#1001\"");
    }

    #[tokio::test]
    async fn cancelling_restocks_without_refunding_and_waits_for_the_job() {
        let t = Arc::new(FakeTransport::new());
        let mut cancelled = order(5, "#1001");
        cancelled["cancelledAt"] = json!("2026-10-17T10:00:00Z");
        data(&t, json!({"order": order(5, "#1001")}));
        data(&t, json!({"orderCancel": {"orderCancelUserErrors": []}}));
        data(&t, json!({"order": order(5, "#1001")}));
        data(&t, json!({"order": cancelled}));
        let o = token_shop(&t)
            .update_order_status("5", "Cancelled")
            .await
            .unwrap();
        assert_eq!(o.status, "cancelled");
        let b = sent(&t);
        assert_eq!(b[1]["variables"]["restock"], true);
        assert!(b[1]["variables"].get("refundMethod").is_none());
    }

    #[tokio::test]
    async fn fulfilment_is_refused_and_user_errors_are_reported() {
        let t = Arc::new(FakeTransport::new());
        data(&t, json!({"order": order(5, "#1001")}));
        data(
            &t,
            json!({"orderClose": {"userErrors": [{"field": ["id"], "message": "Order is already closed"}]}}),
        );
        let s = token_shop(&t);
        assert!(matches!(
            s.update_order_status("5", "completed").await,
            Err(ConnectorError::Api { status: 400, .. })
        ));
        assert_eq!(
            s.update_order_status("5", "archived").await.unwrap_err(),
            ConnectorError::Api {
                status: 422,
                message: "id: Order is already closed".into()
            }
        );
    }

    #[tokio::test]
    async fn customers_map_email_orders_and_spend() {
        let t = Arc::new(FakeTransport::new());
        data(
            &t,
            json!({"customers": {"nodes": [{
            "id": "gid://shopify/Customer/77", "firstName": "Jo", "lastName": "Doe",
            "numberOfOrders": "4", "defaultEmailAddress": {"emailAddress": "jo@example.com"},
            "amountSpent": {"amount": "120.50"}
        }], "pageInfo": {"hasNextPage": false}}}),
        );
        let page = token_shop(&t)
            .list_customers(&CustomerQuery {
                email: Some("jo@example.com".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let c = &page.items[0];
        assert_eq!(c.email, "jo@example.com");
        assert_eq!(
            (c.orders_count, c.total_spent),
            (Some(4), Some(dec("120.5")))
        );
        assert_eq!(
            sent(&t)[0]["variables"]["query"],
            "email:\"jo@example.com\""
        );
    }

    #[tokio::test]
    async fn sales_report_sums_pages_and_skips_cancelled_orders() {
        let t = Arc::new(FakeTransport::new());
        let node = |total: &str, net: &str, qty: u64, cancelled: bool| {
            json!({
                "cancelledAt": if cancelled { json!("2026-10-02T00:00:00Z") } else { Value::Null },
                "currentSubtotalLineItemsQuantity": qty,
                "totalPriceSet": {"shopMoney": {"amount": total, "currencyCode": "EUR"}},
                "currentSubtotalPriceSet": {"shopMoney": {"amount": net}}
            })
        };
        data(&t, json!({"ordersCount": {"count": 3}}));
        data(
            &t,
            json!({"orders": {"nodes": [node("100.00", "80.00", 2, false), node("50.00", "40.00", 1, true)],
            "pageInfo": {"hasNextPage": true, "endCursor": "c1"}}}),
        );
        data(
            &t,
            json!({"orders": {"nodes": [node("10.50", "9.00", 1, false)],
            "pageInfo": {"hasNextPage": false}}}),
        );
        let r = token_shop(&t)
            .sales_report(&DateRange {
                from: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                to: NaiveDate::from_ymd_opt(2026, 10, 16).unwrap(),
            })
            .await
            .unwrap();
        assert_eq!((r.orders, r.items), (2, 3));
        assert_eq!((r.total_sales, r.net_sales), (dec("110.5"), dec("89")));
        assert_eq!(r.currency.as_deref(), Some("EUR"));
        let b = sent(&t);
        assert_eq!(
            b[1]["variables"]["query"],
            "created_at:>=2026-10-01 created_at:<2026-10-17"
        );
        assert_eq!(b[2]["variables"]["after"], "c1");
    }

    #[tokio::test]
    async fn throttled_requests_are_retried_and_access_denied_is_an_auth_error() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            GQL,
            HttpResponse::new(
                200,
                r#"{"errors":[{"message":"Throttled","extensions":{"code":"THROTTLED"}}]}"#,
            ),
        );
        data(&t, json!({"shop": {"name": "Acme"}}));
        t.on(
            Method::Post,
            GQL,
            HttpResponse::new(200, r#"{"errors":[{"message":"Access denied for orders field. Required access: `read_orders` access scope.","extensions":{"code":"ACCESS_DENIED"}}]}"#),
        );
        let s = token_shop(&t);
        s.verify().await.unwrap();
        let ConnectorError::Auth(msg) = s.verify().await.unwrap_err() else {
            panic!()
        };
        assert!(msg.contains("read_orders"));
    }
}
