//! Property tests for the ledger.
//!
//! Each `#[tokio::test]` here boots a fresh Postgres + Axum server (one per
//! test, reused across proptest cases) and runs N random transaction
//! sequences against it. Between proptest cases we TRUNCATE postings,
//! transactions, and idempotency_keys so each case sees a clean slate.
//! Accounts are seeded once per test.
//!
//! ## How we run proptest from async
//!
//! `proptest!{}`-block tests are synchronous. To drive async code from
//! inside `proptest::TestRunner::run`, we use:
//!
//!     tokio::task::block_in_place(|| {
//!         tokio::runtime::Handle::current().block_on(async { ... })
//!     });
//!
//! `block_in_place` releases the current worker so the runtime can keep
//! scheduling. This requires the multi-thread runtime — hence the
//! `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` markers.

mod common;

use common::TestApp;
use ledger_core::{
    AccountId, Currency, Direction, MinorUnit, PostingInput, TransactionId, TransactionInput,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;

// ============================================================================
// Account seeding — a small pool with mixed types and allow_negative
// ============================================================================

#[derive(Debug, Clone, Copy)]
struct AcctInfo {
    id: AccountId,
    normal: Direction,
}

async fn seed_pool(app: &TestApp, n_each: usize) -> Vec<AcctInfo> {
    let mut out = Vec::new();
    // Half ASSETs (DEBIT-normal), half LIABILITYs (CREDIT-normal).
    // All allow_negative=true so random transactions don't run into
    // overdraft floors and confuse our model.
    for i in 0..n_each {
        let resp = app
            .client
            .post(app.url("/accounts"))
            .json(&json!({
                "name": format!("asset-{i}"),
                "account_type": "ASSET",
                "allow_negative": true,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: Value = resp.json().await.unwrap();
        out.push(AcctInfo {
            id: AccountId(Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()),
            normal: Direction::Debit,
        });
    }
    for i in 0..n_each {
        let resp = app
            .client
            .post(app.url("/accounts"))
            .json(&json!({
                "name": format!("liab-{i}"),
                "account_type": "LIABILITY",
                "allow_negative": true,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: Value = resp.json().await.unwrap();
        out.push(AcctInfo {
            id: AccountId(Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()),
            normal: Direction::Credit,
        });
    }
    out
}

// Wipe transient state between proptest cases. Accounts survive.
async fn truncate(app: &TestApp) {
    sqlx::query("TRUNCATE postings, transactions, idempotency_keys")
        .execute(app.db.pool())
        .await
        .expect("truncate failed");
}

// ============================================================================
// Strategies — how proptest generates random transactions
// ============================================================================

/// Build a strategy for one balanced 2-line transaction over the given pool.
/// Returns a transaction where:
///   - debit and credit accounts are distinct,
///   - currency ∈ {USD, EUR, INR},
///   - amount ∈ [1, 1_000_000].
fn txn_strategy(pool: Vec<AcctInfo>) -> impl Strategy<Value = TransactionInput> + Clone {
    let n = pool.len();
    (0..n, 0..n, 0u8..3, 1i64..1_000_000i64)
        .prop_filter("debit != credit", |(d, c, _, _)| d != c)
        .prop_map(move |(d, c, cur_idx, amount)| {
            let cur = match cur_idx {
                0 => Currency::new("USD").unwrap(),
                1 => Currency::new("EUR").unwrap(),
                _ => Currency::new("INR").unwrap(),
            };
            TransactionInput {
                external_id: None,
                description: Some("prop".into()),
                reverses_transaction_id: None,
                postings: vec![
                    PostingInput {
                        account_id: pool[d].id,
                        direction: Direction::Debit,
                        amount_minor: MinorUnit::new(amount).unwrap(),
                        currency: cur,
                    },
                    PostingInput {
                        account_id: pool[c].id,
                        direction: Direction::Credit,
                        amount_minor: MinorUnit::new(amount).unwrap(),
                        currency: cur,
                    },
                ],
            }
        })
}

fn seq_strategy(pool: Vec<AcctInfo>, len: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = Vec<TransactionInput>> {
    proptest::collection::vec(txn_strategy(pool), len)
}

// ============================================================================
// Helpers to apply one transaction via the real HTTP API and update the model
// ============================================================================

async fn post_txn(app: &TestApp, key: &str, txn: &TransactionInput) -> Result<TransactionId, StatusCode> {
    let body = json!({
        "description": "p",
        "postings": txn.postings.iter().map(|p| json!({
            "account_id": p.account_id.0,
            "direction": match p.direction { Direction::Debit => "DEBIT", Direction::Credit => "CREDIT" },
            "amount_minor": p.amount_minor.get(),
            "currency": p.currency.as_str(),
        })).collect::<Vec<_>>(),
    });
    let resp = app
        .client
        .post(app.url("/transactions"))
        .header("Idempotency-Key", key)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    if status == StatusCode::CREATED {
        let body: Value = resp.json().await.unwrap();
        let id = body["id"].as_str().unwrap();
        Ok(TransactionId(Uuid::parse_str(id).unwrap()))
    } else {
        Err(status)
    }
}

async fn get_balances(app: &TestApp, id: AccountId) -> HashMap<Currency, i64> {
    let body: Value = app
        .client
        .get(app.url(&format!("/accounts/{}", id.0)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["balances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            let cur = Currency::new(b["currency"].as_str().unwrap()).unwrap();
            let amt = b["amount_minor"].as_i64().unwrap();
            (cur, amt)
        })
        .collect()
}

/// Apply `txn` to the in-memory reference model. The signed amount for an
/// account is `+amount` if the posting's direction matches the account's
/// normal balance, else `-amount`. This is the exact mirror of the SQL
/// `CASE WHEN p.direction = a.normal_balance THEN ... ELSE -... END`.
fn apply_to_model(
    model: &mut HashMap<(AccountId, Currency), i64>,
    pool: &[AcctInfo],
    txn: &TransactionInput,
) {
    for p in &txn.postings {
        let acc = pool.iter().find(|a| a.id == p.account_id).expect("known account");
        let signed = if p.direction == acc.normal {
            p.amount_minor.get()
        } else {
            -p.amount_minor.get()
        };
        *model.entry((acc.id, p.currency)).or_insert(0) += signed;
    }
}

// ============================================================================
// P1 + P2: conservation and model equivalence over random sequences
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p1_p2_conservation_and_model_equivalence() {
    let app = TestApp::spawn().await;
    let pool = seed_pool(&app, 4).await; // 8 accounts total (4 ASSET, 4 LIAB)

    let strategy = seq_strategy(pool.clone(), 1..=15);
    let mut runner = TestRunner::new(Config::with_cases(15));

    let app_ref = &app;
    let pool_ref = &pool;

    runner
        .run(&strategy, |seq| {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    truncate(app_ref).await;

                    let mut model: HashMap<(AccountId, Currency), i64> = HashMap::new();
                    for (i, txn) in seq.iter().enumerate() {
                        let key = format!("p1p2-{i}-{}", uuid::Uuid::new_v4());
                        match post_txn(app_ref, &key, txn).await {
                            Ok(_) => apply_to_model(&mut model, pool_ref, txn),
                            Err(_) => continue, // rejected → don't update model either
                        }
                    }

                    // P1: global conservation = `Σ debits - Σ credits = 0`
                    // per currency across the postings table.
                    //
                    // We query the DB directly rather than summing the model:
                    // the model tracks per-account *balances* (signed by the
                    // account's normal side), which is the accounting
                    // equation, not the conservation invariant. Two different
                    // identities — same shape, different sign rules.
                    let rows = sqlx::query!(
                        r#"
                        SELECT
                            currency,
                            COALESCE(SUM(CASE direction WHEN 'DEBIT'
                                              THEN amount_minor
                                              ELSE -amount_minor END), 0)::BIGINT AS "net!"
                        FROM postings
                        GROUP BY currency
                        "#
                    )
                    .fetch_all(app_ref.db.pool())
                    .await
                    .unwrap();
                    for row in rows {
                        prop_assert_eq!(
                            row.net, 0,
                            "P1 broken in {}: Σ debits - Σ credits = {}",
                            row.currency.trim(),
                            row.net
                        );
                    }

                    // P2: per-account agreement between API and simulator.
                    for acc in pool_ref {
                        let api = get_balances(app_ref, acc.id).await;
                        for cur in [
                            Currency::new("USD").unwrap(),
                            Currency::new("EUR").unwrap(),
                            Currency::new("INR").unwrap(),
                        ] {
                            let expected = model.get(&(acc.id, cur)).copied().unwrap_or(0);
                            let actual = api.get(&cur).copied().unwrap_or(0);
                            prop_assert_eq!(
                                actual, expected,
                                "P2 broken for {:?}/{:?}: api={}, model={}",
                                acc.id, cur, actual, expected
                            );
                        }
                    }

                    Ok(())
                })
            })
        })
        .expect("property failed (proptest will print the shrunk counterexample above)");
}

// ============================================================================
// P3: reversal symmetry — apply a txn, post its mirror, balances return
// ============================================================================

fn reverse_postings(t: &TransactionInput, original_id: TransactionId) -> TransactionInput {
    TransactionInput {
        external_id: None,
        description: Some("reverse".into()),
        reverses_transaction_id: Some(original_id),
        postings: t
            .postings
            .iter()
            .map(|p| PostingInput {
                account_id: p.account_id,
                direction: p.direction.opposite(),
                amount_minor: p.amount_minor,
                currency: p.currency,
            })
            .collect(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p3_reversal_symmetry() {
    let app = TestApp::spawn().await;
    let pool = seed_pool(&app, 3).await; // 6 accounts

    let strategy = seq_strategy(pool.clone(), 1..=10);
    let mut runner = TestRunner::new(Config::with_cases(15));

    let app_ref = &app;
    let pool_ref = &pool;

    runner
        .run(&strategy, |seq| {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    truncate(app_ref).await;

                    // Apply each txn and its immediate reversal.
                    // Both should succeed (all accounts are allow_negative=true).
                    for (i, txn) in seq.iter().enumerate() {
                        let key_orig = format!("p3o-{i}-{}", uuid::Uuid::new_v4());
                        let key_rev = format!("p3r-{i}-{}", uuid::Uuid::new_v4());
                        let original_id = match post_txn(app_ref, &key_orig, txn).await {
                            Ok(id) => id,
                            Err(_) => continue, // very rare; treat as no-op
                        };
                        let reverse = reverse_postings(txn, original_id);
                        let rev_status = post_txn(app_ref, &key_rev, &reverse).await;
                        prop_assert!(
                            rev_status.is_ok(),
                            "P3: reversal of {:?} failed with {:?}",
                            original_id,
                            rev_status.err()
                        );
                    }

                    // After every original+reversal pair has landed, every
                    // account must have balance 0 in every currency.
                    for acc in pool_ref {
                        let api = get_balances(app_ref, acc.id).await;
                        for (cur, amt) in &api {
                            prop_assert_eq!(
                                *amt, 0,
                                "P3 broken for {:?}/{:?}: balance is {}, expected 0",
                                acc.id, cur, amt
                            );
                        }
                    }

                    Ok(())
                })
            })
        })
        .expect("property failed (proptest will print the shrunk counterexample above)");
}
