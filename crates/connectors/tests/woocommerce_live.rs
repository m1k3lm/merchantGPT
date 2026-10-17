//! Smoke test against a real WooCommerce store (the `woo` compose profile).
//! Ignored by default; see CLAUDE.md for how to run it.
//!
//! `WOO_RESOLVE` maps the store's host to another address, so the container
//! can reach `localhost:8080` on the Docker host while the URL (and thus the
//! OAuth signature base) stays the store's configured home URL.

use std::sync::Arc;

use chrono::Local;
use connectors::domain::{DateRange, OrderQuery, ProductPatch, ProductQuery};
use connectors::woocommerce::WooCommerce;
use connectors::{Credentials, ReqwestTransport, ShopConnector};

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set"))
}

async fn client() -> WooCommerce {
    let url = env("WOO_URL");
    let mut builder = reqwest::Client::builder();
    if let Ok(target) = std::env::var("WOO_RESOLVE") {
        let host = reqwest::Url::parse(&url)
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        let addr = tokio::net::lookup_host(&target)
            .await
            .unwrap()
            .next()
            .unwrap();
        builder = builder.resolve(&host, addr);
    }
    let creds = Credentials::from([
        ("consumer_key".into(), env("WOO_CK")),
        ("consumer_secret".into(), env("WOO_CS")),
    ]);
    WooCommerce::new(
        &url,
        &creds,
        Arc::new(ReqwestTransport::new(builder.build().unwrap())),
    )
}

#[tokio::test]
#[ignore = "needs a live WooCommerce store"]
async fn live_store_roundtrip() {
    let woo = client().await;
    woo.verify().await.expect("credentials accepted");

    let products = woo
        .list_products(&ProductQuery {
            search: Some("blue hoodie".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let hoodie = products.items.first().expect("seeded product");
    assert_eq!(hoodie.sku.as_deref(), Some("HOOD-BLU"));

    let updated = woo
        .update_product(
            &hoodie.id,
            &ProductPatch {
                stock_quantity: Some(6),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.stock_quantity, Some(6));

    let orders = woo.list_orders(&OrderQuery::default()).await.unwrap();
    assert!(!orders.items.is_empty());
    let order = woo.get_order(&orders.items[0].id).await.unwrap();
    assert!(!order.line_items.is_empty());

    let today = Local::now().date_naive();
    let report = woo
        .sales_report(&DateRange {
            from: today.pred_opt().unwrap(),
            to: today,
        })
        .await
        .unwrap();
    println!("live report: {report:?}");
}
