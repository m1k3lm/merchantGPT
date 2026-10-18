//! Order creation through Magento's cart flow, and abandoned carts.
//!
//! A customer order uses the admin cart endpoints (`/customers/{id}/carts`,
//! `/carts/{id}/...`, `PUT /carts/{id}/order`); a guest order uses the
//! anonymous `/guest-carts/{masked id}/...` endpoints and
//! `payment-information`, which takes the guest's email.

use serde_json::{json, Map, Value};

use super::criteria::Filter;
use super::{bad_request, entity_id, types, Magento};
use crate::domain::*;
use crate::transport::Method;
use crate::{ConnectorError, ShopConnector};

/// Offline payment methods tried in order; the first the store enables wins.
const PAYMENT_METHODS: [&str; 4] = ["checkmo", "banktransfer", "cashondelivery", "free"];

enum Cart {
    Customer(String),
    Guest(String),
}

impl Cart {
    fn prefix(&self) -> String {
        match self {
            Cart::Customer(id) => format!("/carts/{id}"),
            Cart::Guest(id) => format!("/guest-carts/{id}"),
        }
    }

    fn quote_id(&self) -> Value {
        match self {
            Cart::Customer(id) => json!(id.parse::<u64>().unwrap_or(0)),
            Cart::Guest(id) => json!(id),
        }
    }
}

impl Magento {
    /// Prices come from the catalog: Magento's REST API cannot override an
    /// item price. The first available shipping method and the first enabled
    /// offline payment method are used.
    pub(super) async fn create_order_impl(&self, o: &NewOrder) -> Result<Order, ConnectorError> {
        if o.line_items.iter().any(|l| l.price.is_some()) {
            return Err(bad_request(
                "Magento orders use catalog prices; change the product price or create a coupon instead",
            ));
        }
        if o.line_items.iter().any(|l| l.quantity < 1) {
            return Err(bad_request(
                "every line item needs a quantity of at least 1",
            ));
        }
        let (cart, email, default_address) = match o.customer_id.as_deref() {
            Some(id) => {
                let id = entity_id("customer", id)?;
                let customer = self.customer_raw(id).await?;
                let cart_id = self
                    .write(Method::Post, "", &format!("/customers/{id}/carts"), None)
                    .await?;
                let cart_id = types::string(&cart_id)
                    .ok_or_else(|| ConnectorError::InvalidResponse("no cart id".into()))?;
                // Magento reuses the customer's active cart, which may hold
                // what they were shopping for.
                let existing = self.get(&format!("/carts/{cart_id}"), &[]).await?;
                let held = existing["items_count"].as_u64().unwrap_or(0);
                if held > 0 {
                    return Err(bad_request(format!(
                        "this customer has {held} item(s) in their open cart, which Magento would add to the order; create the order in the Magento admin instead"
                    )));
                }
                let email = types::string(&customer["email"]);
                (Cart::Customer(cart_id), email, default_shipping(&customer))
            }
            None => {
                let email = o
                    .email
                    .clone()
                    .filter(|e| !e.trim().is_empty())
                    .ok_or_else(|| bad_request("a guest order needs an email or a customer_id"))?;
                let masked = self.write(Method::Post, "", "/guest-carts", None).await?;
                let masked = masked
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| ConnectorError::InvalidResponse("no guest cart id".into()))?;
                (Cart::Guest(masked), Some(email), None)
            }
        };
        let prefix = cart.prefix();
        for item in &o.line_items {
            self.write(
                Method::Post,
                "",
                &format!("{prefix}/items"),
                Some(json!({"cartItem": {
                    "sku": item.product_id.trim(),
                    "qty": item.quantity,
                    "quote_id": cart.quote_id()
                }})),
            )
            .await?;
        }
        let shipping = match o.shipping.as_ref().or(o.billing.as_ref()) {
            Some(a) => self.quote_address(a, email.as_deref()).await?,
            None => default_address
                .map(|a| from_customer_address(&a, email.as_deref()))
                .ok_or_else(|| bad_request("the order needs a shipping address"))?,
        };
        let billing = match o.billing.as_ref() {
            Some(a) => self.quote_address(a, email.as_deref()).await?,
            None => shipping.clone(),
        };
        let methods = self
            .write(
                Method::Post,
                "",
                &format!("{prefix}/estimate-shipping-methods"),
                Some(json!({ "address": shipping })),
            )
            .await?;
        let method = methods
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["available"] != false)
            .ok_or_else(|| bad_request("Magento offers no shipping method for this address"))?;
        let info = self
            .write(
                Method::Post,
                "",
                &format!("{prefix}/shipping-information"),
                Some(json!({"addressInformation": {
                    "shipping_address": shipping,
                    "billing_address": billing,
                    "shipping_carrier_code": method["carrier_code"],
                    "shipping_method_code": method["method_code"]
                }})),
            )
            .await?;
        let offered: Vec<String> = info["payment_methods"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| types::string(&p["code"]))
            .collect();
        let payment = PAYMENT_METHODS
            .iter()
            .find(|m| offered.iter().any(|o| o == *m))
            .ok_or_else(|| {
                bad_request(format!(
                    "no offline payment method is enabled (one of {}); the store offers {}",
                    PAYMENT_METHODS.join(", "),
                    offered.join(", ")
                ))
            })?;
        let order_id = match &cart {
            Cart::Customer(id) => {
                self.write(
                    Method::Put,
                    "",
                    &format!("/carts/{id}/order"),
                    Some(json!({"paymentMethod": {"method": payment}})),
                )
                .await?
            }
            Cart::Guest(id) => {
                self.write(
                    Method::Post,
                    "",
                    &format!("/guest-carts/{id}/payment-information"),
                    Some(json!({
                        "email": email,
                        "paymentMethod": {"method": payment},
                        "billingAddress": billing
                    })),
                )
                .await?
            }
        };
        let order_id = types::string(&order_id)
            .ok_or_else(|| ConnectorError::InvalidResponse("no order id".into()))?;
        if let Some(note) = o.note.as_deref().filter(|n| !n.trim().is_empty()) {
            let note = OrderNote {
                note: note.to_string(),
                notify_customer: false,
            };
            self.add_order_note(&order_id, &note).await?;
        }
        match o.status.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(status) => self.update_order_status(&order_id, status).await,
            None => self.get_order(&order_id).await,
        }
    }

    /// A quote address (region as text plus id and code).
    async fn quote_address(
        &self,
        a: &AddressPatch,
        email: Option<&str>,
    ) -> Result<Value, ConnectorError> {
        let country = a
            .country
            .as_deref()
            .map(|c| c.trim().to_ascii_uppercase())
            .ok_or_else(|| bad_request("an order address needs a country (ISO code, e.g. ES)"))?;
        let mut out = Map::new();
        for (key, value) in [
            ("firstname", &a.first_name),
            ("lastname", &a.last_name),
            ("company", &a.company),
            ("city", &a.city),
            ("postcode", &a.postcode),
            ("telephone", &a.phone),
            ("vat_id", &a.id_number),
        ] {
            if let Some(v) = value {
                out.insert(key.into(), json!(v.trim()));
            }
        }
        let street: Vec<&str> = [a.address1.as_deref(), a.address2.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        out.insert("street".into(), json!(street));
        out.insert("country_id".into(), json!(country));
        if let Some(r) = self.region(&country, a.state.as_deref()).await? {
            out.insert("region".into(), json!(r.name));
            if let Some(id) = r.id {
                out.insert("region_id".into(), json!(id));
            }
            if let Some(code) = r.code {
                out.insert("region_code".into(), json!(code));
            }
        }
        if let Some(e) = email {
            out.insert("email".into(), json!(e));
        }
        Ok(Value::Object(out))
    }

    /// Abandoned means still active (never turned into an order), with
    /// items and an email, and untouched for `ABANDONED_AFTER_HOURS`.
    /// Magento keeps quote times in UTC.
    pub(super) async fn list_abandoned_carts_impl(
        &self,
        q: &CartQuery,
    ) -> Result<Page<AbandonedCart>, ConnectorError> {
        let idle_since =
            chrono::Utc::now().naive_utc() - chrono::Duration::hours(ABANDONED_AFTER_HOURS);
        let mut c = Self::criteria(q.page, q.per_page)
            .sort("updated_at", "DESC")
            .and(vec![Filter::eq("is_active", "1")])
            .and(vec![Filter::cond("items_count", "gt", "0")])
            .and(vec![Filter::cond("customer_email", "notnull", "")])
            .and(vec![Filter::cond(
                "updated_at",
                "lteq",
                idle_since.format("%Y-%m-%d %H:%M:%S").to_string(),
            )]);
        if let Some(since) = q.since {
            c = c.and(vec![Filter::cond(
                "updated_at",
                "gteq",
                format!("{since} 00:00:00"),
            )]);
        }
        self.list("/carts/search", c, types::abandoned_cart).await
    }
}

/// The customer's default shipping address, else their first one.
fn default_shipping(customer: &Value) -> Option<Value> {
    let addresses = customer["addresses"].as_array()?;
    let id = types::string(&customer["default_shipping"]);
    addresses
        .iter()
        .find(|a| id.is_some() && types::string(&a["id"]) == id)
        .or_else(|| addresses.first())
        .cloned()
}

fn from_customer_address(a: &Value, email: Option<&str>) -> Value {
    let region = &a["region"];
    let mut out = json!({
        "firstname": a["firstname"],
        "lastname": a["lastname"],
        "street": a["street"],
        "city": a["city"],
        "postcode": a["postcode"],
        "country_id": a["country_id"],
        "telephone": a["telephone"],
        "vat_id": a["vat_id"],
        "region": region["region"],
        "region_id": region["region_id"],
        "region_code": region["region_code"],
        "customer_address_id": a["id"],
    });
    if let Some(company) = types::string(&a["company"]) {
        out["company"] = json!(company);
    }
    if let Some(e) = email {
        out["email"] = json!(e);
    }
    out
}
