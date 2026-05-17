#![forbid(unsafe_code)]

pub mod accounts;
pub mod error;
pub mod idempotency;
pub mod retry;
pub mod transactions;

pub use accounts::{AccountRow, AccountWithBalances, CreateAccountInput, CurrencyBalance};
pub use error::DbError;
pub use idempotency::IdempotencyOutcome;
pub use retry::retry_serializable;
pub use transactions::{
    InsertTransactionInput, PostingRow, TransactionRow, TransactionWithPostings,
};

use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

/// Migrations directory, relative to the workspace root.
/// `sqlx::migrate!` reads this at compile time and embeds the SQL files
/// into the binary, so the produced binary can run migrations without
/// the source tree.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// The single entry point to the database. Holds the connection pool;
/// every operation is a method on this struct.
#[derive(Clone)]
pub struct LedgerDb {
    pool: PgPool,
}

impl LedgerDb {
    /// Open a pool against `database_url`. Sets per-connection guards
    /// (statement_timeout, idle_in_transaction_session_timeout) on every
    /// new connection so a stuck query can never hold locks indefinitely.
    pub async fn connect(database_url: &str) -> Result<Self, DbError> {
        let pool = PgPoolOptions::new()
            .max_connections(25)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    // Guard against runaway queries and forgotten transactions.
                    sqlx::query("SET statement_timeout = '5000ms'")
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("SET idle_in_transaction_session_timeout = '10000ms'")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(database_url)
            .await?;

        Ok(Self { pool })
    }

    /// Apply pending migrations from `../migrations`. Idempotent — already-
    /// applied migrations are skipped (tracked in `_sqlx_migrations`).
    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        MIGRATOR.run(&self.pool).await
    }

    /// Direct pool access for callers that need it (the sweeper, the
    /// readiness probe, integration tests).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
