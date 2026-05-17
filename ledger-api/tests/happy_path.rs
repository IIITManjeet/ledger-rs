mod common;

use common::TestApp;
use reqwest::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

async fn create_account(app: &TestApp, name: &str, account_type: &str) -> Value {
    let resp = app
        .client
        .post(app.url("/accounts"))
        .json(&json!({
            "name": name,
            "account_type": account_type,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    resp.json().await.unwrap()
}

#[tokio::test]
async fn healthz_responds_ok() {
    let app = TestApp::spawn().await;
    let resp = app.client.get(app.url("/healthz")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn readyz_responds_ready_when_db_up() {
    let app = TestApp::spawn().await;
    let resp = app.client.get(app.url("/readyz")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ready");
}

#[tokio::test]
async fn create_account_201_and_get_round_trip() {
    let app = TestApp::spawn().await;
    let created = create_account(&app, "Cash", "ASSET").await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["account_type"], "ASSET");
    assert_eq!(created["normal_balance"], "DEBIT");
    assert_eq!(created["allow_negative"], false);

    let resp = app
        .client
        .get(app.url(&format!("/accounts/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"], id);
    assert_eq!(body["balances"], json!([]));
}

#[tokio::test]
async fn get_account_404_when_missing() {
    let app = TestApp::spawn().await;
    let bogus = Uuid::now_v7();
    let resp = app
        .client
        .get(app.url(&format!("/accounts/{bogus}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn post_transaction_201_and_balances_update() {
    let app = TestApp::spawn().await;
    let cash = create_account(&app, "Cash", "ASSET").await;
    let cust = create_account(&app, "Customer", "LIABILITY").await;

    let resp = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "k1")
        .json(&json!({
            "description": "top-up",
            "postings": [
                { "account_id": cash["id"], "direction": "DEBIT",  "amount_minor": 10000, "currency": "USD" },
                { "account_id": cust["id"], "direction": "CREDIT", "amount_minor": 10000, "currency": "USD" },
            ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(
        resp.headers().get("Idempotent-Replayed").is_none(),
        "fresh request must NOT have Idempotent-Replayed header"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["postings"].as_array().unwrap().len(), 2);

    let cash_id = cash["id"].as_str().unwrap();
    let cash_resp = app
        .client
        .get(app.url(&format!("/accounts/{cash_id}")))
        .send()
        .await
        .unwrap();
    let cash_body: Value = cash_resp.json().await.unwrap();
    assert_eq!(cash_body["balances"][0]["currency"], "USD");
    assert_eq!(cash_body["balances"][0]["amount_minor"], 10000);
}

#[tokio::test]
async fn post_transaction_missing_idempotency_key_400() {
    let app = TestApp::spawn().await;
    let resp = app
        .client
        .post(app.url("/transactions"))
        .json(&json!({"postings": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "missing_idempotency_key");
}

#[tokio::test]
async fn post_transaction_unbalanced_422() {
    let app = TestApp::spawn().await;
    let cash = create_account(&app, "Cash", "ASSET").await;
    let cust = create_account(&app, "Customer", "LIABILITY").await;

    let resp = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "k-unbalanced")
        .json(&json!({
            "postings": [
                { "account_id": cash["id"], "direction": "DEBIT",  "amount_minor": 100, "currency": "USD" },
                { "account_id": cust["id"], "direction": "CREDIT", "amount_minor": 50,  "currency": "USD" },
            ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    // Could match either "validation_failed" (app-level) or "unbalanced"
    // (DB-level). Both are acceptable; the app-level is what we hit because
    // ledger_core::validate fires first.
    let code = body["error"]["code"].as_str().unwrap();
    assert!(
        matches!(code, "validation_failed" | "unbalanced"),
        "got code={code}"
    );
}

#[tokio::test]
async fn post_transaction_overdraft_422() {
    let app = TestApp::spawn().await;
    let cash = create_account(&app, "Cash", "ASSET").await;
    let cust = create_account(&app, "Customer", "LIABILITY").await;

    // Try to credit Cash (asset, allow_negative=false) without prior debit.
    let resp = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "k-overdraft")
        .json(&json!({
            "postings": [
                { "account_id": cash["id"], "direction": "CREDIT", "amount_minor": 100, "currency": "USD" },
                { "account_id": cust["id"], "direction": "DEBIT",  "amount_minor": 100, "currency": "USD" },
            ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "overdraft");
}

#[tokio::test]
async fn get_transaction_round_trip() {
    let app = TestApp::spawn().await;
    let cash = create_account(&app, "Cash", "ASSET").await;
    let cust = create_account(&app, "Customer", "LIABILITY").await;

    let created: Value = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "k-rt")
        .json(&json!({
            "postings": [
                { "account_id": cash["id"], "direction": "DEBIT",  "amount_minor": 42, "currency": "USD" },
                { "account_id": cust["id"], "direction": "CREDIT", "amount_minor": 42, "currency": "USD" },
            ],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let id = created["id"].as_str().unwrap();
    let resp = app
        .client
        .get(app.url(&format!("/transactions/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"], id);
    assert_eq!(body["postings"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn pagination_returns_all_postings_via_cursor() {
    let app = TestApp::spawn().await;
    let cash = create_account(&app, "Cash", "ASSET").await;
    let cust = create_account(&app, "Customer", "LIABILITY").await;

    // 30 transactions × 2 postings → 60 postings on cash.
    for i in 0..30 {
        app.client
            .post(app.url("/transactions"))
            .header("Idempotency-Key", format!("k-{i}"))
            .json(&json!({
                "postings": [
                    { "account_id": cash["id"], "direction": "DEBIT",  "amount_minor": 1, "currency": "USD" },
                    { "account_id": cust["id"], "direction": "CREDIT", "amount_minor": 1, "currency": "USD" },
                ],
            }))
            .send()
            .await
            .unwrap();
    }

    let cash_id = cash["id"].as_str().unwrap();
    let mut seen = 0;
    let mut cursor: Option<String> = None;
    loop {
        let url = match &cursor {
            Some(c) => app.url(&format!("/accounts/{cash_id}/postings?limit=10&cursor={c}")),
            None => app.url(&format!("/accounts/{cash_id}/postings?limit=10")),
        };
        let body: Value = app
            .client
            .get(url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let items = body["items"].as_array().unwrap();
        seen += items.len();
        match body["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_owned()),
            None => break,
        }
        assert!(seen <= 60, "ran past expected total");
    }
    assert_eq!(
        seen, 30,
        "expected one posting per txn for the cash account"
    );
}
