//! WooCommerce through the real reqwest transport against a local mock server.

use std::sync::Arc;
use std::time::Duration;

use connectors::domain::ProductQuery;
use connectors::woocommerce::WooCommerce;
use connectors::{Credentials, ReqwestTransport, ShopConnector};
use wiremock::matchers::{method, path, query_param, query_param_contains};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds() -> Credentials {
    Credentials::from([
        ("consumer_key".into(), "ck_live".into()),
        ("consumer_secret".into(), "cs_live".into()),
    ])
}

#[tokio::test]
async fn plain_http_store_gets_oauth_signed_requests_and_transient_errors_are_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wp-json/wc/v3/products"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wp-json/wc/v3/products"))
        .and(query_param("oauth_consumer_key", "ck_live"))
        .and(query_param("oauth_signature_method", "HMAC-SHA256"))
        .and(query_param_contains("oauth_signature", ""))
        .and(query_param("search", "blue hoodie"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-WP-Total", "1")
                .insert_header("X-WP-TotalPages", "1")
                .set_body_raw(
                    include_str!("fixtures/woocommerce/product.json")
                        .replace('{', "[{")
                        .replacen("}\n", "}]\n", 1)
                        .trim_end()
                        .to_string(),
                    "application/json",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let transport =
        ReqwestTransport::new(reqwest::Client::new()).with_backoff(Duration::from_millis(1), 2);
    let woo = WooCommerce::new(&server.uri(), &creds(), Arc::new(transport));
    let page = woo
        .list_products(&ProductQuery {
            search: Some("blue hoodie".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.total, Some(1));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "one 503 then one retry");
    assert!(
        !requests[1].headers.contains_key("authorization"),
        "no Basic auth over plain HTTP"
    );
}
