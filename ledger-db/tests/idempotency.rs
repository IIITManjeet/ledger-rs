mod common;

use common::TestDb;
use ledger_db::IdempotencyOutcome;

fn hash(s: &str) -> [u8; 32] {
    ledger_core::sha256_of_body(s.as_bytes()).expect("valid json for tests")
}

#[tokio::test]
async fn first_request_is_fresh() {
    let t = TestDb::fresh().await;
    let outcome =
        t.db.idempotency_begin("key-1", &hash(r#"{"a":1}"#))
            .await
            .unwrap();
    assert_eq!(outcome, IdempotencyOutcome::Fresh);
}

#[tokio::test]
async fn replay_same_hash_while_pending_returns_in_flight() {
    let t = TestDb::fresh().await;
    let h = hash(r#"{"a":1}"#);

    let first = t.db.idempotency_begin("key-1", &h).await.unwrap();
    assert_eq!(first, IdempotencyOutcome::Fresh);

    // Caller never completed — row is still PENDING.
    let second = t.db.idempotency_begin("key-1", &h).await.unwrap();
    assert_eq!(second, IdempotencyOutcome::InFlight);
}

#[tokio::test]
async fn replay_different_hash_returns_conflict() {
    let t = TestDb::fresh().await;
    let h1 = hash(r#"{"a":1}"#);
    let h2 = hash(r#"{"a":2}"#);

    let first = t.db.idempotency_begin("key-1", &h1).await.unwrap();
    assert_eq!(first, IdempotencyOutcome::Fresh);

    let second = t.db.idempotency_begin("key-1", &h2).await.unwrap();
    assert_eq!(second, IdempotencyOutcome::Conflict);
}

#[tokio::test]
async fn replay_after_completion_returns_stored_response() {
    let t = TestDb::fresh().await;
    let h = hash(r#"{"a":1}"#);

    t.db.idempotency_begin("key-1", &h).await.unwrap();

    let stored = serde_json::json!({"id": "abc", "ok": true});
    t.db.idempotency_complete("key-1", 201, &stored, None)
        .await
        .unwrap();

    let outcome = t.db.idempotency_begin("key-1", &h).await.unwrap();
    match outcome {
        IdempotencyOutcome::Replay {
            response_status,
            response_body,
        } => {
            assert_eq!(response_status, 201);
            assert_eq!(response_body, stored);
        }
        other => panic!("expected Replay, got {other:?}"),
    }
}

#[tokio::test]
async fn replay_after_failure_returns_stored_failure() {
    let t = TestDb::fresh().await;
    let h = hash(r#"{"bad":true}"#);

    t.db.idempotency_begin("key-1", &h).await.unwrap();

    let stored = serde_json::json!({"error": {"code": "unbalanced", "message": "..."}});
    t.db.idempotency_fail("key-1", 422, &stored).await.unwrap();

    let outcome = t.db.idempotency_begin("key-1", &h).await.unwrap();
    match outcome {
        IdempotencyOutcome::Replay {
            response_status,
            response_body,
        } => {
            assert_eq!(response_status, 422);
            assert_eq!(response_body, stored);
        }
        other => panic!("expected Replay(FAILED), got {other:?}"),
    }
}

#[tokio::test]
async fn after_expiry_treated_as_fresh() {
    let t = TestDb::fresh().await;
    let h = hash(r#"{"a":1}"#);

    t.db.idempotency_begin("key-1", &h).await.unwrap();
    t.db.idempotency_complete("key-1", 201, &serde_json::json!({}), None)
        .await
        .unwrap();

    // Force-expire the row, then re-begin — should be Fresh (new row inserted).
    t.db.idempotency_force_expire("key-1").await.unwrap();
    let outcome = t.db.idempotency_begin("key-1", &h).await.unwrap();
    assert_eq!(outcome, IdempotencyOutcome::Fresh);
}

#[tokio::test]
async fn expired_then_different_hash_is_fresh_not_conflict() {
    let t = TestDb::fresh().await;

    t.db.idempotency_begin("key-1", &hash(r#"{"a":1}"#))
        .await
        .unwrap();
    t.db.idempotency_complete("key-1", 201, &serde_json::json!({}), None)
        .await
        .unwrap();
    t.db.idempotency_force_expire("key-1").await.unwrap();

    // Different body — the old row is gone, so this is a clean Fresh.
    let outcome =
        t.db.idempotency_begin("key-1", &hash(r#"{"a":2}"#))
            .await
            .unwrap();
    assert_eq!(outcome, IdempotencyOutcome::Fresh);
}

#[tokio::test]
async fn sweeper_deletes_only_non_pending_expired() {
    let t = TestDb::fresh().await;

    // PENDING row, expired — sweeper should NOT touch it (recovery job handles those).
    t.db.idempotency_begin("pending", &hash(r#"{}"#))
        .await
        .unwrap();
    t.db.idempotency_force_expire("pending").await.unwrap();

    // COMPLETED row, expired — sweeper should delete.
    t.db.idempotency_begin("done", &hash(r#"{}"#))
        .await
        .unwrap();
    t.db.idempotency_complete("done", 201, &serde_json::json!({}), None)
        .await
        .unwrap();
    t.db.idempotency_force_expire("done").await.unwrap();

    // FAILED row, NOT expired — sweeper should NOT touch.
    t.db.idempotency_begin("recent", &hash(r#"{}"#))
        .await
        .unwrap();
    t.db.idempotency_fail("recent", 422, &serde_json::json!({}))
        .await
        .unwrap();

    let deleted = t.db.idempotency_sweep_expired().await.unwrap();
    assert_eq!(deleted, 1, "should have deleted exactly 'done'");

    // Verify the other two survive — by re-begin: 'pending' still in_flight,
    // 'recent' returns the stored failure (Replay).
    let pending_status =
        t.db.idempotency_begin("pending", &hash(r#"{}"#))
            .await
            .unwrap();
    // 'pending' is expired-and-PENDING. The expired branch fires first,
    // and our begin loop deletes-and-retries, so it ends up Fresh.
    // This is intended: caller restarted the work; the abandoned PENDING
    // is just gone.
    assert_eq!(pending_status, IdempotencyOutcome::Fresh);

    let recent_status =
        t.db.idempotency_begin("recent", &hash(r#"{}"#))
            .await
            .unwrap();
    assert!(matches!(recent_status, IdempotencyOutcome::Replay { .. }));
}

#[tokio::test]
async fn concurrent_begin_with_same_key_serializes() {
    use std::sync::Arc;

    let t = TestDb::fresh().await;
    let db = Arc::new(t.db.clone());
    let h = hash(r#"{"a":1}"#);

    // Spawn two concurrent begins with the same key. PG's PRIMARY KEY
    // constraint on `idempotency_keys.key` serializes them: one wins
    // the INSERT (Fresh), the other gets the conflict path. Since the
    // first hasn't called complete() yet, the second sees PENDING → InFlight.
    let h_arr = h;
    let db1 = db.clone();
    let db2 = db.clone();
    let t1 = tokio::spawn(async move { db1.idempotency_begin("race", &h_arr).await });
    let t2 = tokio::spawn(async move { db2.idempotency_begin("race", &h_arr).await });

    let r1 = t1.await.unwrap().unwrap();
    let r2 = t2.await.unwrap().unwrap();

    let mut outcomes = [r1, r2];
    outcomes.sort_by_key(|o| match o {
        IdempotencyOutcome::Fresh => 0,
        IdempotencyOutcome::InFlight => 1,
        _ => 2,
    });
    assert_eq!(outcomes[0], IdempotencyOutcome::Fresh);
    assert_eq!(outcomes[1], IdempotencyOutcome::InFlight);
}

#[tokio::test]
async fn invalid_key_length_rejected() {
    let t = TestDb::fresh().await;
    let too_long = "x".repeat(256);
    let err =
        t.db.idempotency_begin(&too_long, &hash(r#"{}"#))
            .await
            .unwrap_err();
    // CoreError::InvalidKeyLength
    assert!(
        format!("{err:?}").contains("InvalidKeyLength"),
        "got {err:?}"
    );
}
