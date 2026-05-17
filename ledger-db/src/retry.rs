use crate::error::{is_retryable_serialization, DbError};
use std::future::Future;
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 5;
const BASE_BACKOFF_MS: u64 = 10;

/// Run an async DB operation under SERIALIZABLE isolation; retry on
/// serialization failures (`40001`) and deadlocks (`40P01`) up to
/// MAX_ATTEMPTS, with exponential backoff (10, 20, 40, 80, 160 ms).
///
/// Returns `DbError::RetryExhausted` if all attempts fail with a retryable
/// error. Non-retryable errors short-circuit immediately.
///
/// The closure is called fresh per attempt — each invocation must build
/// its own `BEGIN ... COMMIT` (the previous transaction has already been
/// rolled back by Postgres on the abort).
pub async fn retry_serializable<F, Fut, T>(mut op: F) -> Result<T, DbError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, DbError>>,
{
    for attempt in 1..=MAX_ATTEMPTS {
        match op().await {
            Ok(value) => return Ok(value),

            Err(DbError::Sqlx(ref e)) if is_retryable_serialization(e) => {
                tracing::debug!(
                    attempt,
                    max = MAX_ATTEMPTS,
                    "serialization failure; retrying"
                );
                if attempt == MAX_ATTEMPTS {
                    return Err(DbError::RetryExhausted(MAX_ATTEMPTS));
                }
                let backoff = BASE_BACKOFF_MS * (1u64 << (attempt - 1));
                tokio::time::sleep(Duration::from_millis(backoff)).await;
            }

            Err(other) => return Err(other),
        }
    }

    // Unreachable in practice (the loop returns), but Rust needs it for
    // exhaustive control flow.
    Err(DbError::RetryExhausted(MAX_ATTEMPTS))
}
