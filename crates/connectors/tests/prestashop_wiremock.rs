//! PrestaShop through the real reqwest transport: Basic auth with the key as
//! user, JSON output and the bracketed filter syntax.

use std::sync::Arc;

use connectors::domain::ProductQuery;
use connectors::prestashop::{PrestaShop, WEBSERVICE_KEY};
use connectors::{Credentials, ReqwestTransport, ShopConnector};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn key_and_filters_reach_the_webservice() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/shop/api/products"))
        .and(header("authorization", "Basic S0VZOg=="))
        .and(header("user-agent", connectors::transport::USER_AGENT))
        .and(query_param("output_format", "JSON"))
        .and(query_param("filter[reference]", "[demo_11]"))
        .and(query_param("price[final][use_tax]", "0"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"products": [{
                "id": 6, "reference": "demo_11", "price": "11.900000", "final": "9.900000",
                "active": "1", "cache_default_attribute": 0,
                "name": [{"id": 1, "value": "Mug"}]
            }]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/shop/api/stock_availables"))
        .and(query_param("filter[id_product]", "[6]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"stock_availables": [{"id": 40, "id_product": 6, "quantity": 12}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let creds = Credentials::from([(WEBSERVICE_KEY.into(), "KEY".into())]);
    let shop = PrestaShop::new(
        &format!("{}/shop", server.uri()),
        &creds,
        Arc::new(ReqwestTransport::default()),
    );
    let page = shop
        .list_products(&ProductQuery {
            sku: Some("demo_11".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mug = &page.items[0];
    assert_eq!((mug.name.as_str(), mug.stock_quantity), ("Mug", Some(12)));
    assert_eq!(
        mug.sale_price.map(|d| d.to_string()).as_deref(),
        Some("9.9")
    );
}
