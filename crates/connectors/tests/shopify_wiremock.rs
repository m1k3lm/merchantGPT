//! Shopify through the real reqwest transport: the client credentials grant,
//! the access token header and the GraphQL request body.

use std::sync::Arc;

use connectors::domain::ProductQuery;
use connectors::shopify::{Shopify, CLIENT_ID, CLIENT_SECRET};
use connectors::{Credentials, ReqwestTransport, ShopConnector};
use serde_json::json;
use wiremock::matchers::{body_partial_json, body_string, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn client_credentials_token_reaches_the_graphql_admin_api() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/admin/oauth/access_token"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .and(body_string(
            "grant_type=client_credentials&client_id=app-id&client_secret=app-secret",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": "shpat_live", "scope": "read_products", "expires_in": 86399}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/admin/api/2026-10/graphql.json"))
        .and(header("x-shopify-access-token", "shpat_live"))
        .and(header("user-agent", connectors::transport::USER_AGENT))
        .and(body_partial_json(
            json!({"variables": {"first": 10, "query": "sku:\"HOOD-BLU\""}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"productVariants": {
            "nodes": [{
                "id": "gid://shopify/ProductVariant/11", "title": "Default Title", "sku": "HOOD-BLU",
                "price": "39.90", "compareAtPrice": null, "inventoryQuantity": 5,
                "inventoryItem": {"id": "gid://shopify/InventoryItem/70", "tracked": true},
                "product": {"id": "gid://shopify/Product/9", "title": "Hoodie", "status": "ACTIVE"}
            }],
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        }}})))
        .expect(1)
        .mount(&server)
        .await;

    let creds = Credentials::from([
        (CLIENT_ID.into(), "app-id".into()),
        (CLIENT_SECRET.into(), "app-secret".into()),
    ]);
    let shop = Shopify::new(&server.uri(), &creds, Arc::new(ReqwestTransport::default())).unwrap();
    let page = shop
        .list_products(&ProductQuery {
            sku: Some("HOOD-BLU".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items[0].name, "Hoodie");
    assert_eq!(page.items[0].stock_quantity, Some(5));
}
