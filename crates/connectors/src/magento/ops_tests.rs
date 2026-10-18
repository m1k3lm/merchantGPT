//! Requests and mappings of the create, update and delete operations.

use std::str::FromStr;
use std::sync::Arc;

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde_json::{json, Value};

use super::Magento;
use crate::domain::*;
use crate::transport::{FakeTransport, HttpRequest, HttpResponse, Method};
use crate::{ConnectorError, Credentials, ShopConnector};

fn magento(t: &Arc<FakeTransport>) -> Magento {
    let creds = Credentials::from([("access_token".into(), "tok123".into())]);
    Magento::new("https://shop.test/", &creds, t.clone())
}

fn ok(t: &FakeTransport, method: Method, path: &str, body: Value) {
    t.on(method, path, HttpResponse::new(200, body.to_string()));
}

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn query(req: &HttpRequest, key: &str) -> Option<String> {
    req.query_pairs()
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// The JSON body of the first request to `method path`.
fn body(t: &FakeTransport, method: Method, path: &str) -> Value {
    let req = t
        .requests()
        .into_iter()
        .find(|r| r.method == method && r.path() == path)
        .unwrap_or_else(|| panic!("no {method:?} {path}"));
    serde_json::from_str(req.body.as_deref().unwrap_or("null")).unwrap()
}

fn sent(t: &FakeTransport, method: Method, path: &str) -> bool {
    t.requests()
        .iter()
        .any(|r| r.method == method && r.path() == path)
}

fn customer() -> Value {
    json!({
        "id": 12, "email": "jo@example.com", "firstname": "Jo", "lastname": "Doe",
        "website_id": 1, "default_billing": "30", "default_shipping": "30",
        "extension_attributes": {"is_subscribed": false},
        "addresses": [{
            "id": 30, "customer_id": 12, "firstname": "Jo", "lastname": "Doe",
            "street": ["Calle Mayor 1", "2B"], "city": "Madrid", "postcode": "28013",
            "country_id": "ES", "telephone": "600111222",
            "region": {"region": "Madrid", "region_code": "M", "region_id": 158},
            "region_id": 158, "default_billing": true, "default_shipping": true
        }]
    })
}

fn spain(t: &FakeTransport) {
    ok(
        t,
        Method::Get,
        "/rest/V1/directory/countries/ES",
        json!({"id": "ES", "available_regions": [
            {"id": 158, "code": "M", "name": "Madrid"},
            {"id": 139, "code": "B", "name": "Barcelona"}
        ]}),
    );
}

#[tokio::test]
async fn create_product_needs_a_sku_and_assigns_every_website() {
    let t = Arc::new(FakeTransport::new());
    let m = magento(&t);
    let nameless = ProductPatch {
        name: Some("Mug".into()),
        ..Default::default()
    };
    assert!(matches!(
        m.create_product(&nameless).await,
        Err(ConnectorError::Api { status: 400, .. })
    ));
    assert!(t.requests().is_empty());

    ok(
        &t,
        Method::Get,
        "/rest/V1/store/websites",
        json!([{"id": 0}, {"id": 1}]),
    );
    ok(
        &t,
        Method::Post,
        "/rest/all/V1/products",
        json!({"sku": "MUG-1", "name": "Mug", "price": 9.9, "status": 1,
            "extension_attributes": {"category_links": [{"position": 0, "category_id": "5"}]}}),
    );
    let p = m
        .create_product(&ProductPatch {
            name: Some("Mug".into()),
            sku: Some("MUG-1".into()),
            regular_price: Some(dec("9.90")),
            description: Some("<p>Big</p>".into()),
            category_ids: Some(vec!["5".into()]),
            stock_quantity: Some(3),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        (p.id.as_str(), p.category_ids.clone()),
        ("MUG-1", vec!["5".to_string()])
    );
    let b = body(&t, Method::Post, "/rest/all/V1/products");
    let product = &b["product"];
    assert_eq!(
        (
            &product["attribute_set_id"],
            &product["type_id"],
            &product["visibility"],
            &product["status"]
        ),
        (&json!(4), &json!("simple"), &json!(4), &json!(1))
    );
    assert_eq!(
        product["custom_attributes"],
        json!([{"attribute_code": "description", "value": "<p>Big</p>"}])
    );
    assert_eq!(
        product["extension_attributes"],
        json!({
            "stock_item": {"qty": 3, "is_in_stock": true},
            "category_links": [{"position": 0, "category_id": "5"}],
            "website_ids": [1]
        })
    );
}

#[tokio::test]
async fn delete_product_addresses_the_sku() {
    let t = Arc::new(FakeTransport::new());
    ok(
        &t,
        Method::Delete,
        "/rest/V1/products/HOOD%20BLU",
        json!(true),
    );
    magento(&t).delete_product("HOOD BLU").await.unwrap();
    assert!(sent(&t, Method::Delete, "/rest/V1/products/HOOD%20BLU"));
}

#[tokio::test]
async fn categories_list_create_move_and_delete() {
    let t = Arc::new(FakeTransport::new());
    let m = magento(&t);
    ok(
        &t,
        Method::Get,
        "/rest/V1/categories/list",
        json!({"items": [{"id": 5, "parent_id": 2, "name": "Mugs",
            "custom_attributes": [{"attribute_code": "description", "value": "All mugs"}]}],
            "total_count": 1}),
    );
    let page = m
        .list_categories(&CategoryQuery {
            search: Some("mug".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let c = &page.items[0];
    assert_eq!(
        (
            c.id.as_str(),
            c.parent_id.as_deref(),
            c.description.as_deref()
        ),
        ("5", Some("2"), Some("All mugs"))
    );
    assert_eq!(
        query(
            &t.requests()[0],
            "searchCriteria[filter_groups][0][filters][0][value]"
        )
        .as_deref(),
        Some("%mug%")
    );

    ok(
        &t,
        Method::Get,
        "/rest/V1/store/storeGroups",
        json!([{"id": 0, "root_category_id": 0}, {"id": 1, "root_category_id": 2}]),
    );
    ok(
        &t,
        Method::Post,
        "/rest/all/V1/categories",
        json!({"id": 6, "parent_id": 2, "name": "Cups"}),
    );
    m.create_category(&CategoryPatch {
        name: Some("Cups".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let created = body(&t, Method::Post, "/rest/all/V1/categories");
    assert_eq!(created["category"]["parent_id"], 2);
    assert_eq!(created["category"]["is_active"], true);

    ok(&t, Method::Put, "/rest/all/V1/categories/6", json!({}));
    ok(&t, Method::Put, "/rest/V1/categories/6/move", json!(true));
    ok(
        &t,
        Method::Get,
        "/rest/V1/categories/6",
        json!({"id": 6, "parent_id": 5, "name": "Big cups"}),
    );
    let moved = m
        .update_category(
            "6",
            &CategoryPatch {
                name: Some("Big cups".into()),
                parent_id: Some("5".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.parent_id.as_deref(), Some("5"));
    assert_eq!(
        body(&t, Method::Put, "/rest/V1/categories/6/move"),
        json!({"parentId": 5})
    );
    assert_eq!(
        body(&t, Method::Put, "/rest/all/V1/categories/6"),
        json!({"category": {"id": 6, "name": "Big cups"}})
    );

    ok(&t, Method::Delete, "/rest/V1/categories/6", json!(true));
    m.delete_category("6").await.unwrap();
    assert!(m.delete_category("cups").await.is_err());
}

#[tokio::test]
async fn customers_map_phone_and_consent_and_update_in_place() {
    let t = Arc::new(FakeTransport::new());
    let m = magento(&t);
    ok(&t, Method::Get, "/rest/V1/customers/12", customer());
    let c = m.get_customer("12").await.unwrap();
    assert_eq!(
        (c.phone.as_deref(), c.accepts_marketing),
        (Some("600111222"), Some(false))
    );

    let with_phone = CustomerPatch {
        email: Some("new@example.com".into()),
        phone: Some("600".into()),
        ..Default::default()
    };
    assert!(matches!(
        m.create_customer(&with_phone).await,
        Err(ConnectorError::Api { status: 400, .. })
    ));
    ok(
        &t,
        Method::Post,
        "/rest/V1/customers",
        json!({"id": 13, "email": "new@example.com"}),
    );
    m.create_customer(&CustomerPatch {
        email: Some("new@example.com".into()),
        first_name: Some("Ana".into()),
        last_name: Some("Ruiz".into()),
        accepts_marketing: Some(true),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        body(&t, Method::Post, "/rest/V1/customers"),
        json!({"customer": {"email": "new@example.com", "firstname": "Ana", "lastname": "Ruiz",
            "extension_attributes": {"is_subscribed": true}}})
    );

    ok(&t, Method::Put, "/rest/V1/customers/12", customer());
    m.update_customer(
        "12",
        &CustomerPatch {
            accepts_marketing: Some(true),
            phone: Some("699000000".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let saved = body(&t, Method::Put, "/rest/V1/customers/12")["customer"].clone();
    assert_eq!(saved["website_id"], 1, "the full customer is resent");
    assert_eq!(saved["extension_attributes"]["is_subscribed"], true);
    assert_eq!(saved["addresses"][0]["telephone"], "699000000");

    ok(&t, Method::Delete, "/rest/V1/customers/12", json!(true));
    m.delete_customer("12").await.unwrap();
}

#[tokio::test]
async fn addresses_are_saved_through_the_customer_with_resolved_regions() {
    let t = Arc::new(FakeTransport::new());
    let m = magento(&t);
    ok(&t, Method::Get, "/rest/V1/customers/12", customer());
    spain(&t);
    let mut after = customer();
    after["addresses"].as_array_mut().unwrap().push(json!({
        "id": 31, "firstname": "Jo", "lastname": "Doe", "street": ["Rambla 5"],
        "city": "Barcelona", "postcode": "08002", "country_id": "ES", "telephone": "600",
        "region": {"region": "Barcelona", "region_code": "B", "region_id": 139},
        "default_shipping": true
    }));
    ok(&t, Method::Put, "/rest/V1/customers/12", after);

    let listed = m.list_addresses("12").await.unwrap();
    assert_eq!(listed[0].kind.as_deref(), Some("billing,shipping"));
    assert_eq!(listed[0].address2.as_deref(), Some("2B"));

    let new = m
        .create_address(
            "12",
            &AddressPatch {
                kind: Some("shipping".into()),
                first_name: Some("Jo".into()),
                last_name: Some("Doe".into()),
                address1: Some("Rambla 5".into()),
                city: Some("Barcelona".into()),
                postcode: Some("08002".into()),
                state: Some("barcelona".into()),
                country: Some("es".into()),
                phone: Some("600".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(new.id, "31");
    let saved = body(&t, Method::Put, "/rest/V1/customers/12")["customer"].clone();
    let added = &saved["addresses"][1];
    assert_eq!(
        added["region"],
        json!({"region": "Barcelona", "region_code": "B", "region_id": 139})
    );
    assert_eq!(added["country_id"], "ES");
    assert_eq!(added["customer_id"], 12);
    assert_eq!(
        saved["addresses"][0]["default_shipping"], false,
        "the new address took the default"
    );
    assert_eq!(saved["addresses"][0]["default_billing"], true);

    let unknown_region = m
        .create_address(
            "12",
            &AddressPatch {
                state: Some("Narnia".into()),
                country: Some("ES".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        unknown_region.to_string().contains("Madrid"),
        "{unknown_region}"
    );
}

#[tokio::test]
async fn address_updates_and_deletes_stay_within_the_customer() {
    let t = Arc::new(FakeTransport::new());
    let m = magento(&t);
    ok(&t, Method::Get, "/rest/V1/customers/12", customer());
    ok(&t, Method::Put, "/rest/V1/customers/12", customer());
    m.update_address(
        "12",
        "30",
        &AddressPatch {
            address2: Some("3A".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let saved = body(&t, Method::Put, "/rest/V1/customers/12");
    assert_eq!(
        saved["customer"]["addresses"][0]["street"],
        json!(["Calle Mayor 1", "3A"])
    );

    assert!(matches!(
        m.delete_address("12", "99").await,
        Err(ConnectorError::NotFound(_))
    ));
    assert!(!sent(&t, Method::Delete, "/rest/V1/addresses/99"));
    ok(&t, Method::Delete, "/rest/V1/addresses/30", json!(true));
    m.delete_address("12", "30").await.unwrap();
    assert!(sent(&t, Method::Delete, "/rest/V1/addresses/30"));
}

fn checkout_routes(t: &FakeTransport, prefix: &str) {
    ok(
        t,
        Method::Post,
        &format!("{prefix}/items"),
        json!({"item_id": 1}),
    );
    ok(
        t,
        Method::Post,
        &format!("{prefix}/estimate-shipping-methods"),
        json!([
            {"carrier_code": "tablerate", "method_code": "bestway", "available": false},
            {"carrier_code": "flatrate", "method_code": "flatrate", "available": true}
        ]),
    );
    ok(
        t,
        Method::Post,
        &format!("{prefix}/shipping-information"),
        json!({"payment_methods": [{"code": "banktransfer"}, {"code": "checkmo"}]}),
    );
    t.on(
        Method::Get,
        "/rest/V1/orders/55",
        HttpResponse::new(
            200,
            json!({"entity_id": 55, "increment_id": "000000055", "status": "pending",
                "grand_total": 24.8, "order_currency_code": "EUR", "items": []})
            .to_string(),
        ),
    );
}

#[tokio::test]
async fn customer_orders_go_through_the_admin_cart() {
    let t = Arc::new(FakeTransport::new());
    ok(&t, Method::Get, "/rest/V1/customers/12", customer());
    ok(&t, Method::Post, "/rest/V1/customers/12/carts", json!(7));
    ok(
        &t,
        Method::Get,
        "/rest/V1/carts/7",
        json!({"id": 7, "items_count": 0}),
    );
    checkout_routes(&t, "/rest/V1/carts/7");
    ok(&t, Method::Put, "/rest/V1/carts/7/order", json!(55));
    ok(&t, Method::Post, "/rest/V1/orders/55/comments", json!(true));
    let order = magento(&t)
        .create_order(&NewOrder {
            customer_id: Some("12".into()),
            line_items: vec![NewLineItem {
                product_id: "MUG-1".into(),
                quantity: 2,
                price: None,
            }],
            note: Some("Gift wrap".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(order.id, "55");
    assert_eq!(
        body(&t, Method::Post, "/rest/V1/carts/7/items"),
        json!({"cartItem": {"sku": "MUG-1", "qty": 2, "quote_id": 7}})
    );
    let info = body(&t, Method::Post, "/rest/V1/carts/7/shipping-information");
    let a = &info["addressInformation"];
    assert_eq!(
        (&a["shipping_carrier_code"], &a["shipping_method_code"]),
        (&json!("flatrate"), &json!("flatrate"))
    );
    assert_eq!(
        a["shipping_address"]["customer_address_id"], 30,
        "the default address"
    );
    assert_eq!(a["shipping_address"]["region_id"], 158);
    assert_eq!(
        body(&t, Method::Put, "/rest/V1/carts/7/order"),
        json!({"paymentMethod": {"method": "checkmo"}})
    );
    let note = body(&t, Method::Post, "/rest/V1/orders/55/comments");
    assert_eq!(note["statusHistory"]["is_customer_notified"], 0);
}

#[tokio::test]
async fn guest_orders_use_guest_carts_and_pay_with_the_email() {
    let t = Arc::new(FakeTransport::new());
    spain(&t);
    ok(&t, Method::Post, "/rest/V1/guest-carts", json!("mAsKeD"));
    checkout_routes(&t, "/rest/V1/guest-carts/mAsKeD");
    ok(
        &t,
        Method::Post,
        "/rest/V1/guest-carts/mAsKeD/payment-information",
        json!("55"),
    );
    magento(&t)
        .create_order(&NewOrder {
            email: Some("guest@example.com".into()),
            line_items: vec![NewLineItem {
                product_id: "MUG-1".into(),
                quantity: 1,
                price: None,
            }],
            shipping: Some(AddressPatch {
                first_name: Some("Ana".into()),
                last_name: Some("Ruiz".into()),
                address1: Some("Gran Via 1".into()),
                city: Some("Madrid".into()),
                postcode: Some("28013".into()),
                state: Some("M".into()),
                country: Some("ES".into()),
                phone: Some("600".into()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        body(&t, Method::Post, "/rest/V1/guest-carts/mAsKeD/items")["cartItem"]["quote_id"],
        "mAsKeD"
    );
    let pay = body(
        &t,
        Method::Post,
        "/rest/V1/guest-carts/mAsKeD/payment-information",
    );
    assert_eq!(pay["email"], "guest@example.com");
    assert_eq!(pay["paymentMethod"]["method"], "checkmo");
    assert_eq!(pay["billingAddress"]["region"], "Madrid");
    assert_eq!(pay["billingAddress"]["region_code"], "M");
    assert_eq!(pay["billingAddress"]["street"], json!(["Gran Via 1"]));
}

#[tokio::test]
async fn orders_are_refused_when_magento_would_change_them() {
    let t = Arc::new(FakeTransport::new());
    let m = magento(&t);
    let priced = NewOrder {
        customer_id: Some("12".into()),
        line_items: vec![NewLineItem {
            product_id: "MUG-1".into(),
            quantity: 1,
            price: Some(dec("1")),
        }],
        ..Default::default()
    };
    assert!(m.create_order(&priced).await.is_err());
    assert!(t.requests().is_empty());

    ok(&t, Method::Get, "/rest/V1/customers/12", customer());
    ok(&t, Method::Post, "/rest/V1/customers/12/carts", json!(7));
    ok(
        &t,
        Method::Get,
        "/rest/V1/carts/7",
        json!({"id": 7, "items_count": 2}),
    );
    let open_cart = m
        .create_order(&NewOrder {
            line_items: vec![NewLineItem {
                product_id: "MUG-1".into(),
                quantity: 1,
                price: None,
            }],
            ..priced
        })
        .await
        .unwrap_err();
    assert!(open_cart.to_string().contains("open cart"), "{open_cart}");
    assert!(!sent(&t, Method::Post, "/rest/V1/carts/7/items"));

    let guestless = m
        .create_order(&NewOrder {
            line_items: vec![NewLineItem {
                product_id: "MUG-1".into(),
                quantity: 1,
                price: None,
            }],
            ..Default::default()
        })
        .await;
    assert!(guestless.is_err());
}

#[tokio::test]
async fn notes_can_notify_the_customer_and_deletes_are_hidden() {
    let t = Arc::new(FakeTransport::new());
    ok(&t, Method::Post, "/rest/V1/orders/41/comments", json!(true));
    let m = magento(&t);
    m.add_order_note(
        "41",
        &OrderNote {
            note: "Your parcel ships tomorrow".into(),
            notify_customer: true,
        },
    )
    .await
    .unwrap();
    let b = body(&t, Method::Post, "/rest/V1/orders/41/comments");
    assert_eq!(b["statusHistory"]["is_customer_notified"], 1);
    assert_eq!(b["statusHistory"]["comment"], "Your parcel ships tomorrow");
    assert!(m.hidden().contains(&"shop_manage_order:delete"));
}

fn rule() -> Value {
    json!({"rule_id": 9, "name": "Coupon SALE10", "simple_action": "by_percent", "discount_amount": 10,
        "is_active": true, "to_date": "2026-12-31", "description": "Autumn",
        "condition": {"condition_type": "Magento\\SalesRule\\Model\\Rule\\Condition\\Combine",
            "conditions": [{"attribute_name": "base_subtotal", "operator": ">=", "value": "50"}]}})
}

fn coupon_row() -> Value {
    json!({"coupon_id": 3, "rule_id": 9, "code": "SALE10", "usage_limit": 100, "times_used": 4, "is_primary": true})
}

#[tokio::test]
async fn coupons_are_a_rule_and_its_code() {
    let t = Arc::new(FakeTransport::new());
    let m = magento(&t);
    ok(
        &t,
        Method::Get,
        "/rest/V1/customerGroups/search",
        json!({"items": [{"id": 0}, {"id": 1}]}),
    );
    ok(
        &t,
        Method::Get,
        "/rest/V1/store/websites",
        json!([{"id": 0}, {"id": 1}]),
    );
    ok(&t, Method::Post, "/rest/V1/salesRules", rule());
    ok(&t, Method::Post, "/rest/V1/coupons", coupon_row());
    let c = m
        .create_coupon(&CouponPatch {
            code: Some("SALE10".into()),
            kind: Some("percent".into()),
            amount: Some(dec("10")),
            expires_on: NaiveDate::from_ymd_opt(2026, 12, 31),
            usage_limit: Some(100),
            minimum_amount: Some(dec("50")),
            description: Some("Autumn".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        (
            c.id.as_str(),
            c.kind.as_str(),
            c.amount,
            c.minimum_amount,
            c.usage_count
        ),
        ("3", "percent", dec("10"), Some(dec("50")), Some(4))
    );
    let r = body(&t, Method::Post, "/rest/V1/salesRules")["rule"].clone();
    assert_eq!(
        (
            &r["coupon_type"],
            &r["simple_action"],
            &r["use_auto_generation"]
        ),
        (
            &json!("SPECIFIC_COUPON"),
            &json!("by_percent"),
            &json!(false)
        )
    );
    assert_eq!(
        (&r["website_ids"], &r["customer_group_ids"]),
        (&json!([1]), &json!([0, 1]))
    );
    assert_eq!(r["to_date"], "2026-12-31");
    assert_eq!(r["condition"]["conditions"][0]["value"], "50");
    let coupon = body(&t, Method::Post, "/rest/V1/coupons")["coupon"].clone();
    assert_eq!(
        (
            &coupon["rule_id"],
            &coupon["code"],
            &coupon["is_primary"],
            &coupon["type"]
        ),
        (&json!(9), &json!("SALE10"), &json!(true), &json!(0))
    );
}

#[tokio::test]
async fn a_rejected_code_does_not_leave_its_rule_behind() {
    let t = Arc::new(FakeTransport::new());
    ok(
        &t,
        Method::Get,
        "/rest/V1/customerGroups/search",
        json!({"items": [{"id": 1}]}),
    );
    ok(
        &t,
        Method::Get,
        "/rest/V1/store/websites",
        json!([{"id": 1}]),
    );
    ok(&t, Method::Post, "/rest/V1/salesRules", rule());
    t.on(
        Method::Post,
        "/rest/V1/coupons",
        HttpResponse::new(400, r#"{"message":"The coupon code isn't unique."}"#),
    );
    ok(&t, Method::Delete, "/rest/V1/salesRules/9", json!(true));
    let err = magento(&t)
        .create_coupon(&CouponPatch {
            code: Some("SALE10".into()),
            kind: Some("fixed_cart".into()),
            amount: Some(dec("5")),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("isn't unique"), "{err}");
    assert!(sent(&t, Method::Delete, "/rest/V1/salesRules/9"));
}

#[tokio::test]
async fn coupons_list_update_and_delete_with_their_rule() {
    let t = Arc::new(FakeTransport::new());
    let m = magento(&t);
    ok(
        &t,
        Method::Get,
        "/rest/V1/coupons/search",
        json!({"items": [coupon_row()], "total_count": 1}),
    );
    ok(
        &t,
        Method::Get,
        "/rest/V1/salesRules/search",
        json!({"items": [rule()]}),
    );
    let page = m.list_coupons(&CouponQuery::default()).await.unwrap();
    assert_eq!(
        page.items[0].expires_on,
        NaiveDate::from_ymd_opt(2026, 12, 31)
    );
    let rules_req = t
        .requests()
        .into_iter()
        .find(|r| r.path() == "/rest/V1/salesRules/search")
        .unwrap();
    assert_eq!(
        query(
            &rules_req,
            "searchCriteria[filter_groups][0][filters][0][condition_type]"
        )
        .as_deref(),
        Some("in")
    );

    ok(&t, Method::Get, "/rest/V1/salesRules/9", rule());
    ok(&t, Method::Put, "/rest/V1/salesRules/9", rule());
    ok(&t, Method::Put, "/rest/V1/coupons/3", coupon_row());
    m.update_coupon(
        "3",
        &CouponPatch {
            amount: Some(dec("15")),
            code: Some("SALE15".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        body(&t, Method::Put, "/rest/V1/salesRules/9")["rule"]["discount_amount"],
        15.0
    );
    assert_eq!(
        body(&t, Method::Put, "/rest/V1/coupons/3")["coupon"]["code"],
        "SALE15"
    );

    ok(&t, Method::Delete, "/rest/V1/salesRules/9", json!(true));
    m.delete_coupon("3").await.unwrap();
    assert!(sent(&t, Method::Delete, "/rest/V1/salesRules/9"));
}

#[tokio::test]
async fn abandoned_carts_are_active_carts_with_items_and_an_email() {
    let t = Arc::new(FakeTransport::new());
    ok(
        &t,
        Method::Get,
        "/rest/V1/carts/search",
        json!({"items": [{
            "id": 70, "updated_at": "2026-10-16 18:00:00", "customer_is_guest": false,
            "customer": {"id": 12, "email": "jo@example.com", "firstname": "Jo", "lastname": "Doe"},
            "currency": {"quote_currency_code": "EUR"},
            "items": [{"sku": "MUG-1", "name": "Mug", "qty": 2, "price": 9.9}]
        }], "total_count": 1}),
    );
    let page = magento(&t)
        .list_abandoned_carts(&CartQuery {
            since: NaiveDate::from_ymd_opt(2026, 10, 10),
            ..Default::default()
        })
        .await
        .unwrap();
    let cart = &page.items[0];
    assert_eq!(
        (
            cart.email.as_deref(),
            cart.customer_id.as_deref(),
            cart.total,
            cart.currency.as_deref()
        ),
        (
            Some("jo@example.com"),
            Some("12"),
            Some(dec("19.8")),
            Some("EUR")
        )
    );
    assert_eq!(cart.line_items[0].product_id.as_deref(), Some("MUG-1"));
    let req = &t.requests()[0];
    let filters: Vec<(String, String)> = (0..4)
        .map(|g| {
            let f = |k: &str| {
                query(
                    req,
                    &format!("searchCriteria[filter_groups][{g}][filters][0][{k}]"),
                )
                .unwrap_or_default()
            };
            (f("field"), f("condition_type"))
        })
        .collect();
    assert_eq!(
        filters,
        [
            ("is_active".to_string(), "eq".to_string()),
            ("items_count".into(), "gt".into()),
            ("customer_email".into(), "notnull".into()),
            ("updated_at".into(), "gteq".into())
        ]
    );
}

#[tokio::test]
async fn raw_requests_go_under_rest_with_bearer_auth() {
    let t = Arc::new(FakeTransport::new());
    ok(
        &t,
        Method::Get,
        "/rest/V1/store/websites",
        json!([{"id": 1}]),
    );
    let resp = magento(&t)
        .raw(&RawRequest {
            method: "GET".into(),
            path: "V1/store/websites".into(),
            query: [("searchCriteria[pageSize]".to_string(), "5".to_string())].into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(resp.body, json!([{"id": 1}]));
    let req = &t.requests()[0];
    assert_eq!(req.header_value("authorization"), Some("Bearer tok123"));
    assert_eq!(query(req, "searchCriteria[pageSize]").as_deref(), Some("5"));
}
