//! Orders created, annotated or deleted, cart rules (coupons), abandoned
//! carts and raw webservice requests.

use std::collections::{BTreeSet, HashMap};

use chrono::Local;
use rust_decimal::Decimal;
use serde_json::Value;

use super::catalog::line_total;
use super::xml::{self, Field};
use super::{bad_request, list, literal, numeric_id, one_of, paging, types, Params, PrestaShop};
use crate::domain::*;
use crate::shop::unsupported;
use crate::transport::Method;
use crate::{ConnectorError, Platform, ShopConnector};

const COUPON_FIELDS: &str = "[id,code,name,description,reduction_percent,reduction_amount,free_shipping,date_to,quantity,minimum_amount,active]";
const NO_LIMIT: &str = "999999";
/// Used for coupons without an end date: PrestaShop requires one.
const FAR_FUTURE: &str = "2099-12-31 23:59:59";
const MANUAL_PAYMENT: &str = "Manual order (merchantGPT)";

/// `"12"` or `"12:34"` (product 12, combination 34).
fn product_ref(id: &str) -> Result<(String, String), ConnectorError> {
    let (p, c) = id.trim().split_once(':').unwrap_or((id.trim(), "0"));
    Ok((
        numeric_id("product", p)?.to_string(),
        numeric_id("combination", c)?.to_string(),
    ))
}

impl PrestaShop {
    async fn first_id(
        &self,
        resource: &str,
        params: Params,
    ) -> Result<Option<String>, ConnectorError> {
        let v = self.get(resource, params).await?;
        Ok(list(&v, resource)
            .first()
            .and_then(|r| types::string(&r["id"])))
    }

    /// Orders through the webservice are recorded by a payment module; the
    /// one that recorded the latest order is known to be installed.
    async fn payment_module(&self) -> String {
        let params = vec![
            ("display".into(), "[id,module]".into()),
            ("sort".into(), "[id_DESC]".into()),
            ("limit".into(), "1".into()),
        ];
        self.get("orders", params)
            .await
            .ok()
            .and_then(|v| {
                list(&v, "orders")
                    .first()
                    .and_then(|o| types::string(&o["module"]))
            })
            .unwrap_or_else(|| "ps_wirepayment".into())
    }

    async fn order_customer(&self, o: &NewOrder) -> Result<String, ConnectorError> {
        if let Some(c) = &o.customer_id {
            return Ok(numeric_id("customer", c)?.to_string());
        }
        let email = o
            .email
            .as_deref()
            .filter(|e| !e.trim().is_empty())
            .ok_or_else(|| bad_request("customer_id or email is required"))?;
        if let Some(id) = self.customer_by_email(email).await? {
            return Ok(id);
        }
        let named = o.billing.as_ref().or(o.shipping.as_ref());
        let customer = self
            .create_customer_in(&CustomerPatch {
                email: Some(email.into()),
                first_name: named.and_then(|a| a.first_name.clone()),
                last_name: named.and_then(|a| a.last_name.clone()),
                ..Default::default()
            })
            .await?;
        Ok(customer.id)
    }

    /// The price of one unit as the customer pays it at that address.
    async fn unit_price(
        &self,
        product: &str,
        combination: &str,
        quantity: i64,
        place: &Value,
    ) -> Result<Decimal, ConnectorError> {
        let mut params: Params = vec![
            ("display".into(), "[id,cache_default_attribute,pay]".into()),
            ("filter[id]".into(), format!("[{product}]")),
            ("price[pay][use_tax]".into(), "1".into()),
            ("price[pay][use_reduction]".into(), "1".into()),
            ("price[pay][quantity]".into(), quantity.to_string()),
            ("price[pay][product_attribute]".into(), combination.into()),
        ];
        for (param, key) in [
            ("country", "id_country"),
            ("state", "id_state"),
            ("postcode", "postcode"),
        ] {
            if let Some(v) = types::string(&place[key]).filter(|v| v != "0") {
                params.push((format!("price[pay][{param}]"), v));
            }
        }
        let row = list(&self.get("products", params).await?, "products")
            .into_iter()
            .next()
            .ok_or_else(|| ConnectorError::NotFound(format!("product {product}")))?;
        if combination == "0" && types::int(&row["cache_default_attribute"]).unwrap_or(0) > 0 {
            let combos = self
                .get(
                    "combinations",
                    vec![
                        ("display".into(), "[id,reference]".into()),
                        ("filter[id_product]".into(), format!("[{product}]")),
                    ],
                )
                .await?;
            let ids: Vec<String> = list(&combos, "combinations")
                .iter()
                .filter_map(|c| {
                    let id = types::string(&c["id"])?;
                    Some(match types::string(&c["reference"]) {
                        Some(r) => format!("{product}:{id} ({r})"),
                        None => format!("{product}:{id}"),
                    })
                })
                .collect();
            return Err(bad_request(format!(
                "product {product} has combinations; use product_id \"product:combination\", one of: {}",
                ids.join(", ")
            )));
        }
        Ok(types::decimal(&row["pay"])?.unwrap_or_default())
    }

    /// A cart, then the order PrestaShop records from it. The webservice
    /// records it through a payment module as paid ("Remote payment
    /// accepted") and only if the amount equals the cart total, so the total
    /// is computed from PrestaShop's own prices and the order uses the
    /// shop's free carrier (no shipping to compute).
    pub(super) async fn create_order_in(&self, o: &NewOrder) -> Result<Order, ConnectorError> {
        if o.line_items.iter().any(|l| l.price.is_some()) {
            return Err(unsupported(
                Platform::PrestaShop,
                "custom line prices; orders use catalog prices (set a sale price first)",
            ));
        }
        let lines: Vec<(String, String, i64)> = o
            .line_items
            .iter()
            .map(|l| {
                if l.quantity < 1 {
                    return Err(bad_request("quantities must be at least 1"));
                }
                let (p, c) = product_ref(&l.product_id)?;
                Ok((p, c, l.quantity))
            })
            .collect::<Result<_, _>>()?;
        let carrier = self
            .first_id(
                "carriers",
                vec![
                    ("display".into(), "[id]".into()),
                    ("filter[is_free]".into(), "[1]".into()),
                    ("filter[active]".into(), "[1]".into()),
                    ("filter[deleted]".into(), "[0]".into()),
                ],
            )
            .await?
            .ok_or_else(|| {
                unsupported(
                    Platform::PrestaShop,
                    "creating orders without an active free carrier (such as click and collect), because the webservice cannot compute shipping",
                )
            })?;
        let customer = self.order_customer(o).await?;
        let delivery = match o.shipping.as_ref().or(o.billing.as_ref()) {
            Some(a) => self.create_address_for(&customer, a).await?.id,
            None => self
                .addresses_of(&customer)
                .await?
                .into_iter()
                .next()
                .map(|a| a.id)
                .ok_or_else(|| {
                    bad_request("the customer has no address; give a shipping address")
                })?,
        };
        let invoice = match (&o.billing, &o.shipping) {
            (Some(b), Some(_)) => self.create_address_for(&customer, b).await?.id,
            _ => delivery.clone(),
        };
        let place = self.address_row(&customer, &delivery).await?;
        let mut total = Decimal::ZERO;
        for (p, c, q) in &lines {
            total += line_total(self.unit_price(p, c, *q, &place).await?, *q);
        }
        let currency = self
            .first_id(
                "currencies",
                vec![
                    ("display".into(), "[id]".into()),
                    ("filter[active]".into(), "[1]".into()),
                    ("filter[deleted]".into(), "[0]".into()),
                ],
            )
            .await?
            .unwrap_or_else(|| "1".into());
        let lang = self.languages().await?[0].to_string();
        let rows: Vec<Vec<(&str, String)>> = lines
            .iter()
            .map(|(p, c, q)| {
                vec![
                    ("id_product", p.clone()),
                    ("id_product_attribute", c.clone()),
                    ("id_address_delivery", delivery.clone()),
                    ("quantity", q.to_string()),
                ]
            })
            .collect();
        let common = |extra: Vec<(&'static str, Field)>| {
            let mut f = vec![
                ("id_customer", Field::Text(customer.clone())),
                ("id_address_delivery", Field::Text(delivery.clone())),
                ("id_address_invoice", Field::Text(invoice.clone())),
                ("id_currency", Field::Text(currency.clone())),
                ("id_lang", Field::Text(lang.clone())),
                ("id_carrier", Field::Text(carrier.clone())),
            ];
            f.extend(extra);
            f
        };
        let cart = self
            .post(
                "carts",
                xml::document(
                    "cart",
                    &common(vec![(
                        "associations",
                        Field::Xml(xml::rows("cart_rows", "cart_row", &rows)),
                    )]),
                ),
            )
            .await?;
        let cart_id = Self::created_id(&cart, "cart")?;
        let amount = total.to_string();
        let order_fields = common(vec![
            ("id_cart", Field::Text(cart_id.clone())),
            ("module", Field::Text(self.payment_module().await)),
            ("payment", Field::Text(MANUAL_PAYMENT.into())),
            ("total_paid", Field::Text(amount.clone())),
            ("total_paid_real", Field::Text(amount.clone())),
            ("total_products", Field::Text("0".into())),
            ("total_products_wt", Field::Text("0".into())),
            ("conversion_rate", Field::Text("1".into())),
        ]);
        let created = self
            .post("orders", xml::document("order", &order_fields))
            .await?;
        let id = types::string(&created["order"]["id"]).ok_or_else(|| {
            ConnectorError::InvalidResponse("PrestaShop did not return the new order".into())
        })?;
        let mut order = self.get_order(&id).await?;
        if order.status.to_lowercase().contains("error") {
            return Err(ConnectorError::Api {
                status: 409,
                message: format!(
                    "PrestaShop recorded order {} ({}) with a payment error: its cart total differs from {amount}; check the order in the back office",
                    order.id, order.number
                ),
            });
        }
        if let Some(note) = o.note.as_deref().filter(|n| !n.trim().is_empty()) {
            self.add_note(&order.id, note).await?;
        }
        if let Some(status) = o.status.as_deref().filter(|s| !s.trim().is_empty()) {
            order = self.update_order_status(&order.id, status).await?;
        }
        Ok(order)
    }

    /// Appends to the order's private note, which only the merchant sees.
    pub(super) async fn add_note(&self, id: &str, note: &str) -> Result<(), ConnectorError> {
        let id = numeric_id("order", id)?;
        let v = self
            .get(
                "orders",
                vec![
                    ("display".into(), "[id,note]".into()),
                    ("filter[id]".into(), format!("[{id}]")),
                ],
            )
            .await?;
        let row = list(&v, "orders")
            .into_iter()
            .next()
            .ok_or_else(|| ConnectorError::NotFound(format!("order {id}")))?;
        let note = match types::string(&row["note"]).filter(|n| !n.trim().is_empty()) {
            Some(current) => format!("{current}\n{}", note.trim()),
            None => note.trim().to_string(),
        };
        let doc = xml::document(
            "order",
            &[("id", Field::Text(id.into())), ("note", Field::Text(note))],
        );
        self.patch(&format!("orders/{id}"), doc).await.map(|_| ())
    }

    pub(super) async fn order_note(
        &self,
        id: &str,
        note: &OrderNote,
    ) -> Result<(), ConnectorError> {
        if note.notify_customer {
            return Err(unsupported(
                Platform::PrestaShop,
                "emailing a note to the customer (its webservice cannot send email); add it as a private note, and send the message with an email connector such as SMTP or Brevo",
            ));
        }
        self.add_note(id, &note.note).await
    }

    pub(super) async fn delete_order_by_id(&self, id: &str) -> Result<(), ConnectorError> {
        let id = numeric_id("order", id)?;
        self.delete(&format!("orders/{id}")).await
    }

    pub(super) async fn coupons_page(
        &self,
        q: &CouponQuery,
    ) -> Result<Page<Coupon>, ConnectorError> {
        let mut params: Params = vec![
            ("display".into(), COUPON_FIELDS.into()),
            ("sort".into(), "[id_DESC]".into()),
        ];
        if let Some(c) = q.code.as_deref().map(literal).filter(|c| !c.is_empty()) {
            params.push(("filter[code]".into(), format!("[{c}]")));
        }
        let (n, size) = paging(&mut params, q.page, q.per_page, super::MAX_PER_PAGE);
        let items = list(&self.get("cart_rules", params).await?, "cart_rules")
            .iter()
            .map(types::coupon)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(super::page(items, n, size))
    }

    async fn coupon_by_id(&self, id: &str) -> Result<Coupon, ConnectorError> {
        let id = numeric_id("coupon", id)?;
        let v = self
            .get(
                "cart_rules",
                vec![
                    ("display".into(), COUPON_FIELDS.into()),
                    ("filter[id]".into(), format!("[{id}]")),
                ],
            )
            .await?;
        list(&v, "cart_rules")
            .first()
            .map(types::coupon)
            .transpose()?
            .ok_or_else(|| ConnectorError::NotFound(format!("coupon {id}")))
    }

    /// Fields for the discount: `reduction_percent` or a tax-included
    /// `reduction_amount`; the other is reset.
    fn discount_fields(
        kind: &str,
        amount: Decimal,
    ) -> Result<Vec<(&'static str, Field)>, ConnectorError> {
        match kind {
            "percent" => Ok(vec![
                ("reduction_percent", Field::Text(amount.to_string())),
                ("reduction_amount", Field::Text("0".into())),
            ]),
            "fixed_cart" => Ok(vec![
                ("reduction_amount", Field::Text(amount.to_string())),
                ("reduction_tax", Field::Text("1".into())),
                ("reduction_percent", Field::Text("0".into())),
            ]),
            "fixed_product" => Err(unsupported(
                Platform::PrestaShop,
                "fixed_product coupons; its cart rules take a percent or an amount off the order (use percent or fixed_cart)",
            )),
            other => Err(bad_request(format!(
                "unknown coupon kind {other:?}; use percent or fixed_cart"
            ))),
        }
    }

    fn coupon_common(c: &CouponPatch) -> Vec<(&'static str, Field)> {
        let mut f = Vec::new();
        if let Some(code) = &c.code {
            f.push(("code", Field::Text(code.trim().into())));
        }
        if let Some(d) = c.expires_on {
            f.push(("date_to", Field::Text(format!("{d} 23:59:59"))));
        }
        if let Some(l) = c.usage_limit {
            f.push(("quantity", Field::Text(l.to_string())));
        }
        if let Some(m) = c.minimum_amount {
            f.push(("minimum_amount", Field::Text(m.to_string())));
            f.push(("minimum_amount_tax", Field::Text("1".into())));
        }
        if let Some(a) = c.active {
            f.push(("active", Field::Text(u8::from(a).to_string())));
        }
        if let Some(d) = &c.description {
            f.push(("description", Field::Text(d.clone())));
        }
        f
    }

    pub(super) async fn create_coupon_in(&self, c: &CouponPatch) -> Result<Coupon, ConnectorError> {
        let code = c.code.as_deref().unwrap_or_default().trim().to_string();
        let mut fields = vec![(
            "name",
            self.every_language(c.description.as_deref().unwrap_or(&code))
                .await?,
        )];
        fields.extend(Self::discount_fields(
            c.kind.as_deref().unwrap_or("percent"),
            c.amount.unwrap_or_default(),
        )?);
        fields.extend(Self::coupon_common(c));
        let today = Local::now().date_naive();
        fields.push(("date_from", Field::Text(format!("{today} 00:00:00"))));
        if c.expires_on.is_none() {
            fields.push(("date_to", Field::Text(FAR_FUTURE.into())));
        }
        if c.usage_limit.is_none() {
            fields.push(("quantity", Field::Text(NO_LIMIT.into())));
        }
        fields.push(("quantity_per_user", Field::Text(NO_LIMIT.into())));
        if c.active.is_none() {
            fields.push(("active", Field::Text("1".into())));
        }
        // No leftover voucher when the discount exceeds the order.
        fields.push(("partial_use", Field::Text("0".into())));
        let v = self
            .post("cart_rules", xml::document("cart_rule", &fields))
            .await?;
        let id = Self::created_id(&v, "cart_rule")?;
        self.coupon_by_id(&id).await
    }

    pub(super) async fn update_coupon_in(
        &self,
        id: &str,
        c: &CouponPatch,
    ) -> Result<Coupon, ConnectorError> {
        let current = self.coupon_by_id(id).await?;
        let mut fields = vec![("id", Field::Text(current.id.clone()))];
        if c.kind.is_some() || c.amount.is_some() {
            fields.extend(Self::discount_fields(
                c.kind.as_deref().unwrap_or(&current.kind),
                c.amount.unwrap_or(current.amount),
            )?);
        }
        fields.extend(Self::coupon_common(c));
        self.patch(
            &format!("cart_rules/{}", current.id),
            xml::document("cart_rule", &fields),
        )
        .await?;
        self.coupon_by_id(&current.id).await
    }

    pub(super) async fn delete_coupon_by_id(&self, id: &str) -> Result<(), ConnectorError> {
        let id = numeric_id("coupon", id)?;
        self.delete(&format!("cart_rules/{id}")).await
    }

    /// Carts of known customers, with products, that never became an order
    /// and have been untouched for `ABANDONED_AFTER_HOURS`. PrestaShop keeps
    /// times in the shop's timezone, which is only readable with access to
    /// `configurations` (the shop's secrets too), so this computer's local
    /// time stands in for it. Totals use the shop's default prices including
    /// tax, without shipping.
    pub(super) async fn abandoned(
        &self,
        q: &CartQuery,
    ) -> Result<Page<AbandonedCart>, ConnectorError> {
        let idle_since =
            Local::now().naive_local() - chrono::Duration::hours(ABANDONED_AFTER_HOURS);
        let from = q.since.map_or("1970-01-01".to_string(), |d| d.to_string());
        let mut params: Params = vec![
            ("display".into(), "full".into()),
            ("filter[id_customer]".into(), "[1,999999999]".into()),
            (
                "filter[date_upd]".into(),
                format!(
                    "[{from} 00:00:00,{}]",
                    idle_since.format("%Y-%m-%d %H:%M:%S")
                ),
            ),
            ("date".into(), "1".into()),
            ("sort".into(), "[id_DESC]".into()),
        ];
        let (n, size) = paging(&mut params, q.page, q.per_page, super::MAX_PER_PAGE);
        let mut carts = list(&self.get("carts", params).await?, "carts");
        let more = carts.len() > size as usize;
        carts.truncate(size as usize);
        let rows = |c: &Value| {
            c["associations"]["cart_rows"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        };
        carts.retain(|c| !rows(c).is_empty());
        let ids: BTreeSet<String> = carts
            .iter()
            .filter_map(|c| types::string(&c["id"]))
            .collect();
        let ordered: BTreeSet<String> = if ids.is_empty() {
            BTreeSet::new()
        } else {
            let v = self
                .get(
                    "orders",
                    vec![
                        ("display".into(), "[id,id_cart]".into()),
                        ("filter[id_cart]".into(), one_of(ids)),
                    ],
                )
                .await?;
            list(&v, "orders")
                .iter()
                .filter_map(|o| types::string(&o["id_cart"]))
                .collect()
        };
        carts.retain(|c| !types::string(&c["id"]).is_some_and(|id| ordered.contains(&id)));
        let customers = self
            .customer_names(
                carts
                    .iter()
                    .filter_map(|c| types::string(&c["id_customer"]))
                    .collect(),
            )
            .await;
        let product_ids: BTreeSet<String> = carts
            .iter()
            .flat_map(rows)
            .filter_map(|r| types::string(&r["id_product"]))
            .collect();
        let mut products: HashMap<String, (String, Decimal)> = HashMap::new();
        if !product_ids.is_empty() {
            let v = self
                .get(
                    "products",
                    vec![
                        ("display".into(), "[id,name,pay]".into()),
                        ("filter[id]".into(), one_of(product_ids)),
                        ("price[pay][use_tax]".into(), "1".into()),
                        ("price[pay][use_reduction]".into(), "1".into()),
                    ],
                )
                .await?;
            for p in list(&v, "products") {
                if let Some(id) = types::string(&p["id"]) {
                    let name = types::text(&p["name"]).unwrap_or_default();
                    let price = types::decimal(&p["pay"])?.unwrap_or_default();
                    products.insert(id, (name, price));
                }
            }
        }
        let meta = self.meta().await?;
        let mut items = Vec::new();
        for c in &carts {
            let line_items: Vec<LineItem> = rows(c)
                .iter()
                .map(|r| {
                    let pid = types::string(&r["id_product"]).unwrap_or_default();
                    let qty = types::int(&r["quantity"]).unwrap_or(0);
                    let (name, price) = products.get(&pid).cloned().unwrap_or_default();
                    LineItem {
                        product_id: Some(pid),
                        name,
                        quantity: qty,
                        total: line_total(price, qty),
                    }
                })
                .collect();
            let customer =
                types::string(&c["id_customer"]).and_then(|id| customers.get(&id).cloned());
            items.push(AbandonedCart {
                id: types::string(&c["id"]).unwrap_or_default(),
                customer_id: types::string(&c["id_customer"]),
                email: customer
                    .as_ref()
                    .map(|c| c.2.clone())
                    .filter(|e| !e.is_empty()),
                customer_name: customer
                    .as_ref()
                    .map(|c| format!("{} {}", c.0, c.1).trim().to_string())
                    .filter(|n| !n.is_empty()),
                updated_at: types::string(&c["date_upd"]),
                total: Some(line_items.iter().map(|l| l.total).sum()),
                currency: types::string(&c["id_currency"])
                    .and_then(|id| meta.currencies.get(&id).cloned()),
                line_items,
                recovery_url: None,
            });
        }
        let seen = u64::from(n - 1) * u64::from(size) + items.len() as u64;
        Ok(Page {
            items,
            page: n,
            total_pages: (!more).then_some(n),
            total: (!more).then_some(seen),
        })
    }

    pub(super) async fn raw_request(
        &self,
        req: &RawRequest,
    ) -> Result<RawResponse, ConnectorError> {
        let method = match req.method.trim().to_ascii_uppercase().as_str() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "PATCH" => Method::Patch,
            "DELETE" => Method::Delete,
            other => return Err(bad_request(format!("unknown method {other}"))),
        };
        let body = match (&req.body, method) {
            (None, _) | (_, Method::Get) | (_, Method::Delete) => None,
            (Some(Value::String(s)), _) => Some(s.clone()),
            (Some(_), _) => {
                return Err(bad_request(
                    "PrestaShop writes take an XML string body, e.g. <prestashop><carrier>...</carrier></prestashop>",
                ))
            }
        };
        let params: Params = req.query.clone().into_iter().collect();
        let resp = self
            .exchange(method, &req.path, &params, body.as_deref())
            .await?;
        let body = serde_json::from_str(&resp.body).unwrap_or(Value::String(resp.body));
        Ok(RawResponse {
            status: resp.status,
            body,
        })
    }
}
