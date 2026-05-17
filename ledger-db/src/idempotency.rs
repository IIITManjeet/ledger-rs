use crate::error::{classify, DbError};
use crate::LedgerDb;
use chrono::Utc;
use ledger_core::{CoreError, TransactionId};

/// PG idempotency_status enum mirrored in Rust.
/// Private to ledger-db — the API layer sees IdempotencyOutcome, not this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "idempotency_status", rename_all = "SCREAMING_SNAKE_CASE")]
enum IdempotencyStatus {
    Pending,
    Completed,
    Failed,
}

/// What `idempotency_begin` tells the API layer to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyOutcome {
    /// New row inserted with status PENDING. Caller must run the work,
    /// then call `idempotency_complete` (success) or `idempotency_fail`
    /// (failure) to transition the row out of PENDING.
    Fresh,

    /// Existing PENDING row with matching hash — another request with the
    /// same key is already in flight. API returns 409 `in_flight`.
    InFlight,

    /// Existing row with **different** hash. Same key reused with
    /// different body — almost always a client bug. API returns 409
    /// `key_conflict`.
    Conflict,

    /// Existing COMPLETED or FAILED row with matching hash. Return the
    /// stored response verbatim + the `Idempotent-Replayed: true` header.
    Replay {
        response_status: i16,
        response_body: serde_json::Value,
    },
}

impl LedgerDb {
    /// Start an idempotent operation. Inserts a PENDING row keyed on
    /// `key` if one doesn't already exist; otherwise classifies the
    /// existing row.
    ///
    /// `request_hash` must be the SHA-256 of the canonical-JSON form of
    /// the request body (see `ledger_core::sha256_of_body`).
    pub async fn idempotency_begin(
        &self,
        key: &str,
        request_hash: &[u8; 32],
    ) -> Result<IdempotencyOutcome, DbError> {
        if key.is_empty() || key.len() > 255 {
            return Err(DbError::Core(CoreError::InvalidKeyLength(key.len())));
        }

        // Up to two attempts: the second handles the expired-row race
        // (we observe an existing expired row, delete it, then re-insert).
        for _attempt in 0..2 {
            // Step 1: try INSERT with ON CONFLICT DO NOTHING.
            // If RETURNING gives us a row, we own the PENDING slot.
            let inserted = sqlx::query!(
                r#"
                INSERT INTO idempotency_keys (key, request_hash, status)
                VALUES ($1, $2, 'PENDING')
                ON CONFLICT (key) DO NOTHING
                RETURNING key
                "#,
                key,
                &request_hash[..],
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?;

            if inserted.is_some() {
                return Ok(IdempotencyOutcome::Fresh);
            }

            // Step 2: conflict — read the existing row to classify it.
            let row = sqlx::query!(
                r#"
                SELECT
                    request_hash,
                    status AS "status!: IdempotencyStatus",
                    response_status,
                    response_body,
                    expires_at
                FROM idempotency_keys
                WHERE key = $1
                "#,
                key,
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(classify)?;

            let Some(row) = row else {
                // Lost a race — someone deleted the row between our INSERT
                // and our SELECT. Loop and try INSERT again.
                continue;
            };

            // Expired? Delete (conditionally, to avoid clobbering a
            // freshly-replaced row) and retry the insert.
            if row.expires_at < Utc::now() {
                sqlx::query!(
                    r#"
                    DELETE FROM idempotency_keys
                    WHERE key = $1 AND expires_at < NOW()
                    "#,
                    key,
                )
                .execute(&self.pool)
                .await
                .map_err(classify)?;
                continue;
            }

            // Hash mismatch — same key, different body. 409 key_conflict.
            if row.request_hash.as_slice() != request_hash.as_slice() {
                return Ok(IdempotencyOutcome::Conflict);
            }

            // Same hash — branch on status.
            return match row.status {
                IdempotencyStatus::Pending => Ok(IdempotencyOutcome::InFlight),
                IdempotencyStatus::Completed | IdempotencyStatus::Failed => {
                    // CHECK constraint guarantees these are non-null
                    // when status != PENDING.
                    let response_status = row
                        .response_status
                        .expect("CHECK guarantees response_status non-null for non-PENDING");
                    let response_body = row
                        .response_body
                        .expect("CHECK guarantees response_body non-null for non-PENDING");
                    Ok(IdempotencyOutcome::Replay {
                        response_status,
                        response_body,
                    })
                }
            };
        }

        // Two laps and still no resolution — extremely unlikely. Surface
        // as a transient error so the client retries.
        Err(DbError::Sqlx(sqlx::Error::PoolTimedOut))
    }

    /// Mark the PENDING row as COMPLETED, storing the response we want to
    /// return on future replays.
    ///
    /// The `WHERE status = 'PENDING'` clause is defensive — if some bug
    /// causes a second call, we don't overwrite a COMPLETED/FAILED row.
    pub async fn idempotency_complete(
        &self,
        key: &str,
        response_status: u16,
        response_body: &serde_json::Value,
        transaction_id: Option<TransactionId>,
    ) -> Result<(), DbError> {
        sqlx::query!(
            r#"
            UPDATE idempotency_keys
            SET status = 'COMPLETED',
                response_status = $1,
                response_body = $2,
                transaction_id = $3,
                completed_at = NOW()
            WHERE key = $4 AND status = 'PENDING'
            "#,
            response_status as i16,
            response_body,
            transaction_id.map(|t| t.0),
            key,
        )
        .execute(&self.pool)
        .await
        .map_err(classify)?;
        Ok(())
    }

    /// Mark the PENDING row as FAILED. Used for deterministic 422s
    /// (validation, overdraft) and for retry-exhaustion 503s.
    pub async fn idempotency_fail(
        &self,
        key: &str,
        response_status: u16,
        response_body: &serde_json::Value,
    ) -> Result<(), DbError> {
        sqlx::query!(
            r#"
            UPDATE idempotency_keys
            SET status = 'FAILED',
                response_status = $1,
                response_body = $2,
                completed_at = NOW()
            WHERE key = $3 AND status = 'PENDING'
            "#,
            response_status as i16,
            response_body,
            key,
        )
        .execute(&self.pool)
        .await
        .map_err(classify)?;
        Ok(())
    }

    /// Force-set a key's expires_at into the past. Used by tests to
    /// simulate the 24h TTL having elapsed without waiting.
    #[doc(hidden)]
    pub async fn idempotency_force_expire(&self, key: &str) -> Result<(), DbError> {
        sqlx::query!(
            "UPDATE idempotency_keys SET expires_at = NOW() - INTERVAL '1 minute' WHERE key = $1",
            key,
        )
        .execute(&self.pool)
        .await
        .map_err(classify)?;
        Ok(())
    }

    /// Delete expired non-PENDING rows. Used by the sweeper (Day 9).
    pub async fn idempotency_sweep_expired(&self) -> Result<u64, DbError> {
        let result = sqlx::query!(
            r#"
            DELETE FROM idempotency_keys
            WHERE expires_at < NOW() AND status <> 'PENDING'
            "#
        )
        .execute(&self.pool)
        .await
        .map_err(classify)?;
        Ok(result.rows_affected())
    }
}
