//! WooCommerce REST API v3 connector (`{shop}/wp-json/wc/v3`).

mod auth;
mod types;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Days;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};

use crate::domain::*;
use crate::shop::unsupported;
use crate::transport::{HttpResponse, HttpTransport, Method};
use crate::{ConnectorError, Credentials, Platform, ShopConnector};
pub use auth::WooAuth;
use types::{WooCategory, WooCoupon, WooCustomer, WooOrder, WooProduct, WooSalesReport};

const DEFAULT_PER_PAGE: u32 = 10;
const MAX_PER_PAGE: u32 = 100;

pub struct WooCommerce {
    api: String,
    auth: WooAuth,
    transport: Arc<dyn HttpTransport>,
}

impl WooCommerce {
    pub fn new(
        base_url: &str,
        credentials: &Credentials,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        let get = |k: &str| {
            credentials
                .get(k)
                .map(|v| v.trim().to_string())
                .unwrap_or_default()
        };
        Self {
            api: format!("{}/wp-json/wc/v3", base_url.trim_end_matches('/')),
            auth: WooAuth {
                consumer_key: get("consumer_key"),
                consumer_secret: get("consumer_secret"),
            },
            transport,
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        params: Vec<(String, String)>,
        body: Option<Value>,
    ) -> Result<HttpResponse, ConnectorError> {
        let url = format!("{}{path}", self.api);
        let mut req = self.auth.apply(method, &url, params);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = self.transport.send(req).await?;
        if resp.is_success() {
            return Ok(resp);
        }
        let message = serde_json::from_str::<Value>(&resp.body)
            .ok()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| resp.body.chars().take(300).collect());
        Err(match resp.status {
            401 | 403 => ConnectorError::Auth(message),
            404 => ConnectorError::NotFound(message),
            status => ConnectorError::Api { status, message },
        })
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        params: Vec<(String, String)>,
    ) -> Result<(T, HttpResponse), ConnectorError> {
        let resp = self.request(Method::Get, path, params, None).await?;
        Ok((parse(&resp)?, resp))
    }

    async fn put<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T, ConnectorError> {
        let resp = self
            .request(Method::Put, path, Vec::new(), Some(body))
            .await?;
        parse(&resp)
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
    ) -> Result<T, ConnectorError> {
        let resp = self
            .request(Method::Post, path, Vec::new(), Some(body))
            .await?;
        parse(&resp)
    }

    /// `force=true` deletes for good; without it most resources only move to
    /// the trash, and some refuse.
    async fn delete(&self, path: &str) -> Result<(), ConnectorError> {
        self.request(
            Method::Delete,
            path,
            vec![("force".into(), "true".into())],
            None,
        )
        .await
        .map(|_| ())
    }

    async fn customer(&self, id: &str) -> Result<WooCustomer, ConnectorError> {
        let (c, _) = self
            .get(&format!("/customers/{}", path_id(id)?), Vec::new())
            .await?;
        Ok(c)
    }

    /// Sets one address slot and returns what the slot holds afterwards.
    async fn put_slot(
        &self,
        customer_id: &str,
        slot: &'static str,
        fields: Map<String, Value>,
    ) -> Result<Option<Address>, ConnectorError> {
        let c: WooCustomer = self
            .put(
                &format!("/customers/{}", path_id(customer_id)?),
                json!({ slot: fields }),
            )
            .await?;
        Ok(match slot {
            "billing" => c.billing.into_address(slot),
            _ => c.shipping.into_address(slot),
        })
    }

    async fn list<W, T>(
        &self,
        path: &str,
        mut params: Vec<(String, String)>,
        page: Option<u32>,
        per_page: Option<u32>,
    ) -> Result<Page<T>, ConnectorError>
    where
        W: DeserializeOwned,
        T: TryFrom<W, Error = ConnectorError>,
    {
        let page = page.unwrap_or(1).max(1);
        let per_page = per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE);
        params.push(("page".into(), page.to_string()));
        params.push(("per_page".into(), per_page.to_string()));
        let (raw, resp): (Vec<W>, _) = self.get(path, params).await?;
        Ok(Page {
            items: raw.into_iter().map(T::try_from).collect::<Result<_, _>>()?,
            page,
            total_pages: resp.header("x-wp-totalpages").and_then(|v| v.parse().ok()),
            total: resp.header("x-wp-total").and_then(|v| v.parse().ok()),
        })
    }
}

fn parse<T: DeserializeOwned>(resp: &HttpResponse) -> Result<T, ConnectorError> {
    serde_json::from_str(&resp.body).map_err(|e| {
        ConnectorError::InvalidResponse(format!(
            "{e} in {}",
            resp.body.chars().take(200).collect::<String>()
        ))
    })
}

fn opt(params: &mut Vec<(String, String)>, key: &str, value: &Option<String>) {
    if let Some(v) = value.as_ref().filter(|v| !v.trim().is_empty()) {
        params.push((key.into(), v.trim().into()));
    }
}

/// Ids go into URL paths, so only plain numeric ids are accepted.
fn path_id(id: &str) -> Result<&str, ConnectorError> {
    let id = id.trim();
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        Ok(id)
    } else {
        Err(ConnectorError::NotFound(format!(
            "{id:?} is not a WooCommerce id"
        )))
    }
}

fn bad_request(message: impl Into<String>) -> ConnectorError {
    ConnectorError::Api {
        status: 400,
        message: message.into(),
    }
}

/// Ids that go into request bodies, where WooCommerce wants numbers.
fn numeric(id: &str, what: &str) -> Result<u64, ConnectorError> {
    id.trim()
        .parse()
        .map_err(|_| bad_request(format!("{id:?} is not a WooCommerce {what} id")))
}

fn product_body(patch: &ProductPatch) -> Result<Map<String, Value>, ConnectorError> {
    let mut body = Map::new();
    if let Some(v) = &patch.name {
        body.insert("name".into(), json!(v));
    }
    if let Some(v) = &patch.sku {
        body.insert("sku".into(), json!(v));
    }
    if let Some(v) = &patch.status {
        body.insert("status".into(), json!(v));
    }
    if let Some(v) = &patch.regular_price {
        body.insert("regular_price".into(), json!(v.to_string()));
    }
    if let Some(v) = &patch.sale_price {
        // An empty sale price ends the sale; "0" would make the product free.
        let price = if v.is_zero() {
            String::new()
        } else {
            v.to_string()
        };
        body.insert("sale_price".into(), json!(price));
    }
    if let Some(v) = patch.stock_quantity {
        // WooCommerce ignores stock_quantity unless stock management is on.
        body.insert("manage_stock".into(), json!(true));
        body.insert("stock_quantity".into(), json!(v));
    }
    if let Some(v) = &patch.description {
        body.insert("description".into(), json!(v));
    }
    if let Some(ids) = &patch.category_ids {
        let categories = ids
            .iter()
            .map(|id| Ok(json!({"id": numeric(id, "category")?})))
            .collect::<Result<Vec<_>, ConnectorError>>()?;
        body.insert("categories".into(), Value::Array(categories));
    }
    Ok(body)
}

fn category_body(c: &CategoryPatch) -> Result<Value, ConnectorError> {
    let mut body = Map::new();
    if let Some(v) = &c.name {
        body.insert("name".into(), json!(v));
    }
    if let Some(v) = &c.parent_id {
        // An empty parent moves the category to the top level.
        let parent = if v.trim().is_empty() {
            0
        } else {
            numeric(v, "category")?
        };
        body.insert("parent".into(), json!(parent));
    }
    if let Some(v) = &c.description {
        body.insert("description".into(), json!(v));
    }
    Ok(Value::Object(body))
}

fn customer_body(c: &CustomerPatch) -> Result<Value, ConnectorError> {
    if c.accepts_marketing.is_some() {
        return Err(unsupported(
            Platform::WooCommerce,
            "marketing consent (WooCommerce core has no such field; record it in your newsletter tool)",
        ));
    }
    let mut body = Map::new();
    for (key, value) in [
        ("email", &c.email),
        ("first_name", &c.first_name),
        ("last_name", &c.last_name),
    ] {
        if let Some(v) = value {
            body.insert(key.into(), json!(v.trim()));
        }
    }
    if let Some(phone) = &c.phone {
        body.insert("billing".into(), json!({"phone": phone.trim()}));
    }
    Ok(Value::Object(body))
}

/// The address fields as WooCommerce names them. With `complete`, fields left
/// out are cleared, so the slot holds exactly this address.
fn address_fields(a: &AddressPatch, complete: bool) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in [
        ("first_name", &a.first_name),
        ("last_name", &a.last_name),
        ("company", &a.company),
        ("address_1", &a.address1),
        ("address_2", &a.address2),
        ("city", &a.city),
        ("postcode", &a.postcode),
        ("state", &a.state),
        ("country", &a.country),
        ("phone", &a.phone),
    ] {
        match value {
            Some(v) => {
                out.insert(key.into(), json!(v.trim()));
            }
            None if complete => {
                out.insert(key.into(), json!(""));
            }
            None => {}
        }
    }
    out
}

/// WooCommerce customers have exactly two addresses, `billing` and `shipping`.
fn slot(name: &str) -> Result<&'static str, ConnectorError> {
    match name.trim().to_ascii_lowercase().as_str() {
        "billing" => Ok("billing"),
        "shipping" => Ok("shipping"),
        other => Err(bad_request(format!(
            "WooCommerce customers have a billing and a shipping address, not {other:?}; use kind billing or shipping"
        ))),
    }
}

const COUPON_KINDS: [&str; 3] = ["percent", "fixed_cart", "fixed_product"];

fn coupon_body(c: &CouponPatch) -> Result<Value, ConnectorError> {
    let mut body = Map::new();
    if let Some(v) = &c.code {
        body.insert("code".into(), json!(v.trim()));
    }
    if let Some(v) = &c.kind {
        if !COUPON_KINDS.contains(&v.as_str()) {
            return Err(bad_request(format!(
                "coupon kind must be one of {}",
                COUPON_KINDS.join(", ")
            )));
        }
        body.insert("discount_type".into(), json!(v));
    }
    if let Some(v) = &c.amount {
        body.insert("amount".into(), json!(v.to_string()));
    }
    if let Some(v) = &c.expires_on {
        body.insert("date_expires".into(), json!(v.to_string()));
    }
    if let Some(v) = c.usage_limit {
        body.insert("usage_limit".into(), json!(v));
    }
    if let Some(v) = &c.minimum_amount {
        body.insert("minimum_amount".into(), json!(v.to_string()));
    }
    if let Some(v) = &c.description {
        body.insert("description".into(), json!(v));
    }
    if let Some(active) = c.active {
        // WooCommerce has no on/off switch for coupons; a draft does not apply.
        body.insert(
            "status".into(),
            json!(if active { "publish" } else { "draft" }),
        );
    }
    Ok(Value::Object(body))
}

fn raw_method(m: &str) -> Result<Method, ConnectorError> {
    Ok(match m.trim().to_ascii_uppercase().as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "PATCH" => Method::Patch,
        "DELETE" => Method::Delete,
        other => return Err(bad_request(format!("unknown method {other:?}"))),
    })
}

#[async_trait]
impl ShopConnector for WooCommerce {
    fn platform(&self) -> Platform {
        Platform::WooCommerce
    }

    async fn verify(&self) -> Result<(), ConnectorError> {
        self.get::<Value>("/products", vec![("per_page".into(), "1".into())])
            .await
            .map(|_| ())
    }

    async fn list_products(&self, q: &ProductQuery) -> Result<Page<Product>, ConnectorError> {
        let mut params = Vec::new();
        opt(&mut params, "search", &q.search);
        opt(&mut params, "sku", &q.sku);
        opt(&mut params, "status", &q.status);
        self.list::<WooProduct, _>("/products", params, q.page, q.per_page)
            .await
    }

    async fn get_product(&self, id: &str) -> Result<Product, ConnectorError> {
        let (p, _): (WooProduct, _) = self
            .get(&format!("/products/{}", path_id(id)?), Vec::new())
            .await?;
        p.try_into()
    }

    async fn update_product(
        &self,
        id: &str,
        patch: &ProductPatch,
    ) -> Result<Product, ConnectorError> {
        let body = product_body(patch)?;
        let p: WooProduct = self
            .put(&format!("/products/{}", path_id(id)?), Value::Object(body))
            .await?;
        p.try_into()
    }

    async fn list_orders(&self, q: &OrderQuery) -> Result<Page<Order>, ConnectorError> {
        let mut params = vec![
            ("orderby".to_string(), "date".to_string()),
            ("order".to_string(), "desc".to_string()),
        ];
        opt(&mut params, "status", &q.status);
        opt(&mut params, "customer", &q.customer_id);
        if let Some(after) = q.after {
            params.push(("after".into(), format!("{after}T00:00:00")));
        }
        if let Some(before) = q.before.and_then(|d| d.checked_add_days(Days::new(1))) {
            // `before` is exclusive in WooCommerce; the tool's date is inclusive.
            params.push(("before".into(), format!("{before}T00:00:00")));
        }
        self.list::<WooOrder, _>("/orders", params, q.page, q.per_page)
            .await
    }

    async fn get_order(&self, id: &str) -> Result<Order, ConnectorError> {
        let (o, _): (WooOrder, _) = self
            .get(&format!("/orders/{}", path_id(id)?), Vec::new())
            .await?;
        o.try_into()
    }

    async fn update_order_status(&self, id: &str, status: &str) -> Result<Order, ConnectorError> {
        let o: WooOrder = self
            .put(
                &format!("/orders/{}", path_id(id)?),
                json!({"status": status.trim()}),
            )
            .await?;
        o.try_into()
    }

    async fn list_customers(&self, q: &CustomerQuery) -> Result<Page<Customer>, ConnectorError> {
        let mut params = Vec::new();
        opt(&mut params, "search", &q.search);
        opt(&mut params, "email", &q.email);
        self.list::<WooCustomer, _>("/customers", params, q.page, q.per_page)
            .await
    }

    async fn sales_report(&self, range: &DateRange) -> Result<SalesReport, ConnectorError> {
        if range.to < range.from {
            return Err(ConnectorError::Api {
                status: 400,
                message: "`to` is before `from`".into(),
            });
        }
        let (rows, _): (Vec<WooSalesReport>, _) = self
            .get(
                "/reports/sales",
                vec![
                    ("date_min".into(), range.from.to_string()),
                    ("date_max".into(), range.to.to_string()),
                ],
            )
            .await?;
        rows.into_iter()
            .next()
            .ok_or_else(|| ConnectorError::InvalidResponse("empty sales report".into()))?
            .into_report(range)
    }

    /// WooCommerce core keeps no record of carts that did not become orders.
    fn hidden(&self) -> &'static [&'static str] {
        &["shop_list_abandoned_carts"]
    }

    fn api_hint(&self) -> &'static str {
        "WooCommerce REST API v3: paths are relative to /wp-json/wc/v3 (e.g. /products/attributes, /shipping/zones, /reports/top_sellers) and bodies are JSON."
    }

    async fn create_product(&self, product: &ProductPatch) -> Result<Product, ConnectorError> {
        let mut body = product_body(product)?;
        body.entry("type").or_insert(json!("simple"));
        let p: WooProduct = self.post("/products", Value::Object(body)).await?;
        p.try_into()
    }

    async fn delete_product(&self, id: &str) -> Result<(), ConnectorError> {
        self.delete(&format!("/products/{}", path_id(id)?)).await
    }

    async fn list_categories(&self, q: &CategoryQuery) -> Result<Page<Category>, ConnectorError> {
        let mut params = Vec::new();
        opt(&mut params, "search", &q.search);
        self.list::<WooCategory, _>("/products/categories", params, q.page, q.per_page)
            .await
    }

    async fn create_category(&self, c: &CategoryPatch) -> Result<Category, ConnectorError> {
        let cat: WooCategory = self.post("/products/categories", category_body(c)?).await?;
        cat.try_into()
    }

    async fn update_category(
        &self,
        id: &str,
        c: &CategoryPatch,
    ) -> Result<Category, ConnectorError> {
        let cat: WooCategory = self
            .put(
                &format!("/products/categories/{}", path_id(id)?),
                category_body(c)?,
            )
            .await?;
        cat.try_into()
    }

    async fn delete_category(&self, id: &str) -> Result<(), ConnectorError> {
        self.delete(&format!("/products/categories/{}", path_id(id)?))
            .await
    }

    async fn get_customer(&self, id: &str) -> Result<Customer, ConnectorError> {
        self.customer(id).await?.try_into()
    }

    async fn create_customer(&self, c: &CustomerPatch) -> Result<Customer, ConnectorError> {
        let created: WooCustomer = self.post("/customers", customer_body(c)?).await?;
        created.try_into()
    }

    async fn update_customer(
        &self,
        id: &str,
        c: &CustomerPatch,
    ) -> Result<Customer, ConnectorError> {
        let updated: WooCustomer = self
            .put(&format!("/customers/{}", path_id(id)?), customer_body(c)?)
            .await?;
        updated.try_into()
    }

    async fn delete_customer(&self, id: &str) -> Result<(), ConnectorError> {
        self.delete(&format!("/customers/{}", path_id(id)?)).await
    }

    async fn list_addresses(&self, customer_id: &str) -> Result<Vec<Address>, ConnectorError> {
        let c = self.customer(customer_id).await?;
        Ok([
            c.billing.into_address("billing"),
            c.shipping.into_address("shipping"),
        ]
        .into_iter()
        .flatten()
        .collect())
    }

    /// Fills the billing or shipping slot (`kind`), replacing what it held.
    async fn create_address(
        &self,
        customer_id: &str,
        a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        let slot = slot(a.kind.as_deref().unwrap_or_default())?;
        self.put_slot(customer_id, slot, address_fields(a, true))
            .await?
            .ok_or_else(|| bad_request("the address is empty"))
    }

    async fn update_address(
        &self,
        customer_id: &str,
        id: &str,
        a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        let slot = slot(id)?;
        if a.kind
            .as_deref()
            .is_some_and(|k| !k.eq_ignore_ascii_case(slot))
        {
            return Err(bad_request(
                "an address cannot move between the billing and shipping slots; set the other slot instead",
            ));
        }
        self.put_slot(customer_id, slot, address_fields(a, false))
            .await?
            .ok_or_else(|| bad_request("the address is empty"))
    }

    /// Clears the slot; WooCommerce customers always keep both slots.
    async fn delete_address(&self, customer_id: &str, id: &str) -> Result<(), ConnectorError> {
        let slot = slot(id)?;
        self.put_slot(
            customer_id,
            slot,
            address_fields(&AddressPatch::default(), true),
        )
        .await
        .map(|_| ())
    }

    /// Without addresses, an order for a registered customer gets the
    /// customer's saved ones. `note` becomes a private order note.
    async fn create_order(&self, o: &NewOrder) -> Result<Order, ConnectorError> {
        let mut body = Map::new();
        let mut saved = None;
        if let Some(id) = &o.customer_id {
            body.insert("customer_id".into(), json!(numeric(id, "customer")?));
            if o.billing.is_none() || o.shipping.is_none() {
                saved = Some(self.customer(id).await?);
            }
        }
        let mut billing = match (&o.billing, &saved) {
            (Some(b), _) => address_fields(b, false),
            (None, Some(c)) => address_fields(&c.billing.as_patch(), false),
            (None, None) => Map::new(),
        };
        if let Some(email) = o.email.as_ref().or(saved.as_ref().and_then(|c| c.email())) {
            billing.insert("email".into(), json!(email.trim()));
        }
        if !billing.is_empty() {
            body.insert("billing".into(), Value::Object(billing));
        }
        let shipping = match (&o.shipping, &saved) {
            (Some(s), _) => Some(address_fields(s, false)),
            (None, Some(c)) => Some(address_fields(&c.shipping.as_patch(), false)),
            (None, None) => None,
        };
        if let Some(s) = shipping.filter(|s| !s.is_empty()) {
            body.insert("shipping".into(), Value::Object(s));
        }
        let lines = o
            .line_items
            .iter()
            .map(|l| {
                if l.quantity < 1 {
                    return Err(bad_request("line item quantities must be at least 1"));
                }
                let mut line = json!({
                    "product_id": numeric(&l.product_id, "product")?,
                    "quantity": l.quantity,
                });
                if let Some(price) = l.price {
                    // A line's subtotal and total override the catalog price.
                    let total = (price * rust_decimal::Decimal::from(l.quantity)).to_string();
                    line["subtotal"] = json!(total);
                    line["total"] = json!(total);
                }
                Ok(line)
            })
            .collect::<Result<Vec<_>, ConnectorError>>()?;
        body.insert("line_items".into(), Value::Array(lines));
        if let Some(status) = &o.status {
            body.insert("status".into(), json!(status.trim()));
        }
        let created: WooOrder = self.post("/orders", Value::Object(body)).await?;
        let order: Order = created.try_into()?;
        if let Some(note) = o.note.as_ref().filter(|n| !n.trim().is_empty()) {
            let note = OrderNote {
                note: note.clone(),
                notify_customer: false,
            };
            self.add_order_note(&order.id, &note).await?;
        }
        Ok(order)
    }

    /// A customer note is emailed to the customer by WooCommerce.
    async fn add_order_note(&self, id: &str, note: &OrderNote) -> Result<(), ConnectorError> {
        self.post::<Value>(
            &format!("/orders/{}/notes", path_id(id)?),
            json!({"note": note.note, "customer_note": note.notify_customer}),
        )
        .await
        .map(|_| ())
    }

    async fn delete_order(&self, id: &str) -> Result<(), ConnectorError> {
        self.delete(&format!("/orders/{}", path_id(id)?)).await
    }

    async fn list_coupons(&self, q: &CouponQuery) -> Result<Page<Coupon>, ConnectorError> {
        let mut params = Vec::new();
        opt(&mut params, "code", &q.code);
        self.list::<WooCoupon, _>("/coupons", params, q.page, q.per_page)
            .await
    }

    async fn create_coupon(&self, c: &CouponPatch) -> Result<Coupon, ConnectorError> {
        let created: WooCoupon = self.post("/coupons", coupon_body(c)?).await?;
        created.try_into()
    }

    async fn update_coupon(&self, id: &str, c: &CouponPatch) -> Result<Coupon, ConnectorError> {
        let updated: WooCoupon = self
            .put(&format!("/coupons/{}", path_id(id)?), coupon_body(c)?)
            .await?;
        updated.try_into()
    }

    async fn delete_coupon(&self, id: &str) -> Result<(), ConnectorError> {
        self.delete(&format!("/coupons/{}", path_id(id)?)).await
    }

    /// Any wc/v3 route, signed like the other calls. Error statuses come back
    /// as responses, so the model can read WooCommerce's message.
    async fn raw(&self, req: &RawRequest) -> Result<RawResponse, ConnectorError> {
        let method = raw_method(&req.method)?;
        let path = format!("/{}", req.path.trim().trim_start_matches('/'));
        let params = req
            .query
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut http = self
            .auth
            .apply(method, &format!("{}{path}", self.api), params);
        if method != Method::Get {
            if let Some(body) = &req.body {
                let body = match body {
                    Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| body.clone()),
                    other => other.clone(),
                };
                http = http.json(&body);
            }
        }
        let resp = self.transport.send(http).await?;
        let body = serde_json::from_str(&resp.body).unwrap_or(Value::String(resp.body));
        Ok(RawResponse {
            status: resp.status,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{FakeTransport, HttpResponse};
    use chrono::NaiveDate;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/woocommerce/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn woo(t: &Arc<FakeTransport>) -> WooCommerce {
        let creds = Credentials::from([
            ("consumer_key".into(), "ck_x".into()),
            ("consumer_secret".into(), "cs_x".into()),
        ]);
        WooCommerce::new("https://shop.test/", &creds, t.clone())
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[tokio::test]
    async fn list_products_maps_fields_and_pagination() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/wp-json/wc/v3/products",
            HttpResponse::new(200, fixture("products.json"))
                .with_header("X-WP-Total", "23")
                .with_header("X-WP-TotalPages", "12"),
        );
        let page = woo(&t)
            .list_products(&ProductQuery {
                search: Some("hoodie".into()),
                per_page: Some(2),
                page: Some(3),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            (page.page, page.total, page.total_pages),
            (3, Some(23), Some(12))
        );
        let p = &page.items[0];
        assert_eq!(p.id, "794");
        assert_eq!(p.sku.as_deref(), Some("HOOD-BLU"));
        assert_eq!(p.price, Some(dec("42.00")));
        assert_eq!(p.sale_price, None, "empty string means unset");
        assert_eq!(p.stock_quantity, Some(7));
        assert_eq!(page.items[1].sku, None);

        let req = &t.requests()[0];
        let q = req.query_pairs();
        assert!(q.contains(&("search".into(), "hoodie".into())));
        assert!(q.contains(&("page".into(), "3".into())));
        assert!(q.contains(&("per_page".into(), "2".into())));
        assert!(req
            .header_value("authorization")
            .unwrap()
            .starts_with("Basic "));
    }

    #[tokio::test]
    async fn update_product_sends_put_with_patch() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Put,
            "/wp-json/wc/v3/products/794",
            HttpResponse::new(200, fixture("product.json")),
        );
        let p = woo(&t)
            .update_product(
                "794",
                &ProductPatch {
                    regular_price: Some(dec("39.90")),
                    stock_quantity: Some(5),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(p.name, "Blue Hoodie");
        let body: Value = serde_json::from_str(t.requests()[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"regular_price": "39.90", "manage_stock": true, "stock_quantity": 5})
        );
    }

    #[tokio::test]
    async fn invalid_credentials_map_to_auth_error() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/wp-json/wc/v3/products",
            HttpResponse::new(
                401,
                r#"{"code":"woocommerce_rest_authentication_error","message":"Invalid signature - provided signature does not match.","data":{"status":401}}"#,
            ),
        );
        let err = woo(&t).verify().await.unwrap_err();
        assert_eq!(
            err,
            ConnectorError::Auth("Invalid signature - provided signature does not match.".into())
        );
    }

    #[tokio::test]
    async fn orders_map_guest_customers_and_date_filters_are_inclusive() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/wp-json/wc/v3/orders",
            HttpResponse::new(200, fixture("orders.json")),
        );
        let page = woo(&t)
            .list_orders(&OrderQuery {
                status: Some("processing".into()),
                after: NaiveDate::from_ymd_opt(2026, 10, 1),
                before: NaiveDate::from_ymd_opt(2026, 10, 16),
                ..Default::default()
            })
            .await
            .unwrap();
        let o = &page.items[0];
        assert_eq!((o.id.as_str(), o.number.as_str()), ("727", "727"));
        assert_eq!(o.total, dec("84.50"));
        assert_eq!(o.customer_name.as_deref(), Some("Ana García"));
        assert_eq!(o.customer_id, None, "guest checkout");
        assert_eq!(o.line_items[0].quantity, 2);
        assert_eq!(page.items[1].customer_id.as_deref(), Some("12"));

        let q = t.requests()[0].query_pairs();
        assert!(q.contains(&("after".into(), "2026-10-01T00:00:00".into())));
        assert!(q.contains(&("before".into(), "2026-10-17T00:00:00".into())));
        assert!(q.contains(&("status".into(), "processing".into())));
    }

    #[tokio::test]
    async fn sales_report_parses_string_and_number_totals() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/wp-json/wc/v3/reports/sales",
            HttpResponse::new(200, fixture("sales_report.json")),
        );
        let range = DateRange {
            from: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            to: NaiveDate::from_ymd_opt(2026, 10, 16).unwrap(),
        };
        let r = woo(&t).sales_report(&range).await.unwrap();
        assert_eq!(r.total_sales, dec("1234.50"));
        assert_eq!(r.net_sales, dec("1100.00"));
        assert_eq!((r.orders, r.items), (17, 31));
        let q = t.requests()[0].query_pairs();
        assert!(q.contains(&("date_min".into(), "2026-10-01".into())));
    }

    #[tokio::test]
    async fn non_numeric_ids_never_reach_the_url() {
        let t = Arc::new(FakeTransport::new());
        let err = woo(&t).get_order("1/../../users").await.unwrap_err();
        assert!(matches!(err, ConnectorError::NotFound(_)));
        assert!(t.requests().is_empty());
    }

    #[tokio::test]
    async fn customers_and_server_errors() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Get,
            "/wp-json/wc/v3/customers",
            HttpResponse::new(200, fixture("customers.json")),
        );
        t.on(
            Method::Get,
            "/wp-json/wc/v3/orders/9",
            HttpResponse::new(500, "<html>Fatal error</html>"),
        );
        let w = woo(&t);
        let c = &w
            .list_customers(&CustomerQuery::default())
            .await
            .unwrap()
            .items[0];
        assert_eq!((c.id.as_str(), c.email.as_str()), ("12", "jo@example.com"));
        assert_eq!(c.total_spent, None);
        let err = w.get_order("9").await.unwrap_err();
        assert!(matches!(err, ConnectorError::Api { status: 500, .. }));
    }

    fn ok(t: &FakeTransport, method: Method, path: &str, body: Value) {
        t.on(method, path, HttpResponse::new(200, body.to_string()));
    }

    fn body(req: &crate::transport::HttpRequest) -> Value {
        serde_json::from_str(req.body.as_deref().unwrap()).unwrap()
    }

    fn customer_json() -> Value {
        json!({
            "id": 12, "email": "jo@example.com", "first_name": "Jo", "last_name": "Smith",
            "billing": {"first_name": "Jo", "last_name": "Smith", "address_1": "Calle Mayor 1",
                "city": "Madrid", "postcode": "28013", "country": "ES", "phone": "+34600",
                "email": "jo@example.com", "company": "", "address_2": "", "state": ""},
            "shipping": {"first_name": "", "last_name": "", "address_1": "", "city": "",
                "postcode": "", "country": "", "phone": "", "company": "", "address_2": "", "state": ""}
        })
    }

    #[tokio::test]
    async fn create_product_sends_categories_and_ends_sales_with_an_empty_price() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/wp-json/wc/v3/products",
            HttpResponse::new(201, fixture("product.json")),
        );
        let w = woo(&t);
        w.create_product(&ProductPatch {
            name: Some("Mug".into()),
            sale_price: Some(Decimal::ZERO),
            description: Some("<p>Big</p>".into()),
            category_ids: Some(vec!["15".into(), "16".into()]),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(
            body(&t.requests()[0]),
            json!({"name": "Mug", "type": "simple", "sale_price": "", "description": "<p>Big</p>",
                "categories": [{"id": 15}, {"id": 16}]})
        );
        let bad = w
            .create_product(&ProductPatch {
                name: Some("Mug".into()),
                category_ids: Some(vec!["shoes".into()]),
                ..Default::default()
            })
            .await;
        assert!(matches!(bad, Err(ConnectorError::Api { status: 400, .. })));
        assert_eq!(t.requests().len(), 1, "nothing sent for a bad category id");
    }

    #[tokio::test]
    async fn deletes_are_forced() {
        let t = Arc::new(FakeTransport::new());
        for path in [
            "products/5",
            "products/categories/6",
            "customers/7",
            "orders/8",
            "coupons/9",
        ] {
            ok(
                &t,
                Method::Delete,
                &format!("/wp-json/wc/v3/{path}"),
                json!({"id": 1}),
            );
        }
        let w = woo(&t);
        w.delete_product("5").await.unwrap();
        w.delete_category("6").await.unwrap();
        w.delete_customer("7").await.unwrap();
        w.delete_order("8").await.unwrap();
        w.delete_coupon("9").await.unwrap();
        for req in t.requests() {
            assert_eq!(req.method, Method::Delete);
            assert!(
                req.query_pairs().contains(&("force".into(), "true".into())),
                "{}",
                req.url
            );
        }
    }

    #[tokio::test]
    async fn categories_map_top_level_and_bodies_take_numeric_parents() {
        let t = Arc::new(FakeTransport::new());
        ok(
            &t,
            Method::Get,
            "/wp-json/wc/v3/products/categories",
            json!([{"id": 15, "name": "Mugs", "parent": 0, "description": "", "count": 3},
                {"id": 16, "name": "Big mugs", "parent": 15, "description": "Large", "count": 0}]),
        );
        ok(
            &t,
            Method::Put,
            "/wp-json/wc/v3/products/categories/16",
            json!({"id": 16, "name": "Big mugs", "parent": 0}),
        );
        let w = woo(&t);
        let page = w.list_categories(&CategoryQuery::default()).await.unwrap();
        assert_eq!(page.items[0].parent_id, None);
        assert_eq!(page.items[0].description, None);
        assert_eq!(page.items[1].parent_id.as_deref(), Some("15"));
        assert_eq!(page.items[0].count, Some(3));
        w.update_category(
            "16",
            &CategoryPatch {
                parent_id: Some(String::new()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(body(&t.requests()[1]), json!({"parent": 0}));
    }

    #[tokio::test]
    async fn customers_carry_the_billing_phone_and_refuse_consent() {
        let t = Arc::new(FakeTransport::new());
        ok(
            &t,
            Method::Get,
            "/wp-json/wc/v3/customers/12",
            customer_json(),
        );
        let w = woo(&t);
        let c = w.get_customer("12").await.unwrap();
        assert_eq!(c.phone.as_deref(), Some("+34600"));
        assert_eq!(c.accepts_marketing, None);
        let consent = w
            .create_customer(&CustomerPatch {
                email: Some("a@b.c".into()),
                accepts_marketing: Some(true),
                ..Default::default()
            })
            .await;
        assert!(matches!(consent, Err(ConnectorError::Unsupported(_))));
        assert_eq!(t.requests().len(), 1);
    }

    #[tokio::test]
    async fn addresses_are_the_billing_and_shipping_slots() {
        let t = Arc::new(FakeTransport::new());
        ok(
            &t,
            Method::Get,
            "/wp-json/wc/v3/customers/12",
            customer_json(),
        );
        ok(
            &t,
            Method::Put,
            "/wp-json/wc/v3/customers/12",
            customer_json(),
        );
        let w = woo(&t);
        let list = w.list_addresses("12").await.unwrap();
        assert_eq!(list.len(), 1, "the empty shipping slot is left out");
        assert_eq!(
            (list[0].id.as_str(), list[0].country.as_deref()),
            ("billing", Some("ES"))
        );

        let no_kind = w
            .create_address(
                "12",
                &AddressPatch {
                    city: Some("Madrid".into()),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(
            no_kind,
            Err(ConnectorError::Api { status: 400, .. })
        ));
        let moved = w
            .update_address(
                "12",
                "billing",
                &AddressPatch {
                    kind: Some("shipping".into()),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(
            moved,
            Err(ConnectorError::Api { status: 400, .. })
        ));

        w.create_address(
            "12",
            &AddressPatch {
                kind: Some("billing".into()),
                city: Some("Madrid".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        w.update_address(
            "12",
            "billing",
            &AddressPatch {
                postcode: Some("28001".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        w.delete_address("12", "shipping").await.unwrap();
        let puts: Vec<Value> = t
            .requests()
            .iter()
            .filter(|r| r.method == Method::Put)
            .map(body)
            .collect();
        assert_eq!(puts[0]["billing"]["city"], "Madrid");
        assert_eq!(
            puts[0]["billing"]["address_1"], "",
            "create replaces the slot"
        );
        assert_eq!(puts[1], json!({"billing": {"postcode": "28001"}}));
        assert_eq!(puts[2]["shipping"]["address_1"], "");
        assert_eq!(puts[2]["shipping"].as_object().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn orders_for_a_customer_copy_saved_addresses_and_add_the_note_privately() {
        let t = Arc::new(FakeTransport::new());
        ok(
            &t,
            Method::Get,
            "/wp-json/wc/v3/customers/12",
            customer_json(),
        );
        t.on(
            Method::Post,
            "/wp-json/wc/v3/orders",
            HttpResponse::new(
                201,
                json!({"id": 900, "status": "pending", "total": "20.00",
                "currency": "EUR", "customer_id": 12, "billing": {"email": "jo@example.com"},
                "line_items": []})
                .to_string(),
            ),
        );
        t.on(
            Method::Post,
            "/wp-json/wc/v3/orders/900/notes",
            HttpResponse::new(201, "{}"),
        );
        let w = woo(&t);
        let o = w
            .create_order(&NewOrder {
                customer_id: Some("12".into()),
                line_items: vec![NewLineItem {
                    product_id: "794".into(),
                    quantity: 2,
                    price: Some(dec("10.00")),
                }],
                note: Some("Phone order".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(o.id, "900");
        let reqs = t.requests();
        let order = body(&reqs[1]);
        assert_eq!(order["customer_id"], 12);
        assert_eq!(order["billing"]["email"], "jo@example.com");
        assert_eq!(order["billing"]["address_1"], "Calle Mayor 1");
        assert!(
            order.get("shipping").is_none(),
            "an empty saved slot is not copied"
        );
        assert_eq!(
            order["line_items"],
            json!([{"product_id": 794, "quantity": 2, "subtotal": "20.00", "total": "20.00"}])
        );
        assert!(order.get("status").is_none());
        assert_eq!(
            body(&reqs[2]),
            json!({"note": "Phone order", "customer_note": false})
        );

        w.add_order_note(
            "900",
            &OrderNote {
                note: "Shipped today".into(),
                notify_customer: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            body(&t.requests()[3]),
            json!({"note": "Shipped today", "customer_note": true})
        );
    }

    #[tokio::test]
    async fn coupons_map_drafts_and_validate_kinds() {
        let t = Arc::new(FakeTransport::new());
        ok(
            &t,
            Method::Post,
            "/wp-json/wc/v3/coupons",
            json!({"id": 40, "code": "sale10", "discount_type": "percent", "amount": "10.00",
                "date_expires": "2030-01-31T00:00:00", "usage_limit": 100, "usage_count": 2,
                "minimum_amount": "0.00", "status": "draft", "description": ""}),
        );
        let w = woo(&t);
        let c = w
            .create_coupon(&CouponPatch {
                code: Some("SALE10".into()),
                kind: Some("percent".into()),
                amount: Some(dec("10")),
                expires_on: chrono::NaiveDate::from_ymd_opt(2030, 1, 31),
                active: Some(false),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(c.active, Some(false));
        assert_eq!(c.minimum_amount, None, "0.00 means no minimum");
        assert_eq!(c.expires_on, chrono::NaiveDate::from_ymd_opt(2030, 1, 31));
        assert_eq!((c.usage_count, c.description), (Some(2), None));
        assert_eq!(
            body(&t.requests()[0]),
            json!({"code": "SALE10", "discount_type": "percent", "amount": "10",
                "date_expires": "2030-01-31", "status": "draft"})
        );
        let bad = w
            .create_coupon(&CouponPatch {
                code: Some("X".into()),
                kind: Some("bogo".into()),
                amount: Some(dec("1")),
                ..Default::default()
            })
            .await;
        assert!(matches!(bad, Err(ConnectorError::Api { status: 400, .. })));
        assert_eq!(t.requests().len(), 1);
    }

    #[tokio::test]
    async fn raw_requests_reuse_auth_and_return_error_statuses() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/wp-json/wc/v3/products/tags",
            HttpResponse::new(400, r#"{"code":"term_exists","message":"exists"}"#),
        );
        t.on(
            Method::Get,
            "/wp-json/wc/v3/system_status",
            HttpResponse::new(200, "not json"),
        );
        let w = woo(&t);
        let created = w
            .raw(&RawRequest {
                method: "post".into(),
                path: "products/tags".into(),
                query: [("context".to_string(), "edit".to_string())].into(),
                body: Some(json!(r#"{"name": "red"}"#)),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(created.status, 400);
        assert_eq!(created.body["code"], "term_exists");
        let req = &t.requests()[0];
        assert!(req
            .header_value("authorization")
            .unwrap()
            .starts_with("Basic "));
        assert!(req
            .query_pairs()
            .contains(&("context".into(), "edit".into())));
        assert_eq!(
            body(req),
            json!({"name": "red"}),
            "a JSON string body is sent as JSON"
        );

        let text = w
            .raw(&RawRequest {
                method: "GET".into(),
                path: "/system_status".into(),
                body: Some(json!({"ignored": true})),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(text.body, json!("not json"));
        assert!(t.requests()[1].body.is_none(), "GET sends no body");
        assert!(w.hidden().contains(&"shop_list_abandoned_carts"));
    }
}
