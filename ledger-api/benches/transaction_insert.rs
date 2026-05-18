//! Criterion benchmark — hot path: insert a 2-line transaction over HTTP.
//!
//! Setup (run once, outside the timed loop):
//!   - Connect to DATABASE_URL (your local docker-compose Postgres).
//!   - Apply migrations (idempotent).
//!   - TRUNCATE to ensure the postings table is empty (so we measure
//!     insert latency, not growth-degraded latency).
//!   - Boot Axum on 127.0.0.1:<random>.
//!   - Create two allow_negative accounts.
//!
//! Per iteration (the only thing timed):
//!   - POST /transactions with a fresh idempotency key, 2 postings, 1 USD pair.
//!
//! Report: mean ± CI in terminal, p50/p95/p99 in target/criterion/<name>/.
//!
//! Run with:
//!     DATABASE_URL=postgres://ledger:ledger@localhost:5432/ledger \
//!     cargo bench -p ledger-api --bench transaction_insert

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ledger_api::{router, AppState};
use ledger_db::LedgerDb;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

struct Harness {
    addr: SocketAddr,
    client: reqwest::Client,
    cash_id: String,
    cust_id: String,
}

async fn setup(database_url: &str) -> Harness {
    let db = LedgerDb::connect(database_url).await.expect("connect");
    db.migrate().await.expect("migrate");

    // Clean state so we benchmark insertion into an empty(ish) postings table.
    // (The B-tree index degrades very slowly, but this keeps runs comparable.)
    sqlx::query("TRUNCATE postings, transactions, idempotency_keys, accounts CASCADE")
        .execute(db.pool())
        .await
        .expect("truncate");

    let app = router(AppState::new(db));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let cash: serde_json::Value = client
        .post(format!("http://{addr}/accounts"))
        .json(&json!({"name": "Cash", "account_type": "ASSET", "allow_negative": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cust: serde_json::Value = client
        .post(format!("http://{addr}/accounts"))
        .json(&json!({"name": "Customer", "account_type": "LIABILITY", "allow_negative": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    Harness {
        addr,
        client,
        cash_id: cash["id"].as_str().unwrap().to_owned(),
        cust_id: cust["id"].as_str().unwrap().to_owned(),
    }
}

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
}

fn bench_transaction_insert(c: &mut Criterion) {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set to run this bench (e.g. docker compose up postgres)");
    let harness = rt().block_on(setup(&database_url));

    let mut group = c.benchmark_group("transaction_insert_2line");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("post_2line_usd", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let key = format!("bench-{}-{}", i, uuid::Uuid::new_v4());
                let body = json!({
                    "postings": [
                        {"account_id": &harness.cash_id, "direction": "DEBIT",
                         "amount_minor": 100, "currency": "USD"},
                        {"account_id": &harness.cust_id, "direction": "CREDIT",
                         "amount_minor": 100, "currency": "USD"},
                    ],
                });
                let req = harness
                    .client
                    .post(format!("http://{}/transactions", harness.addr))
                    .header("Idempotency-Key", key)
                    .json(&body);

                let start = Instant::now();
                let resp = rt().block_on(async { req.send().await.unwrap() });
                total += start.elapsed();
                black_box(resp.status());
            }
            total
        });
    });

    group.finish();
}

criterion_group!(benches, bench_transaction_insert);
criterion_main!(benches);
