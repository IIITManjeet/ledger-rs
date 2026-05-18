//! 200-concurrent stress test.
//!
//! Fires 200 POST /transactions at the running server in parallel, each
//! touching 2 of 20 shared accounts. Verifies that:
//!
//!   - No 5xx responses (except possibly some 503 retry-exhausted, which
//!     is acceptable under contention but we want it rare).
//!   - P1 (conservation): Σ debits − Σ credits per currency = 0.
//!   - P2 (model equivalence): for every account, the API balance equals
//!     a client-side simulator that mirrors only the *succeeded* requests.
//!
//! Marked `#[ignore]` so `cargo test` skips it by default — it takes
//! ~10–30s and pulls the workload of 200 concurrent SERIALIZABLE
//! transactions through Postgres. Run with:
//!
//!     ./scripts/test.sh -p ledger-api --test stress -- --ignored --nocapture

mod common;

use common::TestApp;
use futures::future::join_all;
use ledger_core::{AccountId, Currency, Direction};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
struct Acct {
    id: AccountId,
    normal: Direction,
}

#[derive(Debug, Clone)]
struct TaskOutcome {
    status: StatusCode,
    /// On success: (debit_account, credit_account, amount, currency)
    posted: Option<(AccountId, AccountId, i64, Currency)>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn stress_200_concurrent_transactions() {
    let app = Arc::new(TestApp::spawn().await);

    // Seed 20 accounts: 10 ASSET, 10 LIABILITY, all allow_negative=true
    // so transactions never trip the overdraft trigger (we want to test
    // *concurrency*, not the overdraft path here).
    let mut accounts: Vec<Acct> = Vec::with_capacity(20);
    for i in 0..10 {
        accounts.push(make(&app, &format!("asset-{i}"), "ASSET", true).await);
    }
    for i in 0..10 {
        accounts.push(make(&app, &format!("liab-{i}"), "LIABILITY", true).await);
    }

    let currencies = [
        Currency::new("USD").unwrap(),
        Currency::new("EUR").unwrap(),
        Currency::new("INR").unwrap(),
    ];

    // Pre-generate 200 deterministic but well-distributed transactions.
    // We don't use rand to keep the test fully deterministic; a small
    // pseudo-random schedule is enough to exercise contention.
    let mut tasks = Vec::with_capacity(200);
    for i in 0..200u32 {
        let app = app.clone();
        let accs = accounts.clone();
        let cur = currencies[(i as usize) % currencies.len()];
        // Hash i into two distinct account indices in [0, 20).
        let d = (i.wrapping_mul(2_654_435_761) as usize) % accs.len();
        let mut c = ((i ^ 0x9E37_79B9).wrapping_mul(2_654_435_761) as usize) % accs.len();
        if c == d {
            c = (c + 1) % accs.len();
        }
        let amount = 100 + ((i as i64) * 7) % 9_900; // 100..9999
        let key = format!("stress-{i}-{}", Uuid::new_v4());

        tasks.push(tokio::spawn(async move {
            post_one(&app, &key, &accs[d], &accs[c], amount, cur).await
        }));
    }

    let raw = join_all(tasks).await;
    let outcomes: Vec<TaskOutcome> = raw.into_iter().map(|r| r.unwrap()).collect();

    // Tally status codes.
    let mut ok = 0usize;
    let mut e_4xx = 0usize;
    let mut e_503 = 0usize;
    let mut e_other_5xx = 0usize;
    for o in &outcomes {
        match o.status.as_u16() {
            200..=299 => ok += 1,
            400..=499 => e_4xx += 1,
            503 => e_503 += 1,
            500..=599 => e_other_5xx += 1,
            _ => panic!("unexpected status {}", o.status),
        }
    }
    eprintln!(
        "stress: {ok} ok / {e_4xx} 4xx / {e_503} 503-retry-exhausted / {e_other_5xx} other-5xx"
    );
    assert_eq!(e_other_5xx, 0, "no non-503 5xx responses are acceptable");
    // 503 retries-exhausted are tolerable but should be rare (<0.5% of requests).
    assert!(
        e_503 <= 1,
        "more than 1 retry-exhausted out of 200: load is too high or backoff too short"
    );
    // The "successes" plus the 503s plus the 4xx must equal 200.
    assert_eq!(
        ok + e_4xx + e_503,
        200,
        "totals don't add up: {} + {} + {} != 200",
        ok,
        e_4xx,
        e_503
    );

    // Build the client-side model from the succeeded transactions only.
    let mut model: HashMap<(AccountId, Currency), i64> = HashMap::new();
    for o in &outcomes {
        if let Some((debit_acct, credit_acct, amount, cur)) = o.posted {
            // Debit side
            let acc = accounts.iter().find(|a| a.id == debit_acct).unwrap();
            let signed = if Direction::Debit == acc.normal {
                amount
            } else {
                -amount
            };
            *model.entry((acc.id, cur)).or_insert(0) += signed;
            // Credit side
            let acc = accounts.iter().find(|a| a.id == credit_acct).unwrap();
            let signed = if Direction::Credit == acc.normal {
                amount
            } else {
                -amount
            };
            *model.entry((acc.id, cur)).or_insert(0) += signed;
        }
    }

    // P1: global conservation per currency, queried directly from PG.
    let rows = sqlx::query!(
        r#"
        SELECT currency,
               COALESCE(SUM(CASE direction WHEN 'DEBIT'
                                 THEN amount_minor
                                 ELSE -amount_minor END), 0)::BIGINT AS "net!"
        FROM postings GROUP BY currency
        "#
    )
    .fetch_all(app.db.pool())
    .await
    .unwrap();
    for row in rows {
        assert_eq!(
            row.net,
            0,
            "P1 broken in {}: postings net = {}",
            row.currency.trim(),
            row.net
        );
    }

    // P2: per-account balance == model.
    for a in &accounts {
        let body: Value = app
            .client
            .get(app.url(&format!("/accounts/{}", a.id.0)))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let api: HashMap<Currency, i64> = body["balances"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                (
                    Currency::new(b["currency"].as_str().unwrap()).unwrap(),
                    b["amount_minor"].as_i64().unwrap(),
                )
            })
            .collect();
        for cur in &currencies {
            let expected = model.get(&(a.id, *cur)).copied().unwrap_or(0);
            let actual = api.get(cur).copied().unwrap_or(0);
            assert_eq!(
                actual, expected,
                "P2 broken for {:?}/{:?}: api={}, model={}",
                a.id, cur, actual, expected
            );
        }
    }
}

// ---- helpers ----

async fn make(app: &TestApp, name: &str, ty: &str, allow_negative: bool) -> Acct {
    let body: Value = app
        .client
        .post(app.url("/accounts"))
        .json(&json!({
            "name": name,
            "account_type": ty,
            "allow_negative": allow_negative,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    Acct {
        id: AccountId(Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()),
        normal: match ty {
            "ASSET" | "EXPENSE" => Direction::Debit,
            _ => Direction::Credit,
        },
    }
}

async fn post_one(
    app: &TestApp,
    key: &str,
    debit: &Acct,
    credit: &Acct,
    amount: i64,
    cur: Currency,
) -> TaskOutcome {
    let resp = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", key)
        .json(&json!({
            "postings": [
                { "account_id": debit.id.0,  "direction": "DEBIT",  "amount_minor": amount, "currency": cur.as_str() },
                { "account_id": credit.id.0, "direction": "CREDIT", "amount_minor": amount, "currency": cur.as_str() },
            ],
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    if status == StatusCode::CREATED {
        TaskOutcome {
            status,
            posted: Some((debit.id, credit.id, amount, cur)),
        }
    } else {
        TaskOutcome {
            status,
            posted: None,
        }
    }
}
