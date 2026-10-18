//! Magento 2 REST connector. Authenticates with an integration access token as
//! a bearer token. Reads use `/rest/V1`; product and category writes use
//! `/rest/all/V1` so they apply to every store view instead of one view's
//! overrides.

mod catalog;
mod coupons;
mod criteria;
mod customers;
#[cfg(test)]
mod ops_tests;
mod orders;
mod types;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Days;
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};

use crate::domain::*;
use crate::transport::{HttpRequest, HttpResponse, HttpTransport, Method};
use crate::{ConnectorError, Credentials, Platform, ShopConnector};
use criteria::{Criteria, Filter};

const DEFAULT_PER_PAGE: u32 = 10;
const MAX_PER_PAGE: u32 = 100;
/// The sales report sums orders client-side; beyond this it asks for a shorter range.
const MAX_REPORT_ORDERS: u64 = 5_000;

pub struct Magento {
    base: String,
    token: String,
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for Magento {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Magento").field("base", &self.base).finish()
    }
}

/// Encodes one path segment; SKUs may contain spaces or slashes.
fn segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.trim().bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn order_id(id: &str) -> Result<&str, ConnectorError> {
    numeric(id).ok_or_else(|| {
        ConnectorError::NotFound(format!(
            "{:?} is not a Magento order id (use the numeric id, not the order number)",
            id.trim()
        ))
    })
}

fn numeric(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

fn entity_id<'a>(kind: &str, id: &'a str) -> Result<&'a str, ConnectorError> {
    numeric(id).ok_or_else(|| {
        ConnectorError::NotFound(format!("{:?} is not a Magento {kind} id", id.trim()))
    })
}

fn bad_request(message: impl Into<String>) -> ConnectorError {
    ConnectorError::Api {
        status: 400,
        message: message.into(),
    }
}

fn status_code(status: &str) -> Result<u8, ConnectorError> {
    match status.trim().to_ascii_lowercase().as_str() {
        "1" | "enabled" | "enable" | "publish" | "active" => Ok(1),
        "2" | "disabled" | "disable" | "draft" | "private" | "inactive" => Ok(2),
        other => Err(ConnectorError::Api {
            status: 400,
            message: format!("unknown product status {other:?}; use enabled or disabled"),
        }),
    }
}

fn number(d: &Decimal) -> Value {
    d.to_string()
        .parse::<f64>()
        .map(|f| json!(f))
        .unwrap_or(Value::Null)
}

impl Magento {
    pub fn new(
        base_url: &str,
        credentials: &Credentials,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            token: credentials
                .get("access_token")
                .map(|t| t.trim().to_string())
                .unwrap_or_default(),
            transport,
        }
    }

    fn url(&self, scope: &str, path: &str, params: &[(String, String)]) -> String {
        let mut url = format!("{}/rest/{scope}V1{path}", self.base);
        if !params.is_empty() {
            let mut u = reqwest::Url::parse(&url).expect("base URL was validated on connect");
            u.query_pairs_mut().extend_pairs(params);
            url = u.to_string();
        }
        url
    }

    async fn send(
        &self,
        method: Method,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, ConnectorError> {
        let mut req = HttpRequest::new(method, url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp: HttpResponse = self.transport.send(req).await?;
        if !resp.is_success() {
            let message = types::error_message(&resp.body);
            return Err(match resp.status {
                401 | 403 => ConnectorError::Auth(message),
                404 => ConnectorError::NotFound(message),
                status => ConnectorError::Api { status, message },
            });
        }
        serde_json::from_str(&resp.body).map_err(|e| {
            ConnectorError::InvalidResponse(format!(
                "{e} in {}",
                resp.body.chars().take(200).collect::<String>()
            ))
        })
    }

    async fn get(&self, path: &str, params: &[(String, String)]) -> Result<Value, ConnectorError> {
        self.send(Method::Get, self.url("", path, params), None)
            .await
    }

    /// A write under `/rest/{scope}V1`; `scope` is `""` or `"all/"`.
    async fn write(
        &self,
        method: Method,
        scope: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ConnectorError> {
        self.send(method, self.url(scope, path, &[]), body).await
    }

    /// The writable fields of a patch as Magento's product object.
    fn product_body(sku: &str, patch: &ProductPatch) -> Result<Map<String, Value>, ConnectorError> {
        let mut product = Map::new();
        product.insert("sku".into(), json!(sku));
        if let Some(v) = &patch.name {
            product.insert("name".into(), json!(v));
        }
        if let Some(v) = &patch.status {
            product.insert("status".into(), json!(status_code(v)?));
        }
        if let Some(v) = &patch.regular_price {
            product.insert("price".into(), number(v));
        }
        let mut custom = Vec::new();
        if let Some(v) = &patch.sale_price {
            custom.push(json!({"attribute_code": "special_price", "value": v.to_string()}));
        }
        if let Some(v) = &patch.description {
            custom.push(json!({"attribute_code": "description", "value": v}));
        }
        if !custom.is_empty() {
            product.insert("custom_attributes".into(), Value::Array(custom));
        }
        let mut ext = Map::new();
        if let Some(q) = patch.stock_quantity {
            ext.insert("stock_item".into(), json!({"qty": q, "is_in_stock": q > 0}));
        }
        if let Some(ids) = &patch.category_ids {
            let links = ids
                .iter()
                .map(|id| {
                    entity_id("category", id).map(|id| json!({"position": 0, "category_id": id}))
                })
                .collect::<Result<Vec<_>, _>>()?;
            ext.insert("category_links".into(), Value::Array(links));
        }
        if !ext.is_empty() {
            product.insert("extension_attributes".into(), Value::Object(ext));
        }
        Ok(product)
    }

    /// Website ids other than the admin website (0).
    async fn website_ids(&self) -> Result<Vec<u64>, ConnectorError> {
        let v = self.get("/store/websites", &[]).await?;
        Ok(v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|w| w["id"].as_u64())
            .filter(|id| *id != 0)
            .collect())
    }

    async fn list<T>(
        &self,
        path: &str,
        criteria: Criteria,
        map: impl Fn(&Value) -> Result<T, ConnectorError>,
    ) -> Result<Page<T>, ConnectorError> {
        let (page, size) = (criteria.page, criteria.page_size);
        let v = self.get(path, &criteria.params()).await?;
        let total = v["total_count"].as_u64();
        let total_pages = total.map(|t| t.div_ceil(u64::from(size)) as u32);
        // Magento answers a page past the end with the last page again.
        let past_end = total_pages.is_some_and(|tp| page > tp.max(1));
        let items = if past_end {
            Vec::new()
        } else {
            v["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(&map)
                .collect::<Result<_, _>>()?
        };
        Ok(Page {
            items,
            page,
            total_pages,
            total,
        })
    }

    fn criteria(page: Option<u32>, per_page: Option<u32>) -> Criteria {
        Criteria::page(
            page.unwrap_or(1).max(1),
            per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE),
        )
    }

    async fn order_action(&self, id: &str, action: &str) -> Result<(), ConnectorError> {
        let url = self.url("", &format!("/orders/{id}/{action}"), &[]);
        match self.send(Method::Post, url, None).await? {
            Value::Bool(false) => Err(ConnectorError::Api {
                status: 409,
                message: format!(
                    "Magento did not allow \"{action}\" for order {id} in its current state"
                ),
            }),
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl ShopConnector for Magento {
    fn platform(&self) -> Platform {
        Platform::Magento
    }

    /// Magento has no order delete in its REST API; orders are cancelled.
    fn hidden(&self) -> &'static [&'static str] {
        &["shop_manage_order:delete"]
    }

    fn api_hint(&self) -> &'static str {
        "Magento 2 REST: path relative to /rest, e.g. /V1/products (default store view) or /all/V1/products (all store views); searchCriteria goes in query; JSON bodies."
    }

    async fn create_product(&self, p: &ProductPatch) -> Result<Product, ConnectorError> {
        self.create_product_impl(p).await
    }

    async fn delete_product(&self, id: &str) -> Result<(), ConnectorError> {
        self.write(
            Method::Delete,
            "",
            &format!("/products/{}", segment(id)),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn list_categories(&self, q: &CategoryQuery) -> Result<Page<Category>, ConnectorError> {
        self.list_categories_impl(q).await
    }

    async fn create_category(&self, c: &CategoryPatch) -> Result<Category, ConnectorError> {
        self.create_category_impl(c).await
    }

    async fn update_category(
        &self,
        id: &str,
        c: &CategoryPatch,
    ) -> Result<Category, ConnectorError> {
        self.update_category_impl(id, c).await
    }

    async fn delete_category(&self, id: &str) -> Result<(), ConnectorError> {
        let id = entity_id("category", id)?;
        self.write(Method::Delete, "", &format!("/categories/{id}"), None)
            .await
            .map(|_| ())
    }

    async fn get_customer(&self, id: &str) -> Result<Customer, ConnectorError> {
        types::customer(&self.customer_raw(id).await?)
    }

    async fn create_customer(&self, c: &CustomerPatch) -> Result<Customer, ConnectorError> {
        self.create_customer_impl(c).await
    }

    async fn update_customer(
        &self,
        id: &str,
        c: &CustomerPatch,
    ) -> Result<Customer, ConnectorError> {
        self.update_customer_impl(id, c).await
    }

    async fn delete_customer(&self, id: &str) -> Result<(), ConnectorError> {
        let id = entity_id("customer", id)?;
        self.write(Method::Delete, "", &format!("/customers/{id}"), None)
            .await
            .map(|_| ())
    }

    async fn list_addresses(&self, customer_id: &str) -> Result<Vec<Address>, ConnectorError> {
        let c = self.customer_raw(customer_id).await?;
        c["addresses"]
            .as_array()
            .into_iter()
            .flatten()
            .map(types::address)
            .collect()
    }

    async fn create_address(
        &self,
        customer_id: &str,
        a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        self.create_address_impl(customer_id, a).await
    }

    async fn update_address(
        &self,
        customer_id: &str,
        id: &str,
        a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        self.update_address_impl(customer_id, id, a).await
    }

    async fn delete_address(&self, customer_id: &str, id: &str) -> Result<(), ConnectorError> {
        self.delete_address_impl(customer_id, id).await
    }

    async fn create_order(&self, o: &NewOrder) -> Result<Order, ConnectorError> {
        self.create_order_impl(o).await
    }

    /// With `notify_customer`, Magento emails the comment to the customer.
    async fn add_order_note(&self, id: &str, note: &OrderNote) -> Result<(), ConnectorError> {
        let id = order_id(id)?;
        let notify = u8::from(note.notify_customer);
        self.write(
            Method::Post,
            "",
            &format!("/orders/{id}/comments"),
            Some(json!({"statusHistory": {
                "comment": note.note,
                "is_customer_notified": notify,
                "is_visible_on_front": notify
            }})),
        )
        .await
        .map(|_| ())
    }

    async fn list_coupons(&self, q: &CouponQuery) -> Result<Page<Coupon>, ConnectorError> {
        self.list_coupons_impl(q).await
    }

    async fn create_coupon(&self, c: &CouponPatch) -> Result<Coupon, ConnectorError> {
        self.create_coupon_impl(c).await
    }

    async fn update_coupon(&self, id: &str, c: &CouponPatch) -> Result<Coupon, ConnectorError> {
        self.update_coupon_impl(id, c).await
    }

    async fn delete_coupon(&self, id: &str) -> Result<(), ConnectorError> {
        self.delete_coupon_impl(id).await
    }

    async fn list_abandoned_carts(
        &self,
        q: &CartQuery,
    ) -> Result<Page<AbandonedCart>, ConnectorError> {
        self.list_abandoned_carts_impl(q).await
    }

    async fn raw(&self, req: &RawRequest) -> Result<RawResponse, ConnectorError> {
        let method = match req.method.as_str() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "PATCH" => Method::Patch,
            "DELETE" => Method::Delete,
            other => return Err(bad_request(format!("unsupported method {other}"))),
        };
        let path = format!("/{}", req.path.trim().trim_start_matches('/'));
        let mut url = reqwest::Url::parse(&format!("{}/rest{path}", self.base))
            .map_err(|e| bad_request(format!("bad path: {e}")))?;
        if !req.query.is_empty() {
            url.query_pairs_mut().extend_pairs(&req.query);
        }
        // The response is returned whatever its status, so the model sees
        // Magento's own error message instead of a summary.
        let mut http = HttpRequest::new(method, url.to_string())
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/json");
        if let Some(b) = &req.body {
            http = http.json(b);
        }
        let resp = self.transport.send(http).await?;
        let body = serde_json::from_str(&resp.body).unwrap_or(Value::String(resp.body));
        Ok(RawResponse {
            status: resp.status,
            body,
        })
    }

    async fn verify(&self) -> Result<(), ConnectorError> {
        self.get("/products", &Criteria::page(1, 1).params())
            .await
            .map(|_| ())
    }

    async fn list_products(&self, q: &ProductQuery) -> Result<Page<Product>, ConnectorError> {
        let mut c = Self::criteria(q.page, q.per_page);
        if let Some(s) = q.search.as_deref().filter(|s| !s.trim().is_empty()) {
            c = c.and(vec![Filter::like("name", s), Filter::like("sku", s)]);
        }
        if let Some(sku) = q.sku.as_deref().filter(|s| !s.trim().is_empty()) {
            c = c.and(vec![Filter::eq("sku", sku.trim())]);
        }
        if let Some(st) = q.status.as_deref().filter(|s| !s.trim().is_empty()) {
            c = c.and(vec![Filter::eq("status", status_code(st)?.to_string())]);
        }
        self.list("/products", c, types::product).await
    }

    async fn get_product(&self, id: &str) -> Result<Product, ConnectorError> {
        types::product(&self.get(&format!("/products/{}", segment(id)), &[]).await?)
    }

    async fn update_product(
        &self,
        id: &str,
        patch: &ProductPatch,
    ) -> Result<Product, ConnectorError> {
        let sku = id.trim();
        if patch.sku.as_deref().is_some_and(|s| s.trim() != sku) {
            return Err(ConnectorError::Api {
                status: 400,
                message: "Magento identifies products by SKU; changing it is not supported".into(),
            });
        }
        let product = Self::product_body(sku, patch)?;
        let url = self.url("all/", &format!("/products/{}", segment(sku)), &[]);
        let v = self
            .send(Method::Put, url, Some(json!({"product": product})))
            .await?;
        types::product(&v)
    }

    async fn list_orders(&self, q: &OrderQuery) -> Result<Page<Order>, ConnectorError> {
        let mut c = Self::criteria(q.page, q.per_page).sort("created_at", "DESC");
        if let Some(s) = q.status.as_deref().filter(|s| !s.trim().is_empty()) {
            c = c.and(vec![Filter::eq("status", s.trim())]);
        }
        if let Some(cid) = q.customer_id.as_deref().filter(|s| !s.trim().is_empty()) {
            c = c.and(vec![Filter::eq("customer_id", cid.trim())]);
        }
        if let Some(after) = q.after {
            c = c.and(vec![Filter::cond(
                "created_at",
                "gteq",
                format!("{after} 00:00:00"),
            )]);
        }
        if let Some(before) = q.before.and_then(|d| d.checked_add_days(Days::new(1))) {
            c = c.and(vec![Filter::cond(
                "created_at",
                "lt",
                format!("{before} 00:00:00"),
            )]);
        }
        self.list("/orders", c, types::order).await
    }

    async fn get_order(&self, id: &str) -> Result<Order, ConnectorError> {
        types::order(&self.get(&format!("/orders/{}", order_id(id)?), &[]).await?)
    }

    async fn update_order_status(&self, id: &str, status: &str) -> Result<Order, ConnectorError> {
        let id = order_id(id)?;
        let target = status.trim().to_ascii_lowercase().replace('-', "_");
        match target.as_str() {
            "canceled" | "cancelled" | "cancel" => self.order_action(id, "cancel").await?,
            "holded" | "on_hold" | "hold" => self.order_action(id, "hold").await?,
            "unhold" => self.order_action(id, "unhold").await?,
            "complete" | "completed" | "closed" => {
                return Err(ConnectorError::Api {
                    status: 400,
                    message: "Magento completes orders by invoicing and shipping them, which this assistant cannot do yet; do it in the Magento admin".into(),
                })
            }
            _ => {
                if self.get_order(id).await?.status == "holded" {
                    self.order_action(id, "unhold").await?;
                }
                if self.get_order(id).await?.status != target {
                    let url = self.url("", &format!("/orders/{id}/comments"), &[]);
                    self.send(
                        Method::Post,
                        url,
                        Some(json!({"statusHistory": {
                            "comment": format!("Status set to {target} by merchantGPT"),
                            "status": target,
                            "is_customer_notified": 0,
                            "is_visible_on_front": 0
                        }})),
                    )
                    .await?;
                }
            }
        }
        self.get_order(id).await
    }

    async fn list_customers(&self, q: &CustomerQuery) -> Result<Page<Customer>, ConnectorError> {
        let mut c = Self::criteria(q.page, q.per_page);
        if let Some(s) = q.search.as_deref().filter(|s| !s.trim().is_empty()) {
            c = c.and(vec![
                Filter::like("email", s),
                Filter::like("firstname", s),
                Filter::like("lastname", s),
            ]);
        }
        if let Some(e) = q.email.as_deref().filter(|s| !s.trim().is_empty()) {
            c = c.and(vec![Filter::eq("email", e.trim())]);
        }
        self.list("/customers/search", c, types::customer).await
    }

    /// Magento's REST API has no sales report, so this sums the range's
    /// non-cancelled orders. Dates are compared with Magento's UTC timestamps.
    async fn sales_report(&self, range: &DateRange) -> Result<SalesReport, ConnectorError> {
        if range.to < range.from {
            return Err(ConnectorError::Api {
                status: 400,
                message: "`to` is before `from`".into(),
            });
        }
        let end = range
            .to
            .checked_add_days(Days::new(1))
            .ok_or_else(|| ConnectorError::Api {
                status: 400,
                message: "date out of range".into(),
            })?;
        let mut report = SalesReport {
            from: range.from,
            to: range.to,
            total_sales: Decimal::ZERO,
            net_sales: Decimal::ZERO,
            orders: 0,
            items: 0,
            currency: None,
        };
        let mut currencies = std::collections::BTreeSet::new();
        let mut page = 1;
        loop {
            let c = Criteria::page(page, MAX_PER_PAGE)
                .and(vec![Filter::cond(
                    "created_at",
                    "gteq",
                    format!("{} 00:00:00", range.from),
                )])
                .and(vec![Filter::cond(
                    "created_at",
                    "lt",
                    format!("{end} 00:00:00"),
                )])
                .and(vec![Filter::cond("status", "nin", "canceled")]);
            let mut params = c.params();
            params.push((
                "fields".into(),
                "items[grand_total,subtotal,discount_amount,subtotal_refunded,total_qty_ordered,order_currency_code],total_count".into(),
            ));
            let v = self.get("/orders", &params).await?;
            let total = v["total_count"].as_u64().unwrap_or(0);
            if total > MAX_REPORT_ORDERS {
                return Err(ConnectorError::Api {
                    status: 400,
                    message: format!(
                        "{total} orders in this range; ask for a shorter range (at most {MAX_REPORT_ORDERS} orders)"
                    ),
                });
            }
            let items = v["items"].as_array().cloned().unwrap_or_default();
            for o in &items {
                let money = |k: &str| types::decimal(&o[k]).map(Option::unwrap_or_default);
                report.total_sales += money("grand_total")?;
                report.net_sales +=
                    money("subtotal")? + money("discount_amount")? - money("subtotal_refunded")?;
                report.items += money("total_qty_ordered")?
                    .round()
                    .to_string()
                    .parse::<u64>()
                    .unwrap_or(0);
                report.orders += 1;
                if let Some(c) = o["order_currency_code"].as_str() {
                    currencies.insert(c.to_string());
                }
            }
            if items.is_empty() || u64::from(page) * u64::from(MAX_PER_PAGE) >= total {
                break;
            }
            page += 1;
        }
        if currencies.len() == 1 {
            report.currency = currencies.into_iter().next();
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

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/magento/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn magento(t: &Arc<FakeTransport>) -> Magento {
        let creds = Credentials::from([("access_token".into(), "tok123".into())]);
        Magento::new("https://shop.test/", &creds, t.clone())
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn query(req: &HttpRequest, key: &str) -> Option<String> {
        req.query_pairs()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    #[tokio::test]
    async fn products_list_by_search_and_map_status_price_and_sku() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/rest/V1/products",
            HttpResponse::new(200, fixture("products.json")),
        );
        let page = magento(&t)
            .list_products(&ProductQuery {
                search: Some("hoodie".into()),
                status: Some("enabled".into()),
                per_page: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!((page.total, page.total_pages), (Some(3), Some(2)));
        let p = &page.items[0];
        assert_eq!(p.id, "HOOD-BLU", "products are addressed by SKU");
        assert_eq!(p.status.as_deref(), Some("enabled"));
        assert_eq!(p.price, Some(dec("42")));
        assert_eq!(p.sale_price, Some(dec("35.5")));
        assert_eq!(page.items[1].status.as_deref(), Some("disabled"));

        let req = &t.requests()[0];
        assert_eq!(req.header_value("authorization"), Some("Bearer tok123"));
        assert_eq!(
            query(req, "searchCriteria[filter_groups][0][filters][1][field]").as_deref(),
            Some("sku")
        );
        assert_eq!(
            query(req, "searchCriteria[filter_groups][0][filters][0][value]").as_deref(),
            Some("%hoodie%")
        );
        assert_eq!(
            query(req, "searchCriteria[filter_groups][1][filters][0][value]").as_deref(),
            Some("1")
        );
    }

    #[tokio::test]
    async fn a_page_past_the_end_is_empty() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/rest/V1/products",
            HttpResponse::new(200, fixture("products.json")),
        );
        let page = magento(&t)
            .list_products(&ProductQuery {
                page: Some(5),
                per_page: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(page.items.is_empty());
    }

    #[tokio::test]
    async fn single_product_includes_stock() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/rest/V1/products/HOOD%20BLU%2FXL",
            HttpResponse::new(200, fixture("product.json")),
        );
        let p = magento(&t).get_product("HOOD BLU/XL").await.unwrap();
        assert_eq!(p.stock_quantity, Some(7));
        assert_eq!(p.stock_status.as_deref(), Some("instock"));
    }

    #[tokio::test]
    async fn update_product_writes_to_all_store_views() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Put,
            "/rest/all/V1/products/HOOD-BLU",
            HttpResponse::new(200, fixture("product.json")),
        );
        let m = magento(&t);
        m.update_product(
            "HOOD-BLU",
            &ProductPatch {
                regular_price: Some(dec("39.90")),
                sale_price: Some(dec("29.90")),
                stock_quantity: Some(0),
                status: Some("draft".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let body: Value = serde_json::from_str(t.requests()[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"product": {
                "sku": "HOOD-BLU",
                "status": 2,
                "price": 39.9,
                "custom_attributes": [{"attribute_code": "special_price", "value": "29.90"}],
                "extension_attributes": {"stock_item": {"qty": 0, "is_in_stock": false}}
            }})
        );
        let renamed = m
            .update_product(
                "HOOD-BLU",
                &ProductPatch {
                    sku: Some("NEW".into()),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(
            renamed,
            Err(ConnectorError::Api { status: 400, .. })
        ));
    }

    #[tokio::test]
    async fn orders_map_guests_skip_child_items_and_filter_dates() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/rest/V1/orders",
            HttpResponse::new(200, fixture("orders.json")),
        );
        let page = magento(&t)
            .list_orders(&OrderQuery {
                after: NaiveDate::from_ymd_opt(2026, 10, 1),
                before: NaiveDate::from_ymd_opt(2026, 10, 16),
                ..Default::default()
            })
            .await
            .unwrap();
        let guest = &page.items[0];
        assert_eq!(
            (guest.id.as_str(), guest.number.as_str()),
            ("41", "000000041")
        );
        assert_eq!(guest.customer_id, None);
        assert_eq!(guest.customer_name.as_deref(), Some("Ana García"));
        assert_eq!(guest.total, dec("84.5"));
        assert_eq!(
            guest.line_items.len(),
            1,
            "child line of the configurable is skipped"
        );
        assert_eq!(guest.line_items[0].quantity, 2);
        assert_eq!(page.items[1].customer_id.as_deref(), Some("12"));

        let req = &t.requests()[0];
        assert_eq!(
            query(req, "searchCriteria[filter_groups][1][filters][0][value]").as_deref(),
            Some("2026-10-17 00:00:00")
        );
        assert_eq!(
            query(req, "searchCriteria[sortOrders][0][field]").as_deref(),
            Some("created_at")
        );
    }

    #[tokio::test]
    async fn status_changes_use_the_matching_magento_action() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/rest/V1/orders/41/cancel",
            HttpResponse::new(200, "true"),
        );
        t.on(
            Method::Post,
            "/rest/V1/orders/42/hold",
            HttpResponse::new(200, "false"),
        );
        t.on(
            Method::Get,
            "/rest/V1/orders/41",
            HttpResponse::new(200, fixture("order.json")),
        );
        t.on(
            Method::Get,
            "/rest/V1/orders/43",
            HttpResponse::new(200, fixture("order.json")),
        );
        t.on(
            Method::Post,
            "/rest/V1/orders/43/comments",
            HttpResponse::new(200, "true"),
        );
        let m = magento(&t);

        m.update_order_status("41", "cancelled").await.unwrap();
        let refused = m.update_order_status("42", "on-hold").await.unwrap_err();
        assert!(matches!(refused, ConnectorError::Api { status: 409, .. }));
        let complete = m.update_order_status("41", "completed").await.unwrap_err();
        assert!(matches!(complete, ConnectorError::Api { status: 400, .. }));

        m.update_order_status("43", "pending_review").await.unwrap();
        let comment = t
            .requests()
            .into_iter()
            .find(|r| r.path() == "/rest/V1/orders/43/comments")
            .unwrap();
        let body: Value = serde_json::from_str(comment.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["statusHistory"]["status"], "pending_review");
        assert!(
            m.get_order("000000041").await.is_err(),
            "increment ids are not entity ids"
        );
    }

    #[tokio::test]
    async fn customers_search_by_name_or_email() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/rest/V1/customers/search",
            HttpResponse::new(200, fixture("customers.json")),
        );
        let page = magento(&t)
            .list_customers(&CustomerQuery {
                search: Some("jo".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.items[0].email, "jo@example.com");
        assert_eq!(
            query(
                &t.requests()[0],
                "searchCriteria[filter_groups][0][filters][2][field]"
            )
            .as_deref(),
            Some("lastname")
        );
    }

    #[tokio::test]
    async fn sales_report_sums_every_page_of_orders() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/rest/V1/orders",
            HttpResponse::new(200, fixture("report_page1.json")),
        )
        .on(
            Method::Get,
            "/rest/V1/orders",
            HttpResponse::new(200, fixture("report_page2.json")),
        );
        let r = magento(&t)
            .sales_report(&DateRange {
                from: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                to: NaiveDate::from_ymd_opt(2026, 10, 16).unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(t.requests().len(), 2);
        assert_eq!(r.orders, 101);
        assert_eq!(r.total_sales, dec("1110"));
        assert_eq!(r.net_sales, dec("995"));
        assert_eq!(r.items, 102);
        assert_eq!(r.currency.as_deref(), Some("EUR"));
        assert_eq!(
            query(&t.requests()[1], "searchCriteria[currentPage]").as_deref(),
            Some("2")
        );
        assert!(query(&t.requests()[0], "fields")
            .unwrap()
            .contains("grand_total"));
    }

    #[tokio::test]
    async fn auth_errors_carry_magento_messages() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/rest/V1/products",
            HttpResponse::new(
                401,
                r#"{"message":"The consumer isn't authorized to access %resources.","parameters":{"resources":"Magento_Catalog::products"}}"#,
            ),
        );
        assert_eq!(
            magento(&t).verify().await.unwrap_err(),
            ConnectorError::Auth(
                "The consumer isn't authorized to access Magento_Catalog::products.".into()
            )
        );
    }
}
