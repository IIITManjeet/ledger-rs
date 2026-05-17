use crate::error::{classify, DbError};
use crate::retry::retry_serializable;
use crate::LedgerDb;
use chrono::{DateTime, Utc};
use ledger_core::{AccountId, Currency, Direction, PostingId, TransactionId, TransactionInput};
use sqlx::Acquire;

/// Input to `insert_transaction`. Equivalent to `ledger_core::TransactionInput`
/// — we accept the core type and apply its `validate()` first.
#[derive(Debug, Clone)]
pub struct InsertTransactionInput {
    pub input: TransactionInput,
}

/// A row from the `transactions` table.
#[derive(Debug, Clone)]
pub struct TransactionRow {
    pub id: TransactionId,
    pub external_id: Option<String>,
    pub description: Option<String>,
    pub reverses_transaction_id: Option<TransactionId>,
    pub created_at: DateTime<Utc>,
}

/// A row from the `postings` table.
#[derive(Debug, Clone)]
pub struct PostingRow {
    pub id: PostingId,
    pub transaction_id: TransactionId,
    pub account_id: AccountId,
    pub direction: Direction,
    pub amount_minor: i64,
    pub currency: Currency,
    pub created_at: DateTime<Utc>,
}

/// A transaction with its postings — the unit `GET /transactions/:id` returns.
#[derive(Debug, Clone)]
pub struct TransactionWithPostings {
    pub transaction: TransactionRow,
    pub postings: Vec<PostingRow>,
}

impl LedgerDb {
    /// Insert a transaction and its postings atomically under SERIALIZABLE
    /// isolation. Retries on serialization failures. Returns the full
    /// committed result.
    ///
    /// Order of operations inside the transaction:
    ///   1. SET TRANSACTION ISOLATION LEVEL SERIALIZABLE
    ///   2. INSERT into transactions (one row)
    ///   3. INSERT into postings (N rows, loop)
    ///   4. COMMIT — deferred triggers fire here:
    ///         - transaction_balanced (>=2 postings, sum=0 per currency)
    ///         - transaction_no_overdraft (allow_negative or balance>=0)
    pub async fn insert_transaction(
        &self,
        InsertTransactionInput { input }: InsertTransactionInput,
    ) -> Result<TransactionWithPostings, DbError> {
        // Fail fast on obviously-broken input before opening a SERIALIZABLE
        // transaction. Cheaper for the DB, friendlier errors for the caller.
        input.validate()?;

        let pool = self.pool.clone();
        retry_serializable(|| async {
            let mut conn = pool.acquire().await?;
            let mut tx = conn.begin().await?;

            sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
                .execute(&mut *tx)
                .await
                .map_err(classify)?;

            // 1. Insert the transaction row.
            let transaction_id = TransactionId::new();
            let txn_row = sqlx::query!(
                r#"
                INSERT INTO transactions (id, external_id, description, reverses_transaction_id)
                VALUES ($1, $2, $3, $4)
                RETURNING id, external_id, description, reverses_transaction_id, created_at
                "#,
                transaction_id.0,
                input.external_id.as_deref(),
                input.description.as_deref(),
                input.reverses_transaction_id.map(|t| t.0),
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(classify)?;

            // 2. Insert each posting. For typical 2-line transactions this
            //    is 2 round-trips; we can switch to UNNEST-based bulk insert
            //    on Day 10 if benchmarks flag it.
            let mut posting_rows = Vec::with_capacity(input.postings.len());
            for posting in &input.postings {
                let posting_id = PostingId::new();
                let amount: i64 = posting.amount_minor.into();
                let row = sqlx::query!(
                    r#"
                    INSERT INTO postings (id, transaction_id, account_id, direction, amount_minor, currency)
                    VALUES ($1, $2, $3, $4, $5, $6)
                    RETURNING
                        id,
                        transaction_id,
                        account_id,
                        direction AS "direction!: Direction",
                        amount_minor,
                        currency,
                        created_at
                    "#,
                    posting_id.0,
                    transaction_id.0,
                    posting.account_id.0,
                    posting.direction as Direction,
                    amount,
                    posting.currency.as_str(),
                )
                .fetch_one(&mut *tx)
                .await
                .map_err(classify)?;

                posting_rows.push(PostingRow {
                    id: PostingId(row.id),
                    transaction_id: TransactionId(row.transaction_id),
                    account_id: AccountId(row.account_id),
                    direction: row.direction,
                    amount_minor: row.amount_minor,
                    currency: Currency::new(row.currency.trim())?,
                    created_at: row.created_at,
                });
            }

            // 3. Commit — this is where the deferred triggers fire. If any
            //    invariant fails, `classify` maps it to InvariantViolated.
            tx.commit().await.map_err(classify)?;

            Ok(TransactionWithPostings {
                transaction: TransactionRow {
                    id: TransactionId(txn_row.id),
                    external_id: txn_row.external_id,
                    description: txn_row.description,
                    reverses_transaction_id: txn_row.reverses_transaction_id.map(TransactionId),
                    created_at: txn_row.created_at,
                },
                postings: posting_rows,
            })
        })
        .await
    }

    /// Fetch a transaction with all its postings. Returns NotFound if absent.
    pub async fn get_transaction_with_postings(
        &self,
        id: TransactionId,
    ) -> Result<TransactionWithPostings, DbError> {
        let txn_row = sqlx::query!(
            r#"
            SELECT id, external_id, description, reverses_transaction_id, created_at
            FROM transactions
            WHERE id = $1
            "#,
            id.0,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(classify)?
        .ok_or(DbError::NotFound)?;

        let posting_rows = sqlx::query!(
            r#"
            SELECT
                id,
                transaction_id,
                account_id,
                direction AS "direction!: Direction",
                amount_minor,
                currency,
                created_at
            FROM postings
            WHERE transaction_id = $1
            ORDER BY created_at, id
            "#,
            id.0,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(classify)?;

        let postings = posting_rows
            .into_iter()
            .map(|r| {
                Ok::<_, DbError>(PostingRow {
                    id: PostingId(r.id),
                    transaction_id: TransactionId(r.transaction_id),
                    account_id: AccountId(r.account_id),
                    direction: r.direction,
                    amount_minor: r.amount_minor,
                    currency: Currency::new(r.currency.trim())?,
                    created_at: r.created_at,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(TransactionWithPostings {
            transaction: TransactionRow {
                id: TransactionId(txn_row.id),
                external_id: txn_row.external_id,
                description: txn_row.description,
                reverses_transaction_id: txn_row.reverses_transaction_id.map(TransactionId),
                created_at: txn_row.created_at,
            },
            postings,
        })
    }
}
