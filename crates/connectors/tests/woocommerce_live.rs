//! Smoke test against a real WooCommerce store (the `woo` compose profile).
//! Ignored by default; see CLAUDE.md for how to run it.
//!
//! `WOO_RESOLVE` maps the store's host to another address, so the container
//! can reach `localhost:8080` on the Docker host while the URL (and thus the
//! OAuth signature base) stays the store's configured home URL.

use std::sync::Arc;

use std::str::FromStr;

use chrono::Local;
use connectors::domain::*;
use connectors::woocommerce::WooCommerce;
use connectors::{ConnectorError, Credentials, ReqwestTransport, ShopConnector};
use rust_decimal::Decimal;
use serde_json::json;

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

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

/// Creates, changes and deletes one of everything; leaves the store as found.
#[tokio::test]
#[ignore = "needs a live WooCommerce store"]
async fn live_store_writes_every_entity() {
    let woo = client().await;
    let tag = chrono::Utc::now().timestamp_millis().to_string();

    // Categories and products.
    let category = woo
        .create_category(&CategoryPatch {
            name: Some(format!("MGPT test {tag}")),
            description: Some("temporary".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let renamed = woo
        .update_category(
            &category.id,
            &CategoryPatch {
                name: Some(format!("MGPT renamed {tag}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.description.as_deref(), Some("temporary"));
    let found = woo
        .list_categories(&CategoryQuery {
            search: Some(format!("renamed {tag}")),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.items.len(), 1, "{found:?}");

    let product = woo
        .create_product(&ProductPatch {
            name: Some(format!("MGPT mug {tag}")),
            sku: Some(format!("MGPT-{tag}")),
            regular_price: Some(dec("12.50")),
            sale_price: Some(dec("9.90")),
            stock_quantity: Some(4),
            description: Some("<p>Test mug</p>".into()),
            category_ids: Some(vec![category.id.clone()]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(product.category_ids, std::slice::from_ref(&category.id));
    assert_eq!(product.sale_price, Some(dec("9.90")));
    assert_eq!(product.stock_quantity, Some(4));
    let ended = woo
        .update_product(
            &product.id,
            &ProductPatch {
                sale_price: Some(Decimal::ZERO),
                category_ids: Some(vec![]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ended.sale_price, None, "a zero sale price ends the sale");
    assert_eq!(ended.price, Some(dec("12.50")));

    // Customers and their two address slots.
    let customer = woo
        .create_customer(&CustomerPatch {
            email: Some(format!("mgpt-{tag}@example.com")),
            first_name: Some("Test".into()),
            last_name: Some("Customer".into()),
            phone: Some("+34600000000".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(customer.phone.as_deref(), Some("+34600000000"));
    let consent = woo
        .update_customer(
            &customer.id,
            &CustomerPatch {
                accepts_marketing: Some(true),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(consent, Err(ConnectorError::Unsupported(_))),
        "{consent:?}"
    );
    let updated = woo
        .update_customer(
            &customer.id,
            &CustomerPatch {
                last_name: Some("Renamed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.last_name, "Renamed");

    let billing = woo
        .create_address(
            &customer.id,
            &AddressPatch {
                kind: Some("billing".into()),
                first_name: Some("Test".into()),
                last_name: Some("Customer".into()),
                address1: Some("Calle Mayor 1".into()),
                city: Some("Madrid".into()),
                postcode: Some("28013".into()),
                country: Some("ES".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(billing.id, "billing");
    assert_eq!(billing.phone, None, "create replaces the whole slot");
    woo.create_address(
        &customer.id,
        &AddressPatch {
            kind: Some("shipping".into()),
            address1: Some("Gran Vía 2".into()),
            city: Some("Madrid".into()),
            country: Some("ES".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let moved = woo
        .update_address(
            &customer.id,
            "billing",
            &AddressPatch {
                city: Some("Barcelona".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (moved.city.as_deref(), moved.address1.as_deref()),
        (Some("Barcelona"), Some("Calle Mayor 1"))
    );
    woo.delete_address(&customer.id, "shipping").await.unwrap();
    let slots: Vec<String> = woo
        .list_addresses(&customer.id)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(slots, ["billing"]);

    // Orders: created for the customer, with a price override and a note.
    let order = woo
        .create_order(&NewOrder {
            customer_id: Some(customer.id.clone()),
            line_items: vec![NewLineItem {
                product_id: product.id.clone(),
                quantity: 2,
                price: Some(dec("10.00")),
            }],
            status: Some("on-hold".into()),
            note: Some("Phone order, test".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(order.status, "on-hold");
    assert_eq!(order.customer_id.as_deref(), Some(customer.id.as_str()));
    assert_eq!(
        order.customer_email,
        Some(format!("mgpt-{tag}@example.com"))
    );
    assert_eq!(order.line_items[0].total, dec("20.00"));
    assert!(order.total >= dec("20.00"), "{order:?}");
    woo.add_order_note(
        &order.id,
        &OrderNote {
            note: "Ready to ship tomorrow".into(),
            notify_customer: false,
        },
    )
    .await
    .unwrap();
    let notes = woo
        .raw(&RawRequest {
            method: "GET".into(),
            path: format!("/orders/{}/notes", order.id),
            ..Default::default()
        })
        .await
        .unwrap();
    let texts: Vec<&str> = notes
        .body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["note"].as_str())
        .collect();
    assert!(texts.contains(&"Ready to ship tomorrow"), "{texts:?}");
    assert!(texts.contains(&"Phone order, test"), "{texts:?}");
    woo.delete_order(&order.id).await.unwrap();
    assert!(matches!(
        woo.get_order(&order.id).await,
        Err(ConnectorError::NotFound(_))
    ));

    // Coupons: a draft does not apply until activated.
    let code = format!("mgpt{tag}");
    let coupon = woo
        .create_coupon(&CouponPatch {
            code: Some(code.clone()),
            kind: Some("percent".into()),
            amount: Some(dec("15")),
            expires_on: chrono::NaiveDate::from_ymd_opt(2030, 1, 31),
            usage_limit: Some(100),
            minimum_amount: Some(dec("30")),
            active: Some(false),
            description: Some("test coupon".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        (coupon.kind.as_str(), coupon.amount, coupon.active),
        ("percent", dec("15"), Some(false))
    );
    assert_eq!(
        coupon.expires_on,
        chrono::NaiveDate::from_ymd_opt(2030, 1, 31)
    );
    assert_eq!(coupon.minimum_amount, Some(dec("30")));
    let active = woo
        .update_coupon(
            &coupon.id,
            &CouponPatch {
                active: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(active.active, Some(true));
    let listed = woo
        .list_coupons(&CouponQuery {
            code: Some(code),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 1);
    woo.delete_coupon(&coupon.id).await.unwrap();

    // The raw API: a signed write with a query parameter, and an error status.
    let created = woo
        .raw(&RawRequest {
            method: "POST".into(),
            path: "products/tags".into(),
            body: Some(json!({"name": format!("mgpt tag {tag}")})),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(created.status, 201, "{created:?}");
    let tag_id = created.body["id"].as_u64().unwrap();
    let gone = woo
        .raw(&RawRequest {
            method: "DELETE".into(),
            path: format!("/products/tags/{tag_id}"),
            query: [("force".to_string(), "true".to_string())].into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(gone.status, 200, "{gone:?}");
    let missing = woo
        .raw(&RawRequest {
            method: "GET".into(),
            path: "/products/tags/999999999".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(missing.status, 404);

    woo.delete_customer(&customer.id).await.unwrap();
    woo.delete_product(&product.id).await.unwrap();
    woo.delete_category(&category.id).await.unwrap();
    assert!(matches!(
        woo.get_product(&product.id).await,
        Err(ConnectorError::NotFound(_))
    ));
}

/// Adds one processing order shipped to Madrid and deletes it afterwards.
#[tokio::test]
#[ignore = "needs a live WooCommerce store"]
async fn sales_by_location_counts_the_reports_orders() {
    let woo = client().await;
    let hoodie = woo
        .list_products(&ProductQuery {
            search: Some("blue hoodie".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .items
        .remove(0);
    let address = |state: &str| AddressPatch {
        first_name: Some("Ana".into()),
        last_name: Some("Test".into()),
        address1: Some("Gran Via 1".into()),
        city: Some("Madrid".into()),
        postcode: Some("28013".into()),
        state: Some(state.into()),
        country: Some("ES".into()),
        ..Default::default()
    };
    let order = woo
        .create_order(&NewOrder {
            email: Some("ana-location@example.com".into()),
            line_items: vec![NewLineItem {
                product_id: hoodie.id.clone(),
                quantity: 1,
                price: None,
            }],
            billing: Some(address("B")),
            shipping: Some(address("M")),
            status: Some("processing".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    // The store's day may differ from this computer's around midnight.
    let today = Local::now().date_naive();
    let range = DateRange {
        from: today - chrono::Days::new(1),
        to: today + chrono::Days::new(1),
    };
    let report = woo.sales_report(&range).await.unwrap();
    let places = woo.sales_by_location(&range).await;
    woo.delete_order(&order.id).await.unwrap();
    let places = places.unwrap();
    println!("live places: {places:?}");
    let madrid = places
        .rows
        .iter()
        .find(|r| r.country == "ES" && r.region.as_deref() == Some("M"))
        .expect("the shipping address decides the place");
    assert!(madrid.total_sales >= order.total);
    assert_eq!(
        places.rows.iter().map(|r| r.orders).sum::<u64>(),
        report.orders
    );
    assert_eq!(
        places.rows.iter().map(|r| r.total_sales).sum::<Decimal>(),
        report.total_sales
    );
}
