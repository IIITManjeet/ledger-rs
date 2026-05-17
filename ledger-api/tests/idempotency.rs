mod common;

use common::TestApp;
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;

async fn seed_accounts(app: &TestApp) -> (Value, Value) {
    let cash: Value = app
        .client
        .post(app.url("/accounts"))
        .json(&json!({"name": "Cash", "account_type": "ASSET"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cust: Value = app
        .client
        .post(app.url("/accounts"))
        .json(&json!({"name": "Customer", "account_type": "LIABILITY"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (cash, cust)
}

fn txn_body(cash_id: &str, cust_id: &str, amount: i64) -> Value {
    json!({
        "description": "test",
        "postings": [
            { "account_id": cash_id, "direction": "DEBIT",  "amount_minor": amount, "currency": "USD" },
            { "account_id": cust_id, "direction": "CREDIT", "amount_minor": amount, "currency": "USD" },
        ],
    })
}

// ============================================================================
// Case 1: first request with a fresh key → 201, no replay header
// ============================================================================
#[tokio::test]
async fn case1_first_request_returns_201_no_replay_header() {
    let app = TestApp::spawn().await;
    let (cash, cust) = seed_accounts(&app).await;

    let resp = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "case1-key")
        .json(&txn_body(
            cash["id"].as_str().unwrap(),
            cust["id"].as_str().unwrap(),
            100,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(
        resp.headers().get("Idempotent-Replayed").is_none(),
        "first-time response must NOT carry Idempotent-Replayed"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(body["id"].is_string());
}

// ============================================================================
// Case 2: replay with same key + same body → same response + replay header
// ============================================================================
#[tokio::test]
async fn case2_replay_same_body_returns_stored_response_with_replay_header() {
    let app = TestApp::spawn().await;
    let (cash, cust) = seed_accounts(&app).await;
    let body = txn_body(
        cash["id"].as_str().unwrap(),
        cust["id"].as_str().unwrap(),
        100,
    );

    let first = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "case2-key")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    assert!(first.headers().get("Idempotent-Replayed").is_none());
    let first_body: Value = first.json().await.unwrap();

    let second = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "case2-key")
        .json(&body)
        .send()
        .await
        .unwrap();

    // Same status code as the first response.
    assert_eq!(second.status(), StatusCode::CREATED);
    // The replay header is present.
    assert_eq!(
        second
            .headers()
            .get("Idempotent-Replayed")
            .map(|v| v.to_str().unwrap()),
        Some("true"),
        "replay must carry Idempotent-Replayed: true"
    );
    let second_body: Value = second.json().await.unwrap();

    // The bodies are byte-identical (same transaction id, same postings).
    assert_eq!(first_body, second_body);
}

// ============================================================================
// Case 3: same key + different body → 409 key_conflict
// ============================================================================
#[tokio::test]
async fn case3_different_body_returns_409_key_conflict() {
    let app = TestApp::spawn().await;
    let (cash, cust) = seed_accounts(&app).await;
    let body_a = txn_body(
        cash["id"].as_str().unwrap(),
        cust["id"].as_str().unwrap(),
        100,
    );
    let body_b = txn_body(
        cash["id"].as_str().unwrap(),
        cust["id"].as_str().unwrap(),
        200,
    );

    let first = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "case3-key")
        .json(&body_a)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "case3-key")
        .json(&body_b)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["error"]["code"], "key_conflict");
}

// ============================================================================
// Case 4: concurrent replay — one 201, one 409 in_flight
// ============================================================================
#[tokio::test]
async fn case4_concurrent_replay_one_wins_other_409_in_flight() {
    let app = Arc::new(TestApp::spawn().await);
    let (cash, cust) = seed_accounts(&app).await;
    let body = txn_body(
        cash["id"].as_str().unwrap(),
        cust["id"].as_str().unwrap(),
        100,
    );

    // Fire N requests with the same key as close to simultaneously as we
    // can. The PRIMARY KEY constraint on idempotency_keys.key serializes
    // them in the DB; exactly one wins the PENDING slot. The rest see
    // either PENDING (→ 409 in_flight) or — if the winner has already
    // completed by the time they probe — the stored COMPLETED response
    // (→ 201 with Idempotent-Replayed: true).
    const N: usize = 8;
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let app = app.clone();
        let body = body.clone();
        handles.push(tokio::spawn(async move {
            app.client
                .post(app.url("/transactions"))
                .header("Idempotency-Key", "case4-key")
                .json(&body)
                .send()
                .await
                .unwrap()
        }));
    }

    let mut codes: Vec<StatusCode> = Vec::with_capacity(N);
    let mut in_flight_count = 0;
    let mut replayed_count = 0;
    let mut fresh_count = 0;
    for h in handles {
        let resp = h.await.unwrap();
        let status = resp.status();
        codes.push(status);
        if status == StatusCode::CREATED {
            if resp.headers().contains_key("Idempotent-Replayed") {
                replayed_count += 1;
            } else {
                fresh_count += 1;
            }
        } else if status == StatusCode::CONFLICT {
            let body: Value = resp.json().await.unwrap();
            if body["error"]["code"] == "in_flight" {
                in_flight_count += 1;
            }
        }
    }

    // Exactly one request wins the Fresh slot.
    assert_eq!(
        fresh_count, 1,
        "exactly one request must be Fresh; got codes={codes:?}"
    );
    // The rest are some mix of in_flight (saw PENDING) and replayed (saw COMPLETED).
    assert_eq!(
        fresh_count + in_flight_count + replayed_count,
        N,
        "every request must end in one of the three states; got codes={codes:?}"
    );
}

// ============================================================================
// Case 5: after the TTL expires, a request with the same key gets a fresh slot
// ============================================================================
#[tokio::test]
async fn case5_expired_key_treated_as_fresh() {
    let app = TestApp::spawn().await;
    let (cash, cust) = seed_accounts(&app).await;
    let body = txn_body(
        cash["id"].as_str().unwrap(),
        cust["id"].as_str().unwrap(),
        100,
    );

    let first = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "case5-key")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_id = first.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Force-age the row past its expires_at without waiting 24h.
    app.db.idempotency_force_expire("case5-key").await.unwrap();

    let second = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "case5-key")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);
    // The fresh request created a *new* transaction; the response has
    // a different id and does NOT carry the replay header.
    assert!(second.headers().get("Idempotent-Replayed").is_none());
    let second_id = second.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(
        first_id, second_id,
        "expired-key replay must produce a new transaction"
    );
}

// ============================================================================
// Bonus: replay of a FAILED response also carries the replay header
// ============================================================================
#[tokio::test]
async fn bonus_failure_response_is_also_replayable() {
    let app = TestApp::spawn().await;
    let (cash, cust) = seed_accounts(&app).await;
    // Unbalanced body — will 422.
    let bad = json!({
        "description": "bad",
        "postings": [
            { "account_id": cash["id"], "direction": "DEBIT",  "amount_minor": 100, "currency": "USD" },
            { "account_id": cust["id"], "direction": "CREDIT", "amount_minor": 50,  "currency": "USD" },
        ],
    });

    let first = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "bonus-key")
        .json(&bad)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(first.headers().get("Idempotent-Replayed").is_none());
    let first_body: Value = first.json().await.unwrap();

    let second = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", "bonus-key")
        .json(&bad)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        second
            .headers()
            .get("Idempotent-Replayed")
            .map(|v| v.to_str().unwrap()),
        Some("true"),
        "FAILED responses are replayable too"
    );
    let second_body: Value = second.json().await.unwrap();
    assert_eq!(first_body, second_body);
}
