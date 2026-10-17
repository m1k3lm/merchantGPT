//! WooCommerce REST API v3 connector (`{shop}/wp-json/wc/v3`).

mod auth;
mod types;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Days;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};

use crate::domain::*;
use crate::transport::{HttpResponse, HttpTransport, Method};
use crate::{ConnectorError, Credentials, Platform, ShopConnector};
pub use auth::WooAuth;
use types::{WooCustomer, WooOrder, WooProduct, WooSalesReport};

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
            body.insert("sale_price".into(), json!(v.to_string()));
        }
        if let Some(v) = patch.stock_quantity {
            // WooCommerce ignores stock_quantity unless stock management is on.
            body.insert("manage_stock".into(), json!(true));
            body.insert("stock_quantity".into(), json!(v));
        }
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
}
