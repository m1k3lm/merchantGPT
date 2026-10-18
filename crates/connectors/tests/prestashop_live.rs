//! Against the `presta` compose store (PrestaShop demo data). Ignored by
//! default; run with PRESTA_URL and PRESTA_KEY from scripts/presta-setup.sh.
//! It changes product demo_11 and one order, and puts them back.

use std::str::FromStr;
use std::sync::Arc;

use chrono::Local;
use connectors::domain::*;
use connectors::prestashop::{PrestaShop, WEBSERVICE_KEY};
use connectors::{ConnectorError, Credentials, ReqwestTransport, ShopConnector};
use rust_decimal::Decimal;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set"))
}

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

#[tokio::test]
#[ignore = "needs a live PrestaShop store"]
async fn reads_and_writes_a_live_store() {
    let creds = Credentials::from([(WEBSERVICE_KEY.into(), env("PRESTA_KEY"))]);
    let shop = PrestaShop::new(
        &env("PRESTA_URL"),
        &creds,
        Arc::new(ReqwestTransport::default()),
    );
    shop.verify().await.unwrap();

    let first = shop
        .list_products(&ProductQuery {
            per_page: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(first.items.len(), 2);
    assert_eq!(first.total_pages, None, "more pages follow");
    let found = shop
        .list_products(&ProductQuery {
            search: Some("sweater".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        found.items.iter().any(|p| p.name.contains("sweater")),
        "{found:?}"
    );
    assert_eq!(found.total_pages, Some(1));

    let mug = shop
        .list_products(&ProductQuery {
            sku: Some("demo_11".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .items
        .remove(0);
    let (name, regular, stock) = (
        mug.name.clone(),
        mug.regular_price.unwrap(),
        mug.stock_quantity.unwrap(),
    );
    let changed = shop
        .update_product(
            &mug.id,
            &ProductPatch {
                name: Some(format!("{name} (test)")),
                regular_price: Some(dec("20")),
                sale_price: Some(dec("15.5")),
                stock_quantity: Some(stock + 3),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(changed.name, format!("{name} (test)"));
    assert_eq!(changed.regular_price, Some(dec("20")));
    assert_eq!(changed.sale_price, Some(dec("15.5")));
    assert_eq!(changed.price, Some(dec("15.5")));
    assert_eq!(changed.stock_quantity, Some(stock + 3));
    let restored = shop
        .update_product(
            &mug.id,
            &ProductPatch {
                name: Some(name.clone()),
                regular_price: Some(regular),
                sale_price: Some(Decimal::ZERO),
                stock_quantity: Some(stock),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (restored.name, restored.sale_price, restored.stock_quantity),
        (name, None, Some(stock))
    );
    let shirt = shop
        .update_product(
            "1",
            &ProductPatch {
                stock_quantity: Some(1),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(shirt, Err(ConnectorError::Api { status: 400, .. })),
        "{shirt:?}"
    );

    let orders = shop.list_orders(&OrderQuery::default()).await.unwrap();
    let order = orders.items.last().unwrap().clone();
    assert!(!order.line_items.is_empty());
    assert!(order.customer_name.is_some(), "{order:?}");
    assert_eq!(order.currency, "EUR");
    let by_ref = shop.get_order(&order.number).await.unwrap();
    assert_eq!(by_ref.id, order.id);
    let moved = shop
        .update_order_status(&order.id, "Processing in progress")
        .await
        .unwrap();
    assert_eq!(moved.status, "Processing in progress");
    let processing = shop
        .list_orders(&OrderQuery {
            status: Some("processing".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(processing.items.iter().any(|o| o.id == order.id));
    // A processing order counts as a sale.
    let today = Local::now().date_naive();
    let report = shop
        .sales_report(&DateRange {
            from: today - chrono::Days::new(30),
            to: today,
        })
        .await
        .unwrap();
    assert!(report.orders >= 1, "{report:?}");
    assert!(report.total_sales > Decimal::ZERO);
    shop.update_order_status(&order.id, &order.status)
        .await
        .unwrap();

    let customers = shop
        .list_customers(&CustomerQuery {
            search: Some("doe".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(customers
        .items
        .iter()
        .any(|c| c.last_name.eq_ignore_ascii_case("doe")));
}
