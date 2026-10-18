//! Against the `presta` compose store (PrestaShop demo data). Ignored by
//! default; run with PRESTA_URL and PRESTA_KEY from scripts/presta-setup.sh.
//! They change product demo_11 and one order and put them back, and create
//! then delete one record of every kind (category, product, customer,
//! addresses, coupon, order, cart).

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

#[tokio::test]
#[ignore = "needs a live PrestaShop store"]
async fn a_missing_order_is_reported_as_not_found() {
    let creds = Credentials::from([(WEBSERVICE_KEY.into(), env("PRESTA_KEY"))]);
    let shop = PrestaShop::new(
        &env("PRESTA_URL"),
        &creds,
        Arc::new(ReqwestTransport::default()),
    );
    assert_eq!(
        shop.get_order("999999").await.unwrap_err(),
        ConnectorError::NotFound("orders/999999".into())
    );
}

fn live_shop() -> PrestaShop {
    let creds = Credentials::from([(WEBSERVICE_KEY.into(), env("PRESTA_KEY"))]);
    PrestaShop::new(
        &env("PRESTA_URL"),
        &creds,
        Arc::new(ReqwestTransport::default()),
    )
}

/// Creates, changes and deletes one of everything; every record it makes
/// carries a unique suffix and is removed at the end.
#[tokio::test]
#[ignore = "needs a live PrestaShop store"]
async fn creates_changes_and_deletes_records_on_a_live_store() {
    let shop = live_shop();
    let tag = Local::now().format("%H%M%S%3f").to_string();

    let category = shop
        .create_category(&CategoryPatch {
            name: Some(format!("MGPT {tag}")),
            description: Some("Test category".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let category = shop
        .update_category(
            &category.id,
            &CategoryPatch {
                name: Some(format!("MGPT renamed {tag}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(category.name, format!("MGPT renamed {tag}"));
    let found = shop
        .list_categories(&CategoryQuery {
            search: Some(format!("renamed {tag}")),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.items.len(), 1);

    let product = shop
        .create_product(&ProductPatch {
            name: Some(format!("MGPT mug {tag}")),
            sku: Some(format!("MGPT-{tag}")),
            regular_price: Some(dec("12.50")),
            sale_price: Some(dec("10")),
            stock_quantity: Some(5),
            description: Some("<p>A test mug</p>".into()),
            category_ids: Some(vec![category.id.clone()]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        (
            product.regular_price,
            product.sale_price,
            product.stock_quantity
        ),
        (Some(dec("12.5")), Some(dec("10")), Some(5))
    );
    assert_eq!(product.category_ids, std::slice::from_ref(&category.id));
    let product = shop
        .update_product(
            &product.id,
            &ProductPatch {
                description: Some("<p>Updated</p>".into()),
                category_ids: Some(vec!["2".into(), category.id.clone()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let product = shop.get_product(&product.id).await.unwrap();
    assert_eq!(product.category_ids.len(), 2, "{product:?}");

    let customer = shop
        .create_customer(&CustomerPatch {
            email: Some(format!("mgpt-{tag}@example.com")),
            first_name: Some("Test".into()),
            last_name: Some("Buyer".into()),
            accepts_marketing: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(customer.accepts_marketing, Some(true));
    let customer = shop
        .update_customer(
            &customer.id,
            &CustomerPatch {
                last_name: Some("Shopper".into()),
                accepts_marketing: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (customer.last_name.as_str(), customer.accepts_marketing),
        ("Shopper", Some(false))
    );

    let paris = AddressPatch {
        address1: Some("1 rue de Rivoli".into()),
        city: Some("Paris".into()),
        postcode: Some("75001".into()),
        country: Some("FR".into()),
        phone: Some("0600000000".into()),
        ..Default::default()
    };
    let address = shop.create_address(&customer.id, &paris).await.unwrap();
    assert_eq!(address.country.as_deref(), Some("FR"));
    let address = shop
        .update_address(
            &customer.id,
            &address.id,
            &AddressPatch {
                city: Some("Lyon".into()),
                postcode: Some("69001".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(address.city.as_deref(), Some("Lyon"));
    let with_phone = shop.get_customer(&customer.id).await.unwrap();
    assert_eq!(with_phone.phone.as_deref(), Some("0600000000"));
    let spain = shop
        .create_address(
            &customer.id,
            &AddressPatch {
                country: Some("ES".into()),
                state: Some("M".into()),
                ..paris.clone()
            },
        )
        .await;
    assert!(
        matches!(spain, Err(ConnectorError::Api { status: 400, ref message }) if message.contains("DNI")),
        "{spain:?}"
    );
    let spain = shop
        .create_address(
            &customer.id,
            &AddressPatch {
                country: Some("ES".into()),
                state: Some("M".into()),
                id_number: Some("12345678Z".into()),
                ..paris.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (spain.country.as_deref(), spain.id_number.as_deref()),
        (Some("ES"), Some("12345678Z"))
    );
    shop.delete_address(&customer.id, &spain.id).await.unwrap();

    let coupon = shop
        .create_coupon(&CouponPatch {
            code: Some(format!("MGPT{tag}")),
            kind: Some("percent".into()),
            amount: Some(dec("10")),
            expires_on: chrono::NaiveDate::from_ymd_opt(2027, 1, 31),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        (coupon.kind.as_str(), coupon.amount),
        ("percent", dec("10"))
    );
    let coupon = shop
        .update_coupon(
            &coupon.id,
            &CouponPatch {
                kind: Some("fixed_cart".into()),
                amount: Some(dec("5")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (coupon.kind.as_str(), coupon.amount),
        ("fixed_cart", dec("5"))
    );
    let listed = shop
        .list_coupons(&CouponQuery {
            code: Some(format!("MGPT{tag}")),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 1);

    let order = shop
        .create_order(&NewOrder {
            customer_id: Some(customer.id.clone()),
            line_items: vec![NewLineItem {
                product_id: product.id.clone(),
                quantity: 2,
                price: None,
            }],
            note: Some("Phone order".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(order.total, dec("20"), "{order:?}");
    assert_eq!(order.line_items[0].quantity, 2);
    shop.add_order_note(
        &order.id,
        &OrderNote {
            note: "Called the customer".into(),
            notify_customer: false,
        },
    )
    .await
    .unwrap();
    let notify = shop
        .add_order_note(
            &order.id,
            &OrderNote {
                note: "Hi".into(),
                notify_customer: true,
            },
        )
        .await;
    assert!(matches!(notify, Err(ConnectorError::Unsupported(_))));
    let note = shop
        .raw(&RawRequest {
            method: "GET".into(),
            path: "/orders".into(),
            query: [
                ("display".to_string(), "[id,note]".to_string()),
                ("filter[id]".to_string(), format!("[{}]", order.id)),
            ]
            .into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        note.body["orders"][0]["note"],
        "Phone order\nCalled the customer"
    );

    let cart = shop
        .raw(&RawRequest {
            method: "POST".into(),
            path: "/carts".into(),
            body: Some(serde_json::Value::String(format!(
                "<prestashop><cart><id_customer>{c}</id_customer><id_address_delivery>{a}</id_address_delivery><id_address_invoice>{a}</id_address_invoice><id_currency>1</id_currency><id_lang>1</id_lang><associations><cart_rows><cart_row><id_product>{p}</id_product><id_product_attribute>0</id_product_attribute><id_address_delivery>{a}</id_address_delivery><quantity>3</quantity></cart_row></cart_rows></associations></cart></prestashop>",
                c = customer.id,
                a = address.id,
                p = product.id
            ))),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(cart.status, 201, "{:?}", cart.body);
    let cart_id = cart.body["cart"]["id"].as_str().unwrap().to_string();
    let carts = shop
        .list_abandoned_carts(&CartQuery {
            since: Some(Local::now().date_naive() - chrono::Days::new(1)),
            per_page: Some(50),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        carts.items.iter().all(|c| c.id != cart_id),
        "a cart changed just now is not abandoned yet: {carts:?}"
    );

    let deleted_cart = shop
        .raw(&RawRequest {
            method: "DELETE".into(),
            path: format!("/carts/{cart_id}"),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(deleted_cart.status, 200);
    shop.delete_order(&order.id).await.unwrap();
    assert!(matches!(
        shop.get_order(&order.id).await,
        Err(ConnectorError::NotFound(_))
    ));
    shop.delete_coupon(&coupon.id).await.unwrap();
    for a in shop.list_addresses(&customer.id).await.unwrap() {
        shop.delete_address(&customer.id, &a.id).await.unwrap();
    }
    shop.delete_customer(&customer.id).await.unwrap();
    shop.delete_product(&product.id).await.unwrap();
    shop.delete_category(&category.id).await.unwrap();
    assert!(matches!(
        shop.get_product(&product.id).await,
        Err(ConnectorError::NotFound(_))
    ));
}
