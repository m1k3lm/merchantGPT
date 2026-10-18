//! PrestaShop webservice connector (`{shop}/api`). Authenticates with a
//! webservice key as the Basic auth user. Reads ask for JSON; writes must be
//! XML and use PATCH, so updates need PrestaShop 8 or later.

mod types;
mod xml;

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use base64::prelude::*;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde_json::Value;

use crate::domain::*;
use crate::transport::{HttpRequest, HttpTransport, Method};
use crate::{ConnectorError, Credentials, Platform, ShopConnector};
use types::OrderState;
use xml::Field;

pub const WEBSERVICE_KEY: &str = "webservice_key";

const DEFAULT_PER_PAGE: u32 = 10;
const MAX_PER_PAGE: u32 = 100;
/// Orders are fetched with their rows.
const MAX_ORDERS_PER_PAGE: u32 = 50;
const REPORT_PAGE: u32 = 100;
const MAX_REPORT_ORDERS: u64 = 5_000;
const PRODUCT_FIELDS: &str = "[id,name,reference,price,final,active,cache_default_attribute]";

type Params = Vec<(String, String)>;

/// Shop data that rarely changes, fetched once per connector.
struct Meta {
    states: Vec<OrderState>,
    currencies: HashMap<String, String>,
}

pub struct PrestaShop {
    base: String,
    key: String,
    transport: Arc<dyn HttpTransport>,
    /// Set when the web server strips the Authorization header; the key then
    /// goes in the `ws_key` query parameter instead.
    key_in_query: AtomicBool,
    meta: tokio::sync::Mutex<Option<Arc<Meta>>>,
}

impl std::fmt::Debug for PrestaShop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrestaShop")
            .field("base", &self.base)
            .finish()
    }
}

fn bad_request(message: impl Into<String>) -> ConnectorError {
    ConnectorError::Api {
        status: 400,
        message: message.into(),
    }
}

/// Strips the characters the webservice filter syntax gives a meaning to.
fn literal(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | '|' | '%' | ','))
        .collect()
}

fn numeric_id<'a>(kind: &str, id: &'a str) -> Result<&'a str, ConnectorError> {
    let id = id.trim();
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        Ok(id)
    } else {
        Err(ConnectorError::NotFound(format!(
            "{id:?} is not a PrestaShop {kind} id"
        )))
    }
}

fn one_of(ids: impl IntoIterator<Item = String>) -> String {
    format!("[{}]", ids.into_iter().collect::<Vec<_>>().join("|"))
}

fn date_range(from: Option<NaiveDate>, to: Option<NaiveDate>) -> String {
    let from = from.map_or("1970-01-01".to_string(), |d| d.to_string());
    let to = to.map_or("2999-12-31".to_string(), |d| d.to_string());
    format!("[{from} 00:00:00,{to} 23:59:59]")
}

fn list(v: &Value, resource: &str) -> Vec<Value> {
    // An empty result comes back as `[]` instead of `{resource: []}`.
    v[resource].as_array().cloned().unwrap_or_default()
}

/// A specific price for everyone, any quantity and the whole product: what
/// a plain "sale price" is in PrestaShop.
fn is_plain_sale(sp: &Value) -> bool {
    [
        "id_customer",
        "id_group",
        "id_country",
        "id_currency",
        "id_cart",
        "id_product_attribute",
        "id_specific_price_rule",
    ]
    .iter()
    .all(|k| types::int(&sp[*k]).unwrap_or(0) == 0)
        && types::int(&sp["from_quantity"]).unwrap_or(1) <= 1
}

impl PrestaShop {
    pub fn new(
        base_url: &str,
        credentials: &Credentials,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            base: base_url.trim().trim_end_matches('/').to_string(),
            key: credentials
                .get(WEBSERVICE_KEY)
                .map(|k| k.trim().to_string())
                .unwrap_or_default(),
            transport,
            key_in_query: AtomicBool::new(false),
            meta: tokio::sync::Mutex::new(None),
        }
    }

    fn url(&self, path: &str, params: &[(String, String)]) -> Result<String, ConnectorError> {
        let mut url = reqwest::Url::parse(&format!("{}/api/{path}", self.base))
            .map_err(|e| ConnectorError::InvalidCredentials(format!("bad shop URL: {e}")))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("output_format", "JSON");
            q.extend_pairs(params);
            if self.key_in_query.load(Ordering::Relaxed) {
                q.append_pair("ws_key", &self.key);
            }
        }
        Ok(url.to_string())
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        params: &[(String, String)],
        body: Option<String>,
    ) -> Result<Value, ConnectorError> {
        loop {
            let mut req = HttpRequest::new(method, self.url(path, params)?)
                .header(
                    "Authorization",
                    format!("Basic {}", BASE64_STANDARD.encode(format!("{}:", self.key))),
                )
                .header("Accept", "application/json");
            if let Some(b) = &body {
                req = req.header("Content-Type", "application/xml");
                req.body = Some(b.clone());
            }
            let resp = self.transport.send(req).await?;
            let message = || types::error_message(&resp.body);
            let json_errors =
                serde_json::from_str::<Value>(&resp.body).is_ok_and(|v| v["errors"].is_array());
            match resp.status {
                200..=299 => {}
                // A plain-text 401 means the key never reached PrestaShop.
                401 if !json_errors && !self.key_in_query.swap(true, Ordering::Relaxed) => {
                    continue
                }
                401 | 403 => return Err(ConnectorError::Auth(message())),
                // A missing record is an empty 404; an HTML one is not the webservice.
                404 => {
                    return Err(ConnectorError::NotFound(if json_errors {
                        message()
                    } else if resp.body.trim().is_empty() {
                        path.to_string()
                    } else {
                        format!("{path} (is the webservice enabled at {}?)", self.base)
                    }))
                }
                405 => {
                    return Err(ConnectorError::Api {
                        status: 405,
                        message: format!(
                            "this shop does not accept {method:?} on {path}; updates need PrestaShop 8 or later"
                        ),
                    })
                }
                status => {
                    return Err(ConnectorError::Api {
                        status,
                        message: message(),
                    })
                }
            }
            if resp.body.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&resp.body).map_err(|_| {
                ConnectorError::InvalidResponse(format!(
                    "{} did not answer like the PrestaShop webservice; check the shop URL and that the webservice is enabled",
                    self.base
                ))
            });
        }
    }

    async fn get(&self, path: &str, params: Params) -> Result<Value, ConnectorError> {
        self.send(Method::Get, path, &params, None).await
    }

    async fn meta(&self) -> Result<Arc<Meta>, ConnectorError> {
        let mut cached = self.meta.lock().await;
        if let Some(m) = cached.as_ref() {
            return Ok(m.clone());
        }
        let states = self
            .get(
                "order_states",
                vec![("display".into(), "[id,name,paid,shipped]".into())],
            )
            .await?;
        // Currencies only label amounts; a key without access still works.
        let currencies = self
            .get(
                "currencies",
                vec![("display".into(), "[id,iso_code]".into())],
            )
            .await
            .unwrap_or(Value::Null);
        let meta = Arc::new(Meta {
            states: list(&states, "order_states")
                .iter()
                .filter_map(types::order_state)
                .collect(),
            currencies: list(&currencies, "currencies")
                .iter()
                .filter_map(|c| Some((types::string(&c["id"])?, types::string(&c["iso_code"])?)))
                .collect(),
        });
        *cached = Some(meta.clone());
        Ok(meta)
    }

    fn price_params(params: &mut Params) {
        params.push(("display".into(), PRODUCT_FIELDS.into()));
        params.push(("price[final][use_tax]".into(), "0".into()));
        params.push(("price[final][use_reduction]".into(), "1".into()));
    }

    /// Product-level stock rows of these products.
    async fn stock(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, (String, i64)>, ConnectorError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let v = self
            .get(
                "stock_availables",
                vec![
                    ("display".into(), "[id,id_product,quantity]".into()),
                    ("filter[id_product]".into(), one_of(ids.iter().cloned())),
                    ("filter[id_product_attribute]".into(), "[0]".into()),
                ],
            )
            .await?;
        let mut out = HashMap::new();
        for s in list(&v, "stock_availables") {
            if let (Some(p), Some(id), Some(q)) = (
                types::string(&s["id_product"]),
                types::string(&s["id"]),
                types::int(&s["quantity"]),
            ) {
                out.entry(p).or_insert((id, q));
            }
        }
        Ok(out)
    }

    async fn products(
        &self,
        mut params: Params,
    ) -> Result<(Vec<Value>, Vec<Product>), ConnectorError> {
        Self::price_params(&mut params);
        let raw = list(&self.get("products", params).await?, "products");
        let ids: Vec<String> = raw.iter().filter_map(|p| types::string(&p["id"])).collect();
        let stock = self.stock(&ids).await?;
        let products = raw
            .iter()
            .map(|p| {
                let q = types::string(&p["id"]).and_then(|id| stock.get(&id).map(|s| s.1));
                types::product(p, q)
            })
            .collect::<Result<_, _>>()?;
        Ok((raw, products))
    }

    async fn product_raw(&self, id: &str) -> Result<(Value, Product), ConnectorError> {
        let id = numeric_id("product", id)?;
        let (mut raw, mut products) = self
            .products(vec![("filter[id]".into(), format!("[{id}]"))])
            .await?;
        if products.is_empty() {
            return Err(ConnectorError::NotFound(format!("product {id}")));
        }
        Ok((raw.swap_remove(0), products.swap_remove(0)))
    }

    /// Replaces the product's plain sale with one at `sale` (0 ends it).
    async fn set_sale(
        &self,
        id: &str,
        regular: Decimal,
        sale: Decimal,
    ) -> Result<(), ConnectorError> {
        let existing = self
            .get(
                "specific_prices",
                vec![
                    ("display".into(), "full".into()),
                    ("filter[id_product]".into(), format!("[{id}]")),
                ],
            )
            .await?;
        for sp in list(&existing, "specific_prices")
            .iter()
            .filter(|sp| is_plain_sale(sp))
        {
            if let Some(sp_id) = types::string(&sp["id"]) {
                self.send(
                    Method::Delete,
                    &format!("specific_prices/{sp_id}"),
                    &[],
                    None,
                )
                .await?;
            }
        }
        if sale.is_zero() {
            return Ok(());
        }
        let zero = || Field::Text("0".into());
        let doc = xml::document(
            "specific_price",
            &[
                ("id_product", Field::Text(id.into())),
                ("id_product_attribute", zero()),
                ("id_shop", zero()),
                ("id_cart", zero()),
                ("id_currency", zero()),
                ("id_country", zero()),
                ("id_group", zero()),
                ("id_customer", zero()),
                ("price", Field::Text("-1".into())),
                ("from_quantity", Field::Text("1".into())),
                ("reduction", Field::Text((regular - sale).to_string())),
                ("reduction_tax", zero()),
                ("reduction_type", Field::Text("amount".into())),
                ("from", Field::Text("0000-00-00 00:00:00".into())),
                ("to", Field::Text("0000-00-00 00:00:00".into())),
            ],
        );
        self.send(Method::Post, "specific_prices", &[], Some(doc))
            .await
            .map(|_| ())
    }

    async fn customer_names(
        &self,
        ids: BTreeSet<String>,
    ) -> HashMap<String, (String, String, String)> {
        if ids.is_empty() {
            return HashMap::new();
        }
        let params = vec![
            ("display".into(), "[id,firstname,lastname,email]".into()),
            ("filter[id]".into(), one_of(ids)),
        ];
        // Names are a nicety; a key without customer access still lists orders.
        let Ok(v) = self.get("customers", params).await else {
            return HashMap::new();
        };
        list(&v, "customers")
            .iter()
            .filter_map(|c| {
                Some((
                    types::string(&c["id"])?,
                    (
                        types::string(&c["firstname"]).unwrap_or_default(),
                        types::string(&c["lastname"]).unwrap_or_default(),
                        types::string(&c["email"]).unwrap_or_default(),
                    ),
                ))
            })
            .collect()
    }

    async fn orders(&self, raw: Vec<Value>) -> Result<Vec<Order>, ConnectorError> {
        let meta = self.meta().await?;
        let customers = self
            .customer_names(
                raw.iter()
                    .filter_map(|o| types::string(&o["id_customer"]))
                    .filter(|c| c != "0")
                    .collect(),
            )
            .await;
        raw.iter()
            .map(|o| {
                let refs = types::OrderRefs {
                    states: &meta.states,
                    currency: types::string(&o["id_currency"])
                        .and_then(|c| meta.currencies.get(&c).cloned()),
                    customer: types::string(&o["id_customer"])
                        .and_then(|c| customers.get(&c).cloned()),
                };
                types::order(o, &refs)
            })
            .collect()
    }
}

/// Offset pagination with one extra row to learn whether a next page exists;
/// the webservice has no total count.
fn page<T>(mut items: Vec<T>, page: u32, per_page: u32) -> Page<T> {
    let more = items.len() > per_page as usize;
    items.truncate(per_page as usize);
    let seen = u64::from(page - 1) * u64::from(per_page) + items.len() as u64;
    Page {
        items,
        page,
        total_pages: (!more).then_some(page),
        total: (!more).then_some(seen),
    }
}

fn paging(params: &mut Params, page: Option<u32>, per_page: Option<u32>, max: u32) -> (u32, u32) {
    let page = page.unwrap_or(1).max(1);
    let size = per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, max);
    params.push((
        "limit".into(),
        format!("{},{}", u64::from(page - 1) * u64::from(size), size + 1),
    ));
    (page, size)
}

#[async_trait]
impl ShopConnector for PrestaShop {
    fn platform(&self) -> Platform {
        Platform::PrestaShop
    }

    async fn verify(&self) -> Result<(), ConnectorError> {
        self.get(
            "products",
            vec![
                ("display".into(), "[id]".into()),
                ("limit".into(), "1".into()),
            ],
        )
        .await?;
        self.meta().await.map(|_| ())
    }

    async fn list_products(&self, q: &ProductQuery) -> Result<Page<Product>, ConnectorError> {
        let mut params: Params = vec![("sort".into(), "[id_ASC]".into())];
        if let Some(s) = q.search.as_deref().map(literal).filter(|s| !s.is_empty()) {
            params.push(("filter[name]".into(), format!("%[{s}]%")));
        }
        if let Some(sku) = q.sku.as_deref().map(literal).filter(|s| !s.is_empty()) {
            params.push(("filter[reference]".into(), format!("[{sku}]")));
        }
        if let Some(st) = q.status.as_deref().filter(|s| !s.trim().is_empty()) {
            let active = u8::from(types::active(st)?);
            params.push(("filter[active]".into(), format!("[{active}]")));
        }
        let (n, size) = paging(&mut params, q.page, q.per_page, MAX_PER_PAGE);
        Ok(page(self.products(params).await?.1, n, size))
    }

    async fn get_product(&self, id: &str) -> Result<Product, ConnectorError> {
        Ok(self.product_raw(id).await?.1)
    }

    /// Prices are the base prices excluding tax. A sale price replaces the
    /// product's plain sale (for everyone, any quantity); 0 ends it.
    async fn update_product(
        &self,
        id: &str,
        patch: &ProductPatch,
    ) -> Result<Product, ConnectorError> {
        if patch.is_empty() {
            return Err(bad_request("nothing to change"));
        }
        let (raw, current) = self.product_raw(id).await?;
        let id = current.id.clone();
        if patch.stock_quantity.is_some() && types::has_combinations(&raw) {
            return Err(bad_request(
                "this product has combinations, whose stock is kept per combination; set it in the PrestaShop admin",
            ));
        }
        let regular = patch
            .regular_price
            .or(current.regular_price)
            .unwrap_or_default();
        if let Some(s) = patch.sale_price.filter(|s| !s.is_zero() && *s >= regular) {
            return Err(bad_request(format!(
                "the sale price {s} must be below the regular price {regular}"
            )));
        }
        let mut fields = Vec::new();
        if let Some(name) = &patch.name {
            // The language shown to the merchant changes; the others keep
            // their translations.
            let mut langs = types::languages(&raw["name"]);
            match langs.iter().position(|(_, v)| !v.is_empty()) {
                Some(i) => langs[i].1 = name.clone(),
                None if langs.is_empty() => langs.push((1, name.clone())),
                None => langs[0].1 = name.clone(),
            }
            fields.push(("name", Field::Lang(langs)));
        }
        if let Some(sku) = &patch.sku {
            fields.push(("reference", Field::Text(sku.trim().into())));
        }
        if let Some(st) = &patch.status {
            fields.push((
                "active",
                Field::Text(u8::from(types::active(st)?).to_string()),
            ));
        }
        if let Some(p) = patch.regular_price {
            fields.push(("price", Field::Text(p.to_string())));
        }
        if !fields.is_empty() {
            fields.insert(0, ("id", Field::Text(id.clone())));
            let doc = xml::document("product", &fields);
            self.send(Method::Patch, &format!("products/{id}"), &[], Some(doc))
                .await?;
        }
        if let Some(sale) = patch.sale_price {
            self.set_sale(&id, regular, sale).await?;
        }
        if let Some(q) = patch.stock_quantity {
            let rows = self.stock(std::slice::from_ref(&id)).await?;
            let Some((row, _)) = rows.get(&id) else {
                return Err(bad_request("PrestaShop has no stock row for this product"));
            };
            let doc = xml::document(
                "stock_available",
                &[
                    ("id", Field::Text(row.clone())),
                    ("quantity", Field::Text(q.to_string())),
                ],
            );
            self.send(
                Method::Patch,
                &format!("stock_availables/{row}"),
                &[],
                Some(doc),
            )
            .await?;
        }
        self.get_product(&id).await
    }

    async fn list_orders(&self, q: &OrderQuery) -> Result<Page<Order>, ConnectorError> {
        let mut params: Params = vec![
            ("display".into(), "full".into()),
            ("sort".into(), "[id_DESC]".into()),
        ];
        if let Some(s) = q.status.as_deref().filter(|s| !s.trim().is_empty()) {
            let meta = self.meta().await?;
            let ids = types::match_states(s, &meta.states)?
                .into_iter()
                .map(|st| st.id.to_string());
            params.push(("filter[current_state]".into(), one_of(ids)));
        }
        if let Some(c) = q.customer_id.as_deref().filter(|s| !s.trim().is_empty()) {
            params.push((
                "filter[id_customer]".into(),
                format!("[{}]", numeric_id("customer", c)?),
            ));
        }
        if q.after.is_some() || q.before.is_some() {
            params.push(("filter[date_add]".into(), date_range(q.after, q.before)));
            params.push(("date".into(), "1".into()));
        }
        let (n, size) = paging(&mut params, q.page, q.per_page, MAX_ORDERS_PER_PAGE);
        let raw = list(&self.get("orders", params).await?, "orders");
        Ok(page(self.orders(raw).await?, n, size))
    }

    /// Accepts the numeric id or the order reference, e.g. `XKBKNABJK`.
    async fn get_order(&self, id: &str) -> Result<Order, ConnectorError> {
        let id = id.trim();
        let raw = if numeric_id("order", id).is_ok() {
            self.get(&format!("orders/{id}"), vec![]).await?["order"].clone()
        } else {
            let params = vec![
                ("display".into(), "full".into()),
                ("filter[reference]".into(), format!("[{}]", literal(id))),
            ];
            list(&self.get("orders", params).await?, "orders")
                .into_iter()
                .next()
                .ok_or_else(|| ConnectorError::NotFound(format!("order {id}")))?
        };
        self.orders(vec![raw])
            .await?
            .pop()
            .ok_or_else(|| ConnectorError::NotFound(format!("order {id}")))
    }

    /// Adds an order history entry, which runs PrestaShop's usual side
    /// effects for the new status (stock, invoices) without emailing the
    /// customer.
    async fn update_order_status(&self, id: &str, status: &str) -> Result<Order, ConnectorError> {
        let meta = self.meta().await?;
        let target = match types::match_states(status, &meta.states)?.as_slice() {
            [one] => (*one).clone(),
            many => {
                let names: Vec<_> = many.iter().map(|s| s.name.as_str()).collect();
                return Err(bad_request(format!(
                    "{status:?} matches several order statuses ({}); name one",
                    names.join(", ")
                )));
            }
        };
        let order = self.get_order(id).await?;
        if order.status == target.name {
            return Ok(order);
        }
        let doc = xml::document(
            "order_history",
            &[
                ("id_order", Field::Text(order.id.clone())),
                ("id_order_state", Field::Text(target.id.to_string())),
            ],
        );
        self.send(Method::Post, "order_histories", &[], Some(doc))
            .await?;
        self.get_order(&order.id).await
    }

    /// The webservice cannot OR across fields, so a search looks at email
    /// addresses when it contains `@`, otherwise at last names and then
    /// first names.
    async fn list_customers(&self, q: &CustomerQuery) -> Result<Page<Customer>, ConnectorError> {
        let base: Params = vec![
            ("display".into(), "[id,email,firstname,lastname]".into()),
            ("sort".into(), "[id_ASC]".into()),
        ];
        let mut attempts: Vec<Params> = Vec::new();
        let email = q.email.as_deref().map(literal).filter(|s| !s.is_empty());
        let search = q.search.as_deref().map(literal).filter(|s| !s.is_empty());
        match (email, search) {
            (Some(e), _) => attempts.push(vec![("filter[email]".into(), format!("[{e}]"))]),
            (None, Some(s)) if s.contains('@') => {
                attempts.push(vec![("filter[email]".into(), format!("%[{s}]%"))])
            }
            (None, Some(s)) => {
                attempts.push(vec![("filter[lastname]".into(), format!("%[{s}]%"))]);
                attempts.push(vec![("filter[firstname]".into(), format!("%[{s}]%"))]);
            }
            (None, None) => attempts.push(Vec::new()),
        }
        let mut result = None;
        for filter in attempts {
            let mut params = base.clone();
            params.extend(filter);
            let (n, size) = paging(&mut params, q.page, q.per_page, MAX_PER_PAGE);
            let items = list(&self.get("customers", params).await?, "customers")
                .iter()
                .map(types::customer)
                .collect::<Result<Vec<_>, _>>()?;
            let found = !items.is_empty();
            result = Some(page(items, n, size));
            if found {
                break;
            }
        }
        Ok(result.expect("at least one attempt"))
    }

    /// Sums the range's valid orders (statuses PrestaShop counts as sales).
    /// Net sales are products after discounts, excluding tax and shipping.
    async fn sales_report(&self, range: &DateRange) -> Result<SalesReport, ConnectorError> {
        if range.to < range.from {
            return Err(bad_request("`to` is before `from`"));
        }
        let meta = self.meta().await?;
        let mut report = SalesReport {
            from: range.from,
            to: range.to,
            total_sales: Decimal::ZERO,
            net_sales: Decimal::ZERO,
            orders: 0,
            items: 0,
            currency: None,
        };
        let mut currencies = BTreeSet::new();
        let mut offset = 0u64;
        loop {
            let params = vec![
                ("display".into(), "full".into()),
                (
                    "filter[date_add]".into(),
                    date_range(Some(range.from), Some(range.to)),
                ),
                ("date".into(), "1".into()),
                ("filter[valid]".into(), "[1]".into()),
                ("sort".into(), "[id_ASC]".into()),
                ("limit".into(), format!("{offset},{REPORT_PAGE}")),
            ];
            let orders = list(&self.get("orders", params).await?, "orders");
            for o in &orders {
                let money = |k: &str| types::decimal(&o[k]).map(Option::unwrap_or_default);
                report.total_sales += money("total_paid_tax_incl")?;
                report.net_sales += money("total_products")? - money("total_discounts_tax_excl")?;
                report.items += o["associations"]["order_rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| types::int(&r["product_quantity"]))
                    .sum::<i64>()
                    .max(0) as u64;
                report.orders += 1;
                if let Some(c) = types::string(&o["id_currency"]) {
                    currencies.insert(meta.currencies.get(&c).cloned().unwrap_or(c));
                }
            }
            if report.orders > MAX_REPORT_ORDERS {
                return Err(bad_request(format!(
                    "more than {MAX_REPORT_ORDERS} orders in this range; ask for a shorter range"
                )));
            }
            if orders.len() < REPORT_PAGE as usize {
                break;
            }
            offset += u64::from(REPORT_PAGE);
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
    use crate::transport::{FakeTransport, HttpResponse};
    use serde_json::json;
    use std::str::FromStr;

    fn shop(t: &Arc<FakeTransport>) -> PrestaShop {
        let creds = Credentials::from([(WEBSERVICE_KEY.into(), "KEY".into())]);
        PrestaShop::new("https://shop.test/es/", &creds, t.clone())
    }

    fn ok(t: &FakeTransport, method: Method, path: &str, body: serde_json::Value) {
        t.on(method, path, HttpResponse::new(200, body.to_string()));
    }

    fn query(req: &HttpRequest, key: &str) -> Option<String> {
        req.query_pairs()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn states(t: &FakeTransport) {
        ok(
            t,
            Method::Get,
            "/es/api/order_states",
            json!({"order_states": [
                {"id": 2, "name": [{"id": 1, "value": "Payment accepted"}], "paid": "1", "shipped": "0"},
                {"id": 3, "name": [{"id": 1, "value": "Processing in progress"}], "paid": "1", "shipped": "0"},
                {"id": 6, "name": [{"id": 1, "value": "Canceled"}], "paid": "0", "shipped": "0"},
                {"id": 9, "name": [{"id": 1, "value": "On backorder (paid)"}], "paid": "1", "shipped": "0"},
                {"id": 12, "name": [{"id": 1, "value": "On backorder (not paid)"}], "paid": "0", "shipped": "0"}
            ]}),
        );
        ok(
            t,
            Method::Get,
            "/es/api/currencies",
            json!({"currencies": [{"id": 1, "iso_code": "EUR"}]}),
        );
    }

    fn mug() -> serde_json::Value {
        json!({"id": 6, "reference": "demo_11", "price": "11.900000", "final": "11.900000",
            "active": "1", "cache_default_attribute": 0,
            "name": [{"id": 1, "value": "Mug"}, {"id": 2, "value": "Taza"}]})
    }

    #[tokio::test]
    async fn products_page_with_one_extra_row_and_read_stock_rows() {
        let t = Arc::new(FakeTransport::new());
        ok(
            &t,
            Method::Get,
            "/es/api/products",
            json!({"products": [mug(), mug(), mug()]}),
        );
        ok(
            &t,
            Method::Get,
            "/es/api/stock_availables",
            json!({"stock_availables": [
            {"id": 40, "id_product": 6, "quantity": 0}]}),
        );
        let page = shop(&t)
            .list_products(&ProductQuery {
                search: Some("mug [x]".into()),
                status: Some("enabled".into()),
                page: Some(3),
                per_page: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(
            (page.total, page.total_pages),
            (None, None),
            "a next page exists"
        );
        assert_eq!(page.items[0].stock_status.as_deref(), Some("outofstock"));

        let req = &t.requests()[0];
        assert_eq!(req.header_value("authorization"), Some("Basic S0VZOg=="));
        assert_eq!(query(req, "output_format").as_deref(), Some("JSON"));
        assert_eq!(query(req, "limit").as_deref(), Some("4,3"));
        assert_eq!(query(req, "filter[name]").as_deref(), Some("%[mug x]%"));
        assert_eq!(query(req, "filter[active]").as_deref(), Some("[1]"));
        assert_eq!(
            query(req, "price[final][use_reduction]").as_deref(),
            Some("1")
        );
        assert_eq!(
            query(&t.requests()[1], "filter[id_product_attribute]").as_deref(),
            Some("[0]")
        );
    }

    #[tokio::test]
    async fn a_stripped_authorization_header_falls_back_to_ws_key() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/es/api/products",
            HttpResponse::new(401, "401 Unauthorized"),
        );
        ok(
            &t,
            Method::Get,
            "/es/api/products",
            json!({"products": [{"id": 1}]}),
        );
        states(&t);
        let s = shop(&t);
        s.verify().await.unwrap();
        let reqs = t.requests();
        assert_eq!(query(&reqs[0], "ws_key"), None);
        assert_eq!(query(&reqs[1], "ws_key").as_deref(), Some("KEY"));
        assert_eq!(
            query(reqs.last().unwrap(), "ws_key").as_deref(),
            Some("KEY"),
            "remembered"
        );
    }

    #[tokio::test]
    async fn rejected_keys_report_prestashops_reason() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/es/api/products",
            HttpResponse::new(401, r#"{"errors":[{"code":26,"message":"Resource of type \"products\" is not allowed with this authentication key"}]}"#),
        );
        let err = shop(&t).verify().await.unwrap_err();
        assert_eq!(
            err,
            ConnectorError::Auth(
                "Resource of type \"products\" is not allowed with this authentication key".into()
            )
        );
        assert_eq!(t.requests().len(), 1, "no ws_key retry for a JSON error");
    }

    #[tokio::test]
    async fn update_product_keeps_translations_replaces_the_sale_and_sets_stock() {
        let t = Arc::new(FakeTransport::new());
        ok(
            &t,
            Method::Get,
            "/es/api/products",
            json!({"products": [mug()]}),
        );
        ok(
            &t,
            Method::Get,
            "/es/api/stock_availables",
            json!({"stock_availables": [
            {"id": 40, "id_product": 6, "quantity": 5}]}),
        );
        ok(&t, Method::Patch, "/es/api/products/6", json!({}));
        ok(
            &t,
            Method::Get,
            "/es/api/specific_prices",
            json!({"specific_prices": [
                {"id": 1, "id_product": 6, "id_customer": 0, "id_group": 0, "id_country": 0, "id_currency": 0,
                 "id_cart": 0, "id_product_attribute": 0, "id_specific_price_rule": 0, "from_quantity": 1},
                {"id": 2, "id_product": 6, "id_customer": 0, "id_group": 3, "id_country": 0, "id_currency": 0,
                 "id_cart": 0, "id_product_attribute": 0, "id_specific_price_rule": 0, "from_quantity": 1}
            ]}),
        );
        t.on(
            Method::Delete,
            "/es/api/specific_prices/1",
            HttpResponse::new(200, ""),
        );
        t.on(
            Method::Post,
            "/es/api/specific_prices",
            HttpResponse::new(201, "{}"),
        );
        ok(&t, Method::Patch, "/es/api/stock_availables/40", json!({}));
        shop(&t)
            .update_product(
                "6",
                &ProductPatch {
                    name: Some("Mug & Co".into()),
                    regular_price: Some(dec("20")),
                    sale_price: Some(dec("15.50")),
                    stock_quantity: Some(9),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let reqs = t.requests();
        let body = |path: &str, m: Method| {
            reqs.iter()
                .find(|r| r.path() == path && r.method == m)
                .and_then(|r| r.body.clone())
                .unwrap_or_else(|| panic!("no {m:?} {path}"))
        };
        let product = body("/es/api/products/6", Method::Patch);
        assert!(product.contains("<product><id>6</id><name><language id=\"1\">Mug &amp; Co</language><language id=\"2\">Taza</language></name><price>20</price></product>"), "{product}");
        assert!(
            reqs.iter().all(|r| r.path() != "/es/api/specific_prices/2"),
            "group prices stay"
        );
        let sale = body("/es/api/specific_prices", Method::Post);
        assert!(
            sale.contains("<reduction>4.50</reduction>")
                && sale.contains("<reduction_type>amount</reduction_type>"),
            "{sale}"
        );
        let stock = body("/es/api/stock_availables/40", Method::Patch);
        assert!(
            stock.contains("<id>40</id><quantity>9</quantity>"),
            "{stock}"
        );
    }

    #[tokio::test]
    async fn invalid_updates_are_refused_before_writing() {
        let t = Arc::new(FakeTransport::new());
        let mut shirt = mug();
        shirt["cache_default_attribute"] = json!(1);
        ok(
            &t,
            Method::Get,
            "/es/api/products",
            json!({"products": [shirt]}),
        );
        ok(
            &t,
            Method::Get,
            "/es/api/stock_availables",
            json!({"stock_availables": []}),
        );
        let s = shop(&t);
        for patch in [
            ProductPatch {
                stock_quantity: Some(1),
                ..Default::default()
            },
            ProductPatch {
                sale_price: Some(dec("12")),
                ..Default::default()
            },
        ] {
            let err = s.update_product("1", &patch).await.unwrap_err();
            assert!(
                matches!(err, ConnectorError::Api { status: 400, .. }),
                "{err}"
            );
        }
        assert!(t.requests().iter().all(|r| r.method == Method::Get));
    }

    #[tokio::test]
    async fn orders_map_states_customers_and_currency() {
        let t = Arc::new(FakeTransport::new());
        states(&t);
        ok(
            &t,
            Method::Get,
            "/es/api/orders",
            json!({"orders": [{
                "id": 2, "reference": "XKBKNABJK", "current_state": 3, "id_customer": 7, "id_currency": 1,
                "total_paid_tax_incl": "68.200000", "date_add": "2026-10-18 04:28:57",
                "associations": {"order_rows": [{"product_id": 6, "product_quantity": 2,
                    "product_name": "Mug", "unit_price_tax_incl": "11.900000"}]}
            }]}),
        );
        ok(
            &t,
            Method::Get,
            "/es/api/customers",
            json!({"customers": [
            {"id": 7, "firstname": "John", "lastname": "DOE", "email": "pub@prestashop.com"}]}),
        );
        let page = shop(&t)
            .list_orders(&OrderQuery {
                status: Some("backorder".into()),
                customer_id: Some("7".into()),
                after: NaiveDate::from_ymd_opt(2026, 10, 1),
                ..Default::default()
            })
            .await
            .unwrap();
        let o = &page.items[0];
        assert_eq!(
            (o.number.as_str(), o.status.as_str()),
            ("XKBKNABJK", "Processing in progress")
        );
        assert_eq!(o.customer_name.as_deref(), Some("John DOE"));
        assert_eq!((o.currency.as_str(), o.total), ("EUR", dec("68.2")));
        assert_eq!(o.line_items[0].total, dec("23.8"));
        assert_eq!(page.total_pages, Some(1));

        let req = t
            .requests()
            .into_iter()
            .find(|r| r.path() == "/es/api/orders")
            .unwrap();
        assert_eq!(
            query(&req, "filter[current_state]").as_deref(),
            Some("[9|12]")
        );
        assert_eq!(query(&req, "filter[id_customer]").as_deref(), Some("[7]"));
        assert_eq!(
            query(&req, "filter[date_add]").as_deref(),
            Some("[2026-10-01 00:00:00,2999-12-31 23:59:59]")
        );
        assert_eq!(query(&req, "date").as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn status_changes_add_an_order_history_for_one_matching_state() {
        let t = Arc::new(FakeTransport::new());
        states(&t);
        let order =
            json!({"order": {"id": 2, "reference": "R", "current_state": 2, "id_customer": 0}});
        ok(&t, Method::Get, "/es/api/orders/2", order);
        t.on(
            Method::Post,
            "/es/api/order_histories",
            HttpResponse::new(201, "{}"),
        );
        let s = shop(&t);
        let ambiguous = s.update_order_status("2", "backorder").await.unwrap_err();
        assert!(ambiguous.to_string().contains("several"), "{ambiguous}");
        s.update_order_status("2", "cancelled").await.unwrap();
        let post = t
            .requests()
            .into_iter()
            .find(|r| r.method == Method::Post)
            .unwrap();
        assert!(post
            .body
            .unwrap()
            .contains("<order_history><id_order>2</id_order><id_order_state>6</id_order_state></order_history>"));
    }

    #[tokio::test]
    async fn sales_report_sums_valid_orders_across_pages() {
        let t = Arc::new(FakeTransport::new());
        states(&t);
        let order = |total: &str, qty: i64| {
            json!({
                "id_currency": 1, "total_paid_tax_incl": total, "total_products": "10.000000",
                "total_discounts_tax_excl": "1.000000",
                "associations": {"order_rows": [{"product_quantity": qty}]}
            })
        };
        let full: Vec<_> = (0..REPORT_PAGE).map(|_| order("12.10", 1)).collect();
        ok(&t, Method::Get, "/es/api/orders", json!({"orders": full}));
        ok(
            &t,
            Method::Get,
            "/es/api/orders",
            json!({"orders": [order("5.00", 3)]}),
        );
        let r = shop(&t)
            .sales_report(&DateRange {
                from: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                to: NaiveDate::from_ymd_opt(2026, 10, 16).unwrap(),
            })
            .await
            .unwrap();
        assert_eq!((r.orders, r.items), (101, 103));
        assert_eq!(r.total_sales, dec("1215"));
        assert_eq!(r.net_sales, dec("909"));
        assert_eq!(r.currency.as_deref(), Some("EUR"));
        let reqs: Vec<_> = t
            .requests()
            .into_iter()
            .filter(|r| r.path() == "/es/api/orders")
            .collect();
        assert_eq!(query(&reqs[0], "filter[valid]").as_deref(), Some("[1]"));
        assert_eq!(query(&reqs[1], "limit").as_deref(), Some("100,100"));
    }

    #[tokio::test]
    async fn missing_records_are_not_blamed_on_the_webservice() {
        let t = Arc::new(FakeTransport::new());
        states(&t);
        t.on(
            Method::Get,
            "/es/api/orders/999",
            HttpResponse::new(404, ""),
        );
        t.on(
            Method::Get,
            "/es/api/products",
            HttpResponse::new(404, "<html>Not Found</html>"),
        );
        let s = shop(&t);
        assert_eq!(
            s.get_order("999").await.unwrap_err(),
            ConnectorError::NotFound("orders/999".into())
        );
        let err = s.verify().await.unwrap_err().to_string();
        assert!(err.contains("is the webservice enabled"), "{err}");
    }

    #[tokio::test]
    async fn shops_without_patch_are_told_updates_need_prestashop_8() {
        let t = Arc::new(FakeTransport::new());
        ok(
            &t,
            Method::Get,
            "/es/api/products",
            json!({"products": [mug()]}),
        );
        ok(
            &t,
            Method::Get,
            "/es/api/stock_availables",
            json!({"stock_availables": []}),
        );
        t.on(
            Method::Patch,
            "/es/api/products/6",
            HttpResponse::new(405, ""),
        );
        let err = shop(&t)
            .update_product(
                "6",
                &ProductPatch {
                    sku: Some("X".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("PrestaShop 8"), "{err}");
    }
}
