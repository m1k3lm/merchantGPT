//! End to end against the `woo` compose store with a scripted model: the
//! backend connects the shop, the agent reads orders, and an approved stock
//! change lands in WooCommerce and the audit log. Ignored by default; needs
//! the same WOO_* variables as the connectors live test.

use std::sync::{Arc, Mutex};

use chat_core::{AgentEvent, Backend, BackendEvent, Command, Credentials, Decision};
use connectors::domain::ProductQuery;
use connectors::woocommerce::WooCommerce;
use connectors::{ConnectorKind, Platform, ReqwestTransport, ShopConnector};
use llm::fake::FakeProvider;
use llm::{ChatMessage, LlmProvider, ProviderConfig, ProviderId};
use serde_json::json;
use store::Store;
use tools::CancellationToken;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set"))
}

async fn http_client(url: &str) -> reqwest::Client {
    let mut b = reqwest::Client::builder();
    if let Ok(target) = std::env::var("WOO_RESOLVE") {
        let host = reqwest::Url::parse(url)
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        let addr = tokio::net::lookup_host(&target)
            .await
            .unwrap()
            .next()
            .unwrap();
        b = b.resolve(&host, addr);
    }
    b.build().unwrap()
}

#[tokio::test]
#[ignore = "needs a live WooCommerce store"]
async fn agent_reads_and_changes_a_live_store() {
    let url = env("WOO_URL");
    let creds = Credentials::from([
        ("consumer_key".into(), env("WOO_CK")),
        ("consumer_secret".into(), env("WOO_CS")),
    ]);
    let http = http_client(&url).await;
    let woo = WooCommerce::new(&url, &creds, Arc::new(ReqwestTransport::new(http.clone())));
    let hoodie = woo
        .list_products(&ProductQuery {
            sku: Some("HOOD-BLU".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .items
        .remove(0);
    let new_stock = hoodie.stock_quantity.unwrap_or(0) + 1;

    let provider = Arc::new(FakeProvider::new());
    provider
        .push_tool_call("c1", "shop_list_orders", json!({"status": "processing"}))
        .push_tool_call(
            "c2",
            "shop_manage_product",
            json!({"action": "update", "id": hoodie.id, "stock_quantity": new_stock}),
        )
        .push_text("Done: one processing order, stock updated.");
    let events = Arc::new(Mutex::new(Vec::new()));
    let store = Store::open_in_memory().unwrap();
    let p = provider.clone();
    let ev = events.clone();
    let mut backend = Backend::new(
        Arc::new(move |e: BackendEvent| {
            if let BackendEvent::Agent(AgentEvent::ConfirmationRequested { responder, .. }) = &e {
                responder.respond(Decision::Approve);
            }
            ev.lock().unwrap().push(e);
        }),
        Arc::new(move |_| Ok(p.clone() as Arc<dyn LlmProvider>)),
        Arc::new(ReqwestTransport::new(http)),
        store.clone(),
    );
    // The scripted replies are the agent's; the off-topic check would take one.
    backend.handle(Command::SetScopeGuard(false)).await;
    backend
        .handle(Command::SetLlm(ProviderConfig {
            options: Default::default(),
            provider: ProviderId::OpenAi,
            api_key: "unused-key".into(),
            base_url: None,
            model: "scripted".into(),
        }))
        .await;
    backend
        .handle(Command::ConnectShop {
            url: url.clone(),
            kind: ConnectorKind::Shop(Platform::WooCommerce),
            credentials: creds,
        })
        .await;
    {
        let events = events.lock().unwrap();
        assert!(
            !events.iter().any(|e| matches!(e, BackendEvent::Error(_))),
            "{events:?}"
        );
    }
    backend
        .handle(Command::Send {
            text: "How many processing orders, and add one hoodie to stock".into(),
            cancel: CancellationToken::new(),
        })
        .await;

    let results: Vec<_> = backend
        .history()
        .iter()
        .filter_map(|m| match m {
            ChatMessage::ToolResults { results } => Some(results[0].clone()),
            _ => None,
        })
        .collect();
    assert!(!results[0].is_error, "{}", results[0].content);
    assert!(results[0].content.contains("\"status\":\"processing\""));
    assert!(!results[1].is_error, "{}", results[1].content);

    let after = woo.get_product(&hoodie.id).await.unwrap();
    assert_eq!(after.stock_quantity, Some(new_stock));
    let stages: Vec<_> = store
        .audit_log(backend.active_shop_id(), 10)
        .unwrap()
        .into_iter()
        .map(|a| a.stage)
        .collect();
    assert_eq!(stages, ["succeeded", "approved"]);
}
