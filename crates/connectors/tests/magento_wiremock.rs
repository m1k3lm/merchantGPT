//! Magento through the real reqwest transport: bearer auth and the bracketed
//! `searchCriteria` query encoding.

use std::sync::Arc;

use connectors::domain::OrderQuery;
use connectors::magento::Magento;
use connectors::{Credentials, ReqwestTransport, ShopConnector};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn bearer_token_and_search_criteria_reach_magento() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/V1/orders"))
        .and(header("authorization", "Bearer integration-token"))
        .and(header("user-agent", connectors::transport::USER_AGENT))
        .and(query_param(
            "searchCriteria[filter_groups][0][filters][0][field]",
            "status",
        ))
        .and(query_param(
            "searchCriteria[filter_groups][0][filters][0][value]",
            "processing",
        ))
        .and(query_param("searchCriteria[pageSize]", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/magento/orders.json"),
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let creds = Credentials::from([("access_token".into(), "integration-token".into())]);
    let magento = Magento::new(&server.uri(), &creds, Arc::new(ReqwestTransport::default()));
    let page = magento
        .list_orders(&OrderQuery {
            status: Some("processing".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].number, "000000041");
}
