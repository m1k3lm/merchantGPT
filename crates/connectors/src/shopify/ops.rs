//! Creating and deleting records, collections as categories, customers and
//! their addresses, orders and notes, code discounts as coupons, abandoned
//! checkouts and raw GraphQL.

use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};

use super::types::{self, gid, quoted};
use super::{
    bad_request, check_user_errors, filter, is_mutation, per_page, queries, Shopify, MAX_PER_PAGE,
};
use crate::domain::*;
use crate::{ConnectorError, ShopConnector};

/// Discounts and abandoned checkouts nest connections, which multiply the cost.
const MAX_NESTED_PER_PAGE: u32 = 50;

const FINANCIAL_STATUSES: [&str; 8] = [
    "paid",
    "pending",
    "authorized",
    "partially_paid",
    "partially_refunded",
    "refunded",
    "voided",
    "expired",
];

fn not_found(what: String) -> ConnectorError {
    ConnectorError::NotFound(what)
}

fn invalid(what: &str) -> ConnectorError {
    ConnectorError::InvalidResponse(format!("Shopify returned no {what}"))
}

fn customer_input(c: &CustomerPatch) -> Map<String, Value> {
    let mut input = Map::new();
    for (key, value) in [
        ("email", &c.email),
        ("firstName", &c.first_name),
        ("lastName", &c.last_name),
        ("phone", &c.phone),
    ] {
        if let Some(v) = value {
            input.insert(key.into(), json!(v.trim()));
        }
    }
    input
}

fn consent(subscribed: bool) -> Value {
    json!({"marketingState": if subscribed { "SUBSCRIBED" } else { "UNSUBSCRIBED" }})
}

/// `customerGets` for a coupon: a share of the price (Shopify wants 0-1) or
/// an amount off the order (`fixed_cart`) or off each item (`fixed_product`).
fn customer_gets(kind: &str, amount: Decimal) -> Result<Value, ConnectorError> {
    let value = match kind {
        "percent" => {
            if amount <= Decimal::ZERO || amount > Decimal::from(100) {
                return Err(bad_request(
                    "a percent coupon needs an amount between 0 and 100",
                ));
            }
            let share: f64 = (amount / Decimal::from(100))
                .to_string()
                .parse()
                .map_err(|_| bad_request("bad percentage"))?;
            json!({"percentage": share})
        }
        "fixed_cart" | "fixed_product" => json!({"discountAmount": {
            "amount": amount.to_string(),
            "appliesOnEachItem": kind == "fixed_product"
        }}),
        other => {
            return Err(bad_request(format!(
                "unknown coupon kind {other:?}; use percent, fixed_cart or fixed_product"
            )))
        }
    };
    Ok(json!({"value": value, "items": {"all": true}}))
}

impl Shopify {
    /// Makes the product a member of exactly `wanted` collections. Shopify only
    /// lets products be added to or removed from manual collections; smart
    /// collections report an error.
    pub(super) async fn set_collections(
        &self,
        product_id: &Value,
        current: &[String],
        wanted: &[String],
    ) -> Result<(), ConnectorError> {
        let wanted: Vec<String> = wanted
            .iter()
            .map(|id| gid("Collection", id))
            .collect::<Result<_, _>>()?;
        for c in wanted.iter().filter(|c| !current.contains(c)) {
            let data = self
                .graphql(
                    queries::COLLECTION_ADD,
                    json!({"id": c, "productIds": [product_id]}),
                )
                .await?;
            check_user_errors(&data["collectionAddProducts"], "userErrors")?;
        }
        for c in current.iter().filter(|c| !wanted.contains(c)) {
            let data = self
                .graphql(
                    queries::COLLECTION_REMOVE,
                    json!({"id": c, "productIds": [product_id]}),
                )
                .await?;
            check_user_errors(&data["collectionRemoveProducts"], "userErrors")?;
        }
        Ok(())
    }

    /// Creates the product with its single default variant, then sets the
    /// variant's price, SKU and stock like an update.
    pub(super) async fn create_product_op(
        &self,
        p: &ProductPatch,
    ) -> Result<Product, ConnectorError> {
        let name = p
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| bad_request("a product needs a name"))?;
        let mut product = Map::new();
        product.insert("title".into(), json!(name));
        if let Some(st) = &p.status {
            product.insert("status".into(), json!(types::product_status(st)?));
        }
        if let Some(html) = &p.description {
            product.insert("descriptionHtml".into(), json!(html));
        }
        if let Some(ids) = &p.category_ids {
            let ids: Vec<String> = ids
                .iter()
                .map(|id| gid("Collection", id))
                .collect::<Result<_, _>>()?;
            product.insert("collectionsToJoin".into(), json!(ids));
        }
        let data = self
            .graphql(queries::PRODUCT_CREATE, json!({"product": product}))
            .await?;
        check_user_errors(&data["productCreate"], "userErrors")?;
        let variant = data["productCreate"]["product"]["variants"]["nodes"][0]["id"]
            .as_str()
            .ok_or_else(|| invalid("variant for the new product"))?
            .to_string();
        let rest = ProductPatch {
            sku: p.sku.clone(),
            regular_price: p.regular_price,
            sale_price: p.sale_price,
            stock_quantity: p.stock_quantity,
            ..Default::default()
        };
        if rest.is_empty() {
            self.get_product(&variant).await
        } else {
            self.update_product(&variant, &rest).await
        }
    }

    /// Ids are variants: a product with several variants loses only this one;
    /// a single-variant product is deleted whole.
    pub(super) async fn delete_product_op(&self, id: &str) -> Result<(), ConnectorError> {
        let (_, raw) = self.variant(id).await?;
        let product_id = raw["product"]["id"].clone();
        let variants = raw["product"]["variantsCount"]["count"]
            .as_u64()
            .unwrap_or(1);
        if variants > 1 {
            let data = self
                .graphql(
                    queries::VARIANTS_DELETE,
                    json!({"productId": product_id, "variantsIds": [raw["id"]]}),
                )
                .await?;
            check_user_errors(&data["productVariantsBulkDelete"], "userErrors")
        } else {
            let data = self
                .graphql(
                    queries::PRODUCT_DELETE,
                    json!({"input": {"id": product_id}}),
                )
                .await?;
            check_user_errors(&data["productDelete"], "userErrors")
        }
    }

    pub(super) async fn list_collections(
        &self,
        q: &CategoryQuery,
    ) -> Result<Page<Category>, ConnectorError> {
        let query = filter(
            q.search
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(quoted)
                .into_iter()
                .collect(),
        );
        self.page(
            &queries::collections(),
            ("collections", ""),
            query,
            json!({}),
            q.page,
            per_page(q.per_page, MAX_PER_PAGE),
            None,
            types::category,
        )
        .await
    }

    pub(super) async fn save_collection(
        &self,
        id: Option<&str>,
        c: &CategoryPatch,
    ) -> Result<Category, ConnectorError> {
        if c.parent_id.is_some() {
            return Err(ConnectorError::Unsupported(
                "Shopify collections cannot be nested, so a category has no parent".into(),
            ));
        }
        let mut input = Map::new();
        if let Some(name) = &c.name {
            input.insert("title".into(), json!(name.trim()));
        }
        if let Some(html) = &c.description {
            input.insert("descriptionHtml".into(), json!(html));
        }
        let (document, field) = match id {
            None => (queries::collection_create(), "collectionCreate"),
            Some(id) => {
                input.insert("id".into(), json!(gid("Collection", id)?));
                (queries::collection_update(), "collectionUpdate")
            }
        };
        let data = self
            .graphql(&document, json!({"collection": input}))
            .await?;
        check_user_errors(&data[field], "userErrors")?;
        types::category(&data[field]["collection"])
    }

    pub(super) async fn delete_collection(&self, id: &str) -> Result<(), ConnectorError> {
        let id = gid("Collection", id)?;
        let data = self
            .graphql(queries::COLLECTION_DELETE, json!({"input": {"id": id}}))
            .await?;
        check_user_errors(&data["collectionDelete"], "userErrors")?;
        if data["collectionDelete"]["deletedCollectionId"].is_null() {
            return Err(not_found(format!("collection {id}")));
        }
        Ok(())
    }

    pub(super) async fn customer_raw(&self, id: &str) -> Result<(Customer, Value), ConnectorError> {
        let id = gid("Customer", id)?;
        let data = self
            .graphql(&queries::customer(), json!({"id": id}))
            .await?;
        let c = &data["customer"];
        if c.is_null() {
            return Err(not_found(format!("customer {id}")));
        }
        Ok((types::customer(c)?, c.clone()))
    }

    /// Consent can be given at creation; withholding it is the default.
    pub(super) async fn create_customer_op(
        &self,
        c: &CustomerPatch,
    ) -> Result<Customer, ConnectorError> {
        let mut input = customer_input(c);
        if c.accepts_marketing == Some(true) {
            input.insert("emailMarketingConsent".into(), consent(true));
        }
        let data = self
            .graphql(queries::CUSTOMER_CREATE, json!({"input": input}))
            .await?;
        check_user_errors(&data["customerCreate"], "userErrors")?;
        let id = data["customerCreate"]["customer"]["id"]
            .as_str()
            .ok_or_else(|| invalid("id for the new customer"))?;
        self.get_customer(id).await
    }

    /// Marketing consent changes through its own mutation.
    pub(super) async fn update_customer_op(
        &self,
        id: &str,
        c: &CustomerPatch,
    ) -> Result<Customer, ConnectorError> {
        let id = gid("Customer", id)?;
        let mut input = customer_input(c);
        if !input.is_empty() {
            input.insert("id".into(), json!(id));
            let data = self
                .graphql(queries::CUSTOMER_UPDATE, json!({"input": input}))
                .await?;
            check_user_errors(&data["customerUpdate"], "userErrors")?;
        }
        if let Some(subscribed) = c.accepts_marketing {
            let data = self
                .graphql(
                    queries::CUSTOMER_CONSENT,
                    json!({"input": {"customerId": id, "emailMarketingConsent": consent(subscribed)}}),
                )
                .await?;
            check_user_errors(&data["customerEmailMarketingConsentUpdate"], "userErrors")?;
        }
        self.get_customer(&id).await
    }

    /// Shopify refuses to delete customers who have placed orders.
    pub(super) async fn delete_customer_op(&self, id: &str) -> Result<(), ConnectorError> {
        let id = gid("Customer", id)?;
        let data = self
            .graphql(queries::CUSTOMER_DELETE, json!({"input": {"id": id}}))
            .await?;
        check_user_errors(&data["customerDelete"], "userErrors")
    }

    pub(super) async fn list_addresses_op(
        &self,
        customer_id: &str,
    ) -> Result<Vec<Address>, ConnectorError> {
        let (_, raw) = self.customer_raw(customer_id).await?;
        let default = raw["defaultAddress"]["id"].as_str();
        raw["addressesV2"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| types::address(a, default))
            .collect()
    }

    /// Shopify addresses have no billing or shipping slot; `kind: default`
    /// makes one the customer's default.
    pub(super) async fn save_address(
        &self,
        customer_id: &str,
        id: Option<&str>,
        a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        let set_default = types::wants_default(a);
        if set_default == Some(false) {
            return Err(bad_request(
                "Shopify customer addresses have no billing or shipping slot; the only kind is \"default\"",
            ));
        }
        let customer = gid("Customer", customer_id)?;
        let address = types::address_input(a);
        let (document, field, vars) = match id {
            None => (
                queries::address_create(),
                "customerAddressCreate",
                json!({"customerId": customer, "address": address, "setAsDefault": set_default}),
            ),
            Some(id) => (
                queries::address_update(),
                "customerAddressUpdate",
                json!({
                    "customerId": customer,
                    "addressId": types::address_gid(id)?,
                    "address": address,
                    "setAsDefault": set_default
                }),
            ),
        };
        let data = self.graphql(&document, vars).await?;
        check_user_errors(&data[field], "userErrors")?;
        let saved = &data[field]["address"];
        let default = if set_default == Some(true) {
            saved["id"].as_str()
        } else {
            None
        };
        types::address(saved, default)
    }

    pub(super) async fn delete_address_op(
        &self,
        customer_id: &str,
        id: &str,
    ) -> Result<(), ConnectorError> {
        let data = self
            .graphql(
                queries::ADDRESS_DELETE,
                json!({"customerId": gid("Customer", customer_id)?, "addressId": types::address_gid(id)?}),
            )
            .await?;
        check_user_errors(&data["customerAddressDelete"], "userErrors")
    }

    /// Creates the order without emailing the customer; stock is claimed as
    /// the products' inventory policy allows. `status` is the payment status.
    pub(super) async fn create_order_op(&self, o: &NewOrder) -> Result<Order, ConnectorError> {
        if o.line_items.is_empty() {
            return Err(bad_request("an order needs line items"));
        }
        let financial = match o.status.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(s) => {
                let s = s.to_ascii_lowercase().replace(['-', ' '], "_");
                if !FINANCIAL_STATUSES.contains(&s.as_str()) {
                    return Err(bad_request(format!(
                        "a new Shopify order's status is its payment status: one of {}",
                        FINANCIAL_STATUSES.join(", ")
                    )));
                }
                Some(s.to_ascii_uppercase())
            }
        };
        let currency = if o.line_items.iter().any(|li| li.price.is_some()) {
            let data = self.graphql(queries::SHOP_CURRENCY, json!({})).await?;
            data["shop"]["currencyCode"].clone()
        } else {
            Value::Null
        };
        let mut lines = Vec::new();
        for li in &o.line_items {
            if li.quantity < 1 {
                return Err(bad_request("line item quantities must be at least 1"));
            }
            let mut line = Map::new();
            line.insert(
                "variantId".into(),
                json!(gid("ProductVariant", &li.product_id)?),
            );
            line.insert("quantity".into(), json!(li.quantity));
            if let Some(price) = li.price {
                line.insert(
                    "priceSet".into(),
                    json!({"shopMoney": {"amount": price.to_string(), "currencyCode": currency}}),
                );
            }
            lines.push(Value::Object(line));
        }
        let mut order = Map::new();
        order.insert("lineItems".into(), Value::Array(lines));
        if let Some(c) = o.customer_id.as_deref().filter(|c| !c.trim().is_empty()) {
            order.insert(
                "customer".into(),
                json!({"toAssociate": {"id": gid("Customer", c)?}}),
            );
        }
        if let Some(e) = o.email.as_deref().filter(|e| !e.trim().is_empty()) {
            order.insert("email".into(), json!(e.trim()));
        }
        if let Some(a) = &o.billing {
            order.insert("billingAddress".into(), types::address_input(a));
        }
        if let Some(a) = &o.shipping {
            order.insert("shippingAddress".into(), types::address_input(a));
        }
        if let Some(n) = &o.note {
            order.insert("note".into(), json!(n));
        }
        if let Some(f) = financial {
            order.insert("financialStatus".into(), json!(f));
        }
        let options =
            json!({"sendReceipt": false, "inventoryBehaviour": "DECREMENT_OBEYING_POLICY"});
        let data = self
            .graphql(
                queries::ORDER_CREATE,
                json!({"order": order, "options": options}),
            )
            .await?;
        check_user_errors(&data["orderCreate"], "userErrors")?;
        let id = data["orderCreate"]["order"]["id"]
            .as_str()
            .ok_or_else(|| invalid("id for the new order"))?;
        self.get_order(id).await
    }

    /// Appends to the order's note. Shopify has no way to email a free-form
    /// note to the customer.
    pub(super) async fn add_order_note_op(
        &self,
        id: &str,
        note: &OrderNote,
    ) -> Result<(), ConnectorError> {
        if note.notify_customer {
            return Err(ConnectorError::Unsupported(
                "Shopify cannot email an order note to the customer; add it without notifying, or send the message through a linked email service".into(),
            ));
        }
        let order = self.get_order(id).await?;
        let data = self
            .graphql(queries::ORDER_NOTE, json!({"id": order.id}))
            .await?;
        let current = data["order"]["note"].as_str().unwrap_or_default().trim();
        let text = if current.is_empty() {
            note.note.clone()
        } else {
            format!("{current}\n{}", note.note)
        };
        let data = self
            .graphql(
                queries::ORDER_UPDATE,
                json!({"input": {"id": order.id, "note": text}}),
            )
            .await?;
        check_user_errors(&data["orderUpdate"], "userErrors")
    }

    /// Amount-off code discounts; other discount types are left out.
    pub(super) async fn list_coupons_op(
        &self,
        q: &CouponQuery,
    ) -> Result<Page<Coupon>, ConnectorError> {
        let mut terms = vec!["method:code".to_string()];
        if let Some(code) = q.code.as_deref().filter(|c| !c.trim().is_empty()) {
            terms.push(format!("code:{}", quoted(code)));
        }
        let page = self
            .page(
                &queries::discounts(),
                ("discountNodes", ""),
                filter(terms),
                json!({}),
                q.page,
                per_page(q.per_page, MAX_NESTED_PER_PAGE),
                None,
                types::coupon,
            )
            .await?;
        Ok(Page {
            items: page.items.into_iter().flatten().collect(),
            page: page.page,
            total_pages: page.total_pages,
            total: page.total,
        })
    }

    async fn coupon(&self, id: &str) -> Result<Coupon, ConnectorError> {
        let data = self
            .graphql(&queries::discount(), json!({"id": id}))
            .await?;
        types::coupon(&data["discountNode"])?
            .ok_or_else(|| not_found(format!("amount-off code discount {id}")))
    }

    /// Coupons apply to all products and all buyers; `expires_on` ends at the
    /// close of that day (UTC). `active` maps to (de)activation.
    pub(super) async fn save_coupon(
        &self,
        id: Option<&str>,
        c: &CouponPatch,
    ) -> Result<Coupon, ConnectorError> {
        let id = id.map(|i| gid("DiscountCodeNode", i)).transpose()?;
        let mut d = Map::new();
        if let Some(code) = &c.code {
            d.insert("code".into(), json!(code.trim()));
        }
        let title = c
            .description
            .clone()
            .or_else(|| id.is_none().then(|| c.code.clone()).flatten());
        if let Some(t) = title {
            d.insert("title".into(), json!(t.trim()));
        }
        if id.is_none() {
            d.insert("startsAt".into(), json!(Utc::now().to_rfc3339()));
            d.insert("context".into(), json!({"all": "ALL"}));
        }
        if let Some(date) = c.expires_on {
            d.insert("endsAt".into(), json!(format!("{date}T23:59:59Z")));
        }
        if let Some(limit) = c.usage_limit {
            d.insert("usageLimit".into(), json!(limit));
        }
        if let Some(min) = c.minimum_amount {
            d.insert(
                "minimumRequirement".into(),
                json!({"subtotal": {"greaterThanOrEqualToSubtotal": min.to_string()}}),
            );
        }
        if c.kind.is_some() || c.amount.is_some() {
            let (kind, amount) = match (&c.kind, c.amount, &id) {
                (Some(k), Some(a), _) => (k.clone(), a),
                (_, _, Some(existing)) => {
                    let current = self.coupon(existing).await?;
                    (
                        c.kind.clone().unwrap_or(current.kind),
                        c.amount.unwrap_or(current.amount),
                    )
                }
                _ => return Err(bad_request("a new coupon needs a kind and an amount")),
            };
            d.insert("customerGets".into(), customer_gets(kind.trim(), amount)?);
        }
        let id = match id {
            None => {
                let data = self
                    .graphql(queries::DISCOUNT_CREATE, json!({"basicCodeDiscount": d}))
                    .await?;
                check_user_errors(&data["discountCodeBasicCreate"], "userErrors")?;
                data["discountCodeBasicCreate"]["codeDiscountNode"]["id"]
                    .as_str()
                    .ok_or_else(|| invalid("id for the new discount"))?
                    .to_string()
            }
            Some(id) => {
                if !d.is_empty() {
                    let data = self
                        .graphql(
                            queries::DISCOUNT_UPDATE,
                            json!({"id": id, "basicCodeDiscount": d}),
                        )
                        .await?;
                    check_user_errors(&data["discountCodeBasicUpdate"], "userErrors")?;
                }
                id
            }
        };
        let toggle = match (c.active, d.contains_key("startsAt")) {
            (Some(false), _) => Some((queries::DISCOUNT_DEACTIVATE, "discountCodeDeactivate")),
            (Some(true), false) => Some((queries::DISCOUNT_ACTIVATE, "discountCodeActivate")),
            _ => None,
        };
        if let Some((document, field)) = toggle {
            let data = self.graphql(document, json!({"id": id})).await?;
            check_user_errors(&data[field], "userErrors")?;
        }
        self.coupon(&id).await
    }

    pub(super) async fn delete_coupon_op(&self, id: &str) -> Result<(), ConnectorError> {
        let data = self
            .graphql(
                queries::DISCOUNT_DELETE,
                json!({"id": gid("DiscountCodeNode", id)?}),
            )
            .await?;
        check_user_errors(&data["discountCodeDelete"], "userErrors")
    }

    /// Open (not completed) checkouts, most recent first.
    pub(super) async fn list_abandoned_op(
        &self,
        q: &CartQuery,
    ) -> Result<Page<AbandonedCart>, ConnectorError> {
        let mut terms = vec!["status:open".to_string()];
        if let Some(since) = q.since {
            terms.push(format!("updated_at:>={since}"));
        }
        self.page(
            queries::ABANDONED,
            ("abandonedCheckouts", queries::NEWEST_FIRST),
            filter(terms),
            json!({}),
            q.page,
            per_page(q.per_page, MAX_NESTED_PER_PAGE),
            None,
            types::abandoned_cart,
        )
        .await
    }

    /// GraphQL errors come back as data so the model can correct its document.
    pub(super) async fn raw_op(&self, req: &RawRequest) -> Result<RawResponse, ConnectorError> {
        let Some(document) = req
            .body
            .as_ref()
            .and_then(Value::as_str)
            .filter(|d| !d.trim().is_empty())
        else {
            return Err(bad_request(
                "put the GraphQL document in `body` as a string, and its variables in `variables`",
            ));
        };
        if req.method.eq_ignore_ascii_case("GET") && is_mutation(document) {
            return Err(bad_request(
                "shop_api_get only runs queries; send mutations with shop_api_write",
            ));
        }
        let variables = req.variables.clone().unwrap_or_else(|| json!({}));
        let body = self.execute(document, variables, true).await?;
        Ok(RawResponse { status: 200, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shopify::ACCESS_TOKEN;
    use crate::transport::{FakeTransport, HttpResponse, Method};
    use crate::Credentials;
    use std::str::FromStr;
    use std::sync::Arc;

    const GQL: &str = "/admin/api/2026-10/graphql.json";

    fn shop(t: &Arc<FakeTransport>) -> Shopify {
        let creds = Credentials::from([(ACCESS_TOKEN.into(), "shpat_x".into())]);
        Shopify::new("https://acme.myshopify.com", &creds, t.clone())
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

    fn sent(t: &FakeTransport) -> Vec<Value> {
        t.requests()
            .iter()
            .map(|r| serde_json::from_str(r.body.as_deref().unwrap()).unwrap())
            .collect()
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn variant(levels: usize, variants: u64, collections: &[&str], tracked: bool) -> Value {
        json!({
            "id": "gid://shopify/ProductVariant/11", "title": "Default Title", "sku": "MUG",
            "price": "10.00", "compareAtPrice": null, "inventoryQuantity": 0,
            "inventoryItem": {
                "id": "gid://shopify/InventoryItem/70", "tracked": tracked,
                "inventoryLevels": {"nodes": (0..levels)
                    .map(|n| json!({"location": {"id": format!("gid://shopify/Location/{n}")}}))
                    .collect::<Vec<_>>()}
            },
            "product": {
                "id": "gid://shopify/Product/9", "title": "Mug", "status": "ACTIVE",
                "variantsCount": {"count": variants},
                "collections": {"nodes": collections.iter().map(|c| json!({"id": c})).collect::<Vec<_>>()}
            }
        })
    }

    fn order() -> Value {
        json!({
            "id": "gid://shopify/Order/5", "name": "#1001", "createdAt": "2026-10-18T09:00:00Z",
            "cancelledAt": null, "closed": false,
            "displayFinancialStatus": "PAID", "displayFulfillmentStatus": "UNFULFILLED",
            "totalPriceSet": {"shopMoney": {"amount": "20.00", "currencyCode": "EUR"}},
            "email": "ana@example.com", "customer": null, "billingAddress": null,
            "lineItems": {"nodes": []}
        })
    }

    fn customer_node() -> Value {
        json!({
            "id": "gid://shopify/Customer/77", "firstName": "Jo", "lastName": "Doe",
            "numberOfOrders": "0", "amountSpent": {"amount": "0.0"},
            "defaultEmailAddress": {"emailAddress": "jo@example.com", "marketingState": "SUBSCRIBED"},
            "defaultPhoneNumber": {"phoneNumber": "+34600000000"},
            "defaultAddress": {"id": "gid://shopify/MailingAddress/2?model_name=CustomerAddress"},
            "addressesV2": {"nodes": [
                {"id": "gid://shopify/MailingAddress/1?model_name=CustomerAddress", "city": "Bilbao", "countryCodeV2": "ES"},
                {"id": "gid://shopify/MailingAddress/2?model_name=CustomerAddress", "city": "Madrid", "countryCodeV2": "ES"}
            ]}
        })
    }

    fn discount_node(value: Value) -> Value {
        json!({"id": "gid://shopify/DiscountCodeNode/3", "discount": {
            "__typename": "DiscountCodeBasic", "title": "Autumn", "status": "ACTIVE",
            "startsAt": "2026-10-01T00:00:00Z", "endsAt": "2026-11-30T23:59:59Z",
            "usageLimit": 100, "asyncUsageCount": 4,
            "codes": {"nodes": [{"code": "AUTUMN15"}]},
            "customerGets": {"value": value},
            "minimumRequirement": {"__typename": "DiscountMinimumSubtotal", "greaterThanOrEqualToSubtotal": {"amount": "30.0"}}
        }})
    }

    #[tokio::test]
    async fn new_products_get_their_variant_price_and_stock_at_the_only_location() {
        let t = Arc::new(FakeTransport::new());
        data(
            &t,
            json!({"productCreate": {"product": {"id": "gid://shopify/Product/9",
            "variants": {"nodes": [{"id": "gid://shopify/ProductVariant/11"}]}}, "userErrors": []}}),
        );
        data(&t, json!({"productVariant": variant(0, 1, &[], false)}));
        data(&t, json!({"productVariantsBulkUpdate": {"userErrors": []}}));
        data(
            &t,
            json!({"locations": {"nodes": [{"id": "gid://shopify/Location/1"}]}}),
        );
        data(&t, json!({"inventoryActivate": {"userErrors": []}}));
        data(
            &t,
            json!({"productVariant": variant(1, 1, &["gid://shopify/Collection/4"], true)}),
        );
        let p = shop(&t)
            .create_product(&ProductPatch {
                name: Some("Mug".into()),
                status: Some("draft".into()),
                description: Some("<p>Big</p>".into()),
                category_ids: Some(vec!["4".into()]),
                regular_price: Some(dec("12.50")),
                stock_quantity: Some(6),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(p.category_ids, ["gid://shopify/Collection/4"]);

        let b = sent(&t);
        assert_eq!(
            b[0]["variables"]["product"],
            json!({"title": "Mug", "status": "DRAFT", "descriptionHtml": "<p>Big</p>",
                "collectionsToJoin": ["gid://shopify/Collection/4"]})
        );
        assert_eq!(
            b[2]["variables"]["variants"],
            json!([{"id": "gid://shopify/ProductVariant/11", "price": "12.50", "inventoryItem": {"tracked": true}}])
        );
        let activate = &b[4];
        assert!(activate["query"].as_str().unwrap().contains("@idempotent"));
        assert_eq!(activate["variables"]["available"], 6);
        assert_eq!(
            activate["variables"]["locationId"],
            "gid://shopify/Location/1"
        );
    }

    #[tokio::test]
    async fn deleting_removes_the_variant_or_the_single_variant_product() {
        let t = Arc::new(FakeTransport::new());
        data(&t, json!({"productVariant": variant(1, 3, &[], true)}));
        data(&t, json!({"productVariantsBulkDelete": {"userErrors": []}}));
        data(&t, json!({"productVariant": variant(1, 1, &[], true)}));
        data(
            &t,
            json!({"productDelete": {"deletedProductId": "gid://shopify/Product/9", "userErrors": []}}),
        );
        let s = shop(&t);
        s.delete_product("11").await.unwrap();
        s.delete_product("11").await.unwrap();
        let b = sent(&t);
        assert_eq!(
            b[1]["variables"],
            json!({"productId": "gid://shopify/Product/9", "variantsIds": ["gid://shopify/ProductVariant/11"]})
        );
        assert_eq!(
            b[3]["variables"],
            json!({"input": {"id": "gid://shopify/Product/9"}})
        );
    }

    #[tokio::test]
    async fn category_ids_add_and_remove_collection_memberships() {
        let t = Arc::new(FakeTransport::new());
        let (one, two) = ("gid://shopify/Collection/1", "gid://shopify/Collection/2");
        data(
            &t,
            json!({"productVariant": variant(1, 1, &[one, two], true)}),
        );
        data(&t, json!({"collectionAddProducts": {"userErrors": []}}));
        data(&t, json!({"collectionRemoveProducts": {"userErrors": []}}));
        data(
            &t,
            json!({"productVariant": variant(1, 1, &[two, "gid://shopify/Collection/3"], true)}),
        );
        shop(&t)
            .update_product(
                "11",
                &ProductPatch {
                    category_ids: Some(vec!["2".into(), "3".into()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let b = sent(&t);
        assert!(b[1]["query"]
            .as_str()
            .unwrap()
            .contains("collectionAddProducts"));
        assert_eq!(b[1]["variables"]["id"], "gid://shopify/Collection/3");
        assert!(b[2]["query"]
            .as_str()
            .unwrap()
            .contains("collectionRemoveProducts"));
        assert_eq!(b[2]["variables"]["id"], one);
        assert_eq!(
            b[2]["variables"]["productIds"],
            json!(["gid://shopify/Product/9"])
        );
    }

    #[tokio::test]
    async fn collections_are_categories_without_parents() {
        let t = Arc::new(FakeTransport::new());
        let k = json!({"id": "gid://shopify/Collection/4", "title": "Mugs",
            "descriptionHtml": "", "productsCount": {"count": 7}});
        data(
            &t,
            json!({"collections": {"nodes": [k], "pageInfo": {"hasNextPage": false}}}),
        );
        data(
            &t,
            json!({"collectionUpdate": {"collection": k, "userErrors": []}}),
        );
        data(
            &t,
            json!({"collectionDelete": {"deletedCollectionId": null, "userErrors": []}}),
        );
        let s = shop(&t);
        let page = s.list_categories(&CategoryQuery::default()).await.unwrap();
        assert_eq!(
            (
                page.items[0].name.as_str(),
                page.items[0].count,
                &page.items[0].description
            ),
            ("Mugs", Some(7), &None)
        );
        let nested = s
            .create_category(&CategoryPatch {
                name: Some("Cups".into()),
                parent_id: Some("4".into()),
                ..Default::default()
            })
            .await;
        assert!(matches!(nested, Err(ConnectorError::Unsupported(_))));
        s.update_category(
            "4",
            &CategoryPatch {
                name: Some("Big mugs".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            s.delete_category("4").await,
            Err(ConnectorError::NotFound(_))
        ));
        let b = sent(&t);
        assert_eq!(
            b[1]["variables"]["collection"],
            json!({"title": "Big mugs", "id": "gid://shopify/Collection/4"})
        );
    }

    #[tokio::test]
    async fn customers_carry_consent_phone_and_addresses() {
        let t = Arc::new(FakeTransport::new());
        data(&t, json!({"customer": customer_node()}));
        data(&t, json!({"customer": customer_node()}));
        let s = shop(&t);
        let c = s.get_customer("77").await.unwrap();
        assert_eq!(c.accepts_marketing, Some(true));
        assert_eq!(c.phone.as_deref(), Some("+34600000000"));
        let addresses = s.list_addresses("77").await.unwrap();
        assert_eq!(addresses[0].kind, None);
        assert_eq!(addresses[1].kind.as_deref(), Some("default"));
        assert_eq!(addresses[1].country.as_deref(), Some("ES"));
    }

    #[tokio::test]
    async fn consent_is_set_on_create_and_through_its_own_mutation_on_update() {
        let t = Arc::new(FakeTransport::new());
        data(
            &t,
            json!({"customerCreate": {"customer": {"id": "gid://shopify/Customer/77"}, "userErrors": []}}),
        );
        data(&t, json!({"customer": customer_node()}));
        data(
            &t,
            json!({"customerUpdate": {"customer": {"id": "gid://shopify/Customer/77"}, "userErrors": []}}),
        );
        data(
            &t,
            json!({"customerEmailMarketingConsentUpdate": {"userErrors": []}}),
        );
        data(&t, json!({"customer": customer_node()}));
        let s = shop(&t);
        s.create_customer(&CustomerPatch {
            email: Some(" jo@example.com ".into()),
            first_name: Some("Jo".into()),
            accepts_marketing: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
        s.update_customer(
            "77",
            &CustomerPatch {
                last_name: Some("Roe".into()),
                accepts_marketing: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let b = sent(&t);
        assert_eq!(
            b[0]["variables"]["input"],
            json!({"email": "jo@example.com", "firstName": "Jo",
                "emailMarketingConsent": {"marketingState": "SUBSCRIBED"}})
        );
        assert_eq!(
            b[2]["variables"]["input"],
            json!({"lastName": "Roe", "id": "gid://shopify/Customer/77"})
        );
        assert_eq!(
            b[3]["variables"]["input"],
            json!({"customerId": "gid://shopify/Customer/77",
                "emailMarketingConsent": {"marketingState": "UNSUBSCRIBED"}})
        );
    }

    #[tokio::test]
    async fn addresses_map_to_mailing_address_inputs() {
        let t = Arc::new(FakeTransport::new());
        let saved = json!({"id": "gid://shopify/MailingAddress/9?model_name=CustomerAddress", "city": "Bilbao", "countryCodeV2": "ES"});
        data(
            &t,
            json!({"customerAddressCreate": {"address": saved, "userErrors": []}}),
        );
        data(
            &t,
            json!({"customerAddressDelete": {"deletedAddressId": "x", "userErrors": []}}),
        );
        let s = shop(&t);
        let a = s
            .create_address(
                "77",
                &AddressPatch {
                    kind: Some("default".into()),
                    city: Some("Bilbao".into()),
                    postcode: Some("48001".into()),
                    country: Some("es".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(a.kind.as_deref(), Some("default"));
        let billing = s
            .create_address(
                "77",
                &AddressPatch {
                    kind: Some("billing".into()),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(
            billing,
            Err(ConnectorError::Api { status: 400, .. })
        ));
        s.delete_address(
            "77",
            "gid://shopify/MailingAddress/9?model_name=CustomerAddress",
        )
        .await
        .unwrap();
        let b = sent(&t);
        assert_eq!(
            b[0]["variables"],
            json!({"customerId": "gid://shopify/Customer/77", "setAsDefault": true,
                "address": {"city": "Bilbao", "zip": "48001", "countryCode": "ES"}})
        );
        assert_eq!(
            b[1]["variables"]["addressId"],
            "gid://shopify/MailingAddress/9?model_name=CustomerAddress"
        );
    }

    #[tokio::test]
    async fn orders_are_created_paid_or_pending_without_emailing() {
        let t = Arc::new(FakeTransport::new());
        data(&t, json!({"shop": {"currencyCode": "EUR"}}));
        data(
            &t,
            json!({"orderCreate": {"order": {"id": "gid://shopify/Order/5"}, "userErrors": []}}),
        );
        data(&t, json!({"order": order()}));
        let s = shop(&t);
        let o = s
            .create_order(&NewOrder {
                customer_id: Some("77".into()),
                line_items: vec![NewLineItem {
                    product_id: "11".into(),
                    quantity: 2,
                    price: Some(dec("9.95")),
                }],
                shipping: Some(AddressPatch {
                    city: Some("Bilbao".into()),
                    ..Default::default()
                }),
                status: Some("paid".into()),
                note: Some("phone order".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(o.number, "#1001");
        let b = sent(&t);
        let vars = &b[1]["variables"];
        assert_eq!(
            vars["order"]["lineItems"],
            json!([{"variantId": "gid://shopify/ProductVariant/11", "quantity": 2,
                "priceSet": {"shopMoney": {"amount": "9.95", "currencyCode": "EUR"}}}])
        );
        assert_eq!(
            vars["order"]["customer"],
            json!({"toAssociate": {"id": "gid://shopify/Customer/77"}})
        );
        assert_eq!(vars["order"]["financialStatus"], "PAID");
        assert_eq!(vars["order"]["note"], "phone order");
        assert_eq!(
            vars["options"],
            json!({"sendReceipt": false, "inventoryBehaviour": "DECREMENT_OBEYING_POLICY"})
        );
        let bad = s
            .create_order(&NewOrder {
                line_items: vec![NewLineItem {
                    product_id: "11".into(),
                    quantity: 1,
                    price: None,
                }],
                status: Some("processing".into()),
                ..Default::default()
            })
            .await;
        assert!(matches!(bad, Err(ConnectorError::Api { status: 400, .. })));
    }

    #[tokio::test]
    async fn notes_append_and_cannot_be_emailed() {
        let t = Arc::new(FakeTransport::new());
        data(&t, json!({"order": order()}));
        data(
            &t,
            json!({"order": {"id": "gid://shopify/Order/5", "note": "first"}}),
        );
        data(&t, json!({"orderUpdate": {"userErrors": []}}));
        let s = shop(&t);
        s.add_order_note(
            "5",
            &OrderNote {
                note: "second".into(),
                notify_customer: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            sent(&t)[2]["variables"]["input"],
            json!({"id": "gid://shopify/Order/5", "note": "first\nsecond"})
        );
        let emailed = s
            .add_order_note(
                "5",
                &OrderNote {
                    note: "hi".into(),
                    notify_customer: true,
                },
            )
            .await;
        assert!(matches!(emailed, Err(ConnectorError::Unsupported(_))));
        assert_eq!(s.hidden(), ["shop_manage_order:delete"]);
    }

    #[tokio::test]
    async fn coupons_list_only_amount_off_code_discounts() {
        let t = Arc::new(FakeTransport::new());
        let percent =
            discount_node(json!({"__typename": "DiscountPercentage", "percentage": 0.15}));
        let shipping = json!({"id": "gid://shopify/DiscountCodeNode/4", "discount": {"__typename": "DiscountCodeFreeShipping"}});
        data(
            &t,
            json!({"discountNodes": {"nodes": [percent, shipping], "pageInfo": {"hasNextPage": false}}}),
        );
        let page = shop(&t)
            .list_coupons(&CouponQuery {
                code: Some("AUTUMN15".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        let c = &page.items[0];
        assert_eq!(
            (c.code.as_str(), c.kind.as_str(), c.amount),
            ("AUTUMN15", "percent", dec("15"))
        );
        assert_eq!(c.minimum_amount, Some(dec("30")));
        assert_eq!(c.expires_on, chrono::NaiveDate::from_ymd_opt(2026, 11, 30));
        assert_eq!(
            (c.usage_limit, c.usage_count, c.active),
            (Some(100), Some(4), Some(true))
        );
        assert_eq!(
            sent(&t)[0]["variables"]["query"],
            "method:code code:\"AUTUMN15\""
        );
    }

    #[tokio::test]
    async fn coupons_are_created_for_everyone_and_updated_partially() {
        let t = Arc::new(FakeTransport::new());
        let fixed = discount_node(
            json!({"__typename": "DiscountAmount", "amount": {"amount": "5.0"}, "appliesOnEachItem": true}),
        );
        data(
            &t,
            json!({"discountCodeBasicCreate": {"codeDiscountNode": {"id": "gid://shopify/DiscountCodeNode/3"}, "userErrors": []}}),
        );
        data(&t, json!({"discountCodeDeactivate": {"userErrors": []}}));
        data(&t, json!({"discountNode": fixed.clone()}));
        data(&t, json!({"discountNode": fixed.clone()}));
        data(
            &t,
            json!({"discountCodeBasicUpdate": {"codeDiscountNode": {"id": "gid://shopify/DiscountCodeNode/3"}, "userErrors": []}}),
        );
        data(&t, json!({"discountNode": fixed}));
        let s = shop(&t);
        s.create_coupon(&CouponPatch {
            code: Some("AUTUMN15".into()),
            kind: Some("percent".into()),
            amount: Some(dec("15")),
            expires_on: chrono::NaiveDate::from_ymd_opt(2026, 11, 30),
            active: Some(false),
            ..Default::default()
        })
        .await
        .unwrap();
        let updated = s
            .update_coupon(
                "3",
                &CouponPatch {
                    amount: Some(dec("7")),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.kind, "fixed_product");
        let b = sent(&t);
        let created = &b[0]["variables"]["basicCodeDiscount"];
        assert_eq!(created["code"], "AUTUMN15");
        assert_eq!(created["title"], "AUTUMN15");
        assert_eq!(created["context"], json!({"all": "ALL"}));
        assert_eq!(created["endsAt"], "2026-11-30T23:59:59Z");
        assert!(created["startsAt"].as_str().is_some());
        assert_eq!(
            created["customerGets"],
            json!({"value": {"percentage": 0.15}, "items": {"all": true}})
        );
        assert!(b[1]["query"]
            .as_str()
            .unwrap()
            .contains("discountCodeDeactivate"));
        assert_eq!(
            b[4]["variables"]["basicCodeDiscount"],
            json!({"customerGets": {"value": {"discountAmount": {"amount": "7", "appliesOnEachItem": true}}, "items": {"all": true}}})
        );
        let bad = s
            .create_coupon(&CouponPatch {
                code: Some("X".into()),
                kind: Some("percent".into()),
                amount: Some(dec("150")),
                ..Default::default()
            })
            .await;
        assert!(matches!(bad, Err(ConnectorError::Api { status: 400, .. })));
    }

    #[tokio::test]
    async fn abandoned_checkouts_carry_recovery_links_and_walk_newest_first() {
        let t = Arc::new(FakeTransport::new());
        data(
            &t,
            json!({"abandonedCheckouts": {"pageInfo": {"hasNextPage": true, "endCursor": "c1"}}}),
        );
        data(
            &t,
            json!({"abandonedCheckouts": {"nodes": [{
            "id": "gid://shopify/AbandonedCheckout/8", "name": "#8",
            "abandonedCheckoutUrl": "https://acme.myshopify.com/recover/8", "updatedAt": "2026-10-17T10:00:00Z",
            "customer": {"id": "gid://shopify/Customer/77", "firstName": "Jo", "lastName": "Doe",
                "defaultEmailAddress": {"emailAddress": "jo@example.com"}},
            "totalPriceSet": {"shopMoney": {"amount": "25.00", "currencyCode": "EUR"}},
            "lineItems": {"nodes": [{"title": "Mug", "variantTitle": "Blue", "quantity": 2,
                "variant": {"id": "gid://shopify/ProductVariant/11"},
                "discountedTotalPriceSet": {"shopMoney": {"amount": "20.00"}}}]}
        }], "pageInfo": {"hasNextPage": false}}}),
        );
        let page = shop(&t)
            .list_abandoned_carts(&CartQuery {
                since: chrono::NaiveDate::from_ymd_opt(2026, 10, 1),
                page: Some(2),
                per_page: Some(5),
            })
            .await
            .unwrap();
        let cart = &page.items[0];
        assert_eq!(
            cart.recovery_url.as_deref(),
            Some("https://acme.myshopify.com/recover/8")
        );
        assert_eq!(cart.email.as_deref(), Some("jo@example.com"));
        assert_eq!(cart.line_items[0].name, "Mug - Blue");
        assert_eq!(cart.total, Some(dec("25")));
        let b = sent(&t);
        assert_eq!(
            b[0]["variables"]["query"],
            "status:open updated_at:>=2026-10-01"
        );
        assert!(
            b[0]["query"].as_str().unwrap().contains("reverse: true"),
            "the walk sorts like the page"
        );
    }

    #[tokio::test]
    async fn later_order_pages_walk_cursors_in_the_same_order() {
        let t = Arc::new(FakeTransport::new());
        data(&t, json!({"ordersCount": {"count": 12}}));
        data(
            &t,
            json!({"orders": {"pageInfo": {"hasNextPage": true, "endCursor": "c1"}}}),
        );
        data(
            &t,
            json!({"orders": {"nodes": [order()], "pageInfo": {"hasNextPage": false}}}),
        );
        shop(&t)
            .list_orders(&OrderQuery {
                page: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(sent(&t)[1]["query"]
            .as_str()
            .unwrap()
            .contains("sortKey: CREATED_AT, reverse: true"));
    }

    #[tokio::test]
    async fn raw_queries_return_errors_as_data_and_reads_refuse_mutations() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            GQL,
            HttpResponse::new(
                200,
                r#"{"errors":[{"message":"Field 'nope' doesn't exist on type 'Shop'"}]}"#,
            ),
        );
        let s = shop(&t);
        let read = |body: &str| RawRequest {
            method: "GET".into(),
            body: Some(json!(body)),
            ..Default::default()
        };
        let r = s.raw(&read("{ shop { nope } }")).await.unwrap();
        assert_eq!(r.status, 200);
        assert!(r.body["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("nope"));
        for doc in [
            "mutation { productDelete(input: {id: \"x\"}) { deletedProductId } }",
            "query A { shop { name } }\nmutation B { productDelete(input: {id: \"x\"}) { deletedProductId } }",
            "# a comment\n  mutation{ x }",
        ] {
            let refused = s.raw(&read(doc)).await;
            assert!(matches!(refused, Err(ConnectorError::Api { status: 400, .. })), "{doc}");
        }
        assert!(!is_mutation(
            "{ products(query: \"mutation\") { nodes { id } } }"
        ));
        let no_body = s
            .raw(&RawRequest {
                method: "POST".into(),
                ..Default::default()
            })
            .await;
        assert!(matches!(
            no_body,
            Err(ConnectorError::Api { status: 400, .. })
        ));
        assert_eq!(t.requests().len(), 1);
    }
}
