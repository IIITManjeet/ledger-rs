use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("not found")]
    NotFound,

    /// One of our deferred constraint triggers or CHECK constraints raised.
    /// The message is the raw Postgres text (e.g. "ledger_unbalanced: ...",
    /// "ledger_overdraft: ...", "ledger_immutable: ..."). The API layer maps
    /// these to 422 ProblemJSON responses.
    #[error("ledger invariant violated: {0}")]
    InvariantViolated(String),

    /// A foreign key references a row that doesn't exist (typically: posting
    /// against an account_id that has no row in `accounts`).
    #[error("foreign key references missing row: {0}")]
    ForeignKeyMissing(String),

    /// A unique-key collision (e.g. duplicate `external_id` on transactions).
    #[error("unique constraint violated: {0}")]
    UniqueViolation(String),

    /// SSI retry budget exhausted. The handler should return 503 and mark
    /// the idempotency row FAILED (PLAN §4 step 6).
    #[error("serializable retry budget exhausted after {0} attempts")]
    RetryExhausted(u32),

    /// Application-level validation from `ledger_core` (e.g. unbalanced sum
    /// caught upfront, before the DB sees it).
    #[error(transparent)]
    Core(#[from] ledger_core::CoreError),

    /// Any other Postgres / driver error we didn't recognize.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// Convert a raw sqlx error into our typed DbError. Match on SQLSTATE first
/// (the SQL-standard 5-character code), since it's stable across PG versions
/// and locales; only fall back to message-prefix matching for our custom
/// `RAISE EXCEPTION` text.
pub(crate) fn classify(err: sqlx::Error) -> DbError {
    let Some(db_err) = err.as_database_error() else {
        return DbError::Sqlx(err);
    };

    let code = db_err.code().map(|c| c.into_owned());
    let msg = db_err.message().to_owned();

    match code.as_deref() {
        // check_violation — fires for both our deferred constraint triggers
        // (RAISE EXCEPTION ... USING ERRCODE = '23514') and for inline CHECK
        // constraints (amount > 0, currency regex, etc.).
        Some("23514") => DbError::InvariantViolated(msg),

        // raise_exception (default for RAISE EXCEPTION without USING) —
        // our immutability triggers use this.
        Some("P0001") => DbError::InvariantViolated(msg),

        // foreign_key_violation
        Some("23503") => DbError::ForeignKeyMissing(msg),

        // unique_violation
        Some("23505") => DbError::UniqueViolation(msg),

        _ => DbError::Sqlx(err),
    }
}

/// True if `err` is a Postgres serialization failure (SQLSTATE 40001) or
/// deadlock (SQLSTATE 40P01). Both are safe to retry; we treat them
/// identically per PLAN §3.
pub(crate) fn is_retryable_serialization(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|e| e.code())
        .map(|c| matches!(c.as_ref(), "40001" | "40P01"))
        .unwrap_or(false)
}
