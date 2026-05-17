use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use ledger_db::DbError;
use serde_json::json;

/// Application-level error returned by every route. Renders as a
/// `{"error": {"code": ..., "message": ...}}` JSON body with an
/// appropriate HTTP status.
#[derive(Debug, Clone)]
pub struct AppError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl AppError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// The JSON body we serve for this error. Useful for storing in
    /// idempotency_keys.response_body so replays look identical.
    pub fn body(&self) -> serde_json::Value {
        json!({
            "error": {
                "code": self.code,
                "message": self.message,
            }
        })
    }

    /// True if this error is a *deterministic* outcome — same input
    /// would produce the same error. We mark the idempotency row FAILED
    /// for these so replays return the same response.
    ///
    /// Non-deterministic errors (5xx other than 503 retry-exhausted)
    /// leave the row PENDING; the orphan-recovery job handles them.
    pub fn is_terminal_for_idempotency(&self) -> bool {
        matches!(
            self.status,
            StatusCode::UNPROCESSABLE_ENTITY
                | StatusCode::CONFLICT
                | StatusCode::NOT_FOUND
                | StatusCode::BAD_REQUEST
                | StatusCode::SERVICE_UNAVAILABLE
        )
    }

    // --- Constructors for common error shapes ---

    pub fn missing_idempotency_key() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "missing_idempotency_key",
            "Idempotency-Key header is required for this endpoint",
        )
    }

    pub fn invalid_idempotency_key() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_idempotency_key",
            "Idempotency-Key must be 1..=255 ASCII characters",
        )
    }

    pub fn invalid_json(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_json", msg)
    }

    pub fn in_flight() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "in_flight",
            "another request with this idempotency key is still in progress",
        )
    }

    pub fn key_conflict() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "key_conflict",
            "idempotency key reused with a different request body",
        )
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "resource not found")
    }

    pub fn not_ready(reason: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "not_ready", reason)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = self.body();
        (self.status, Json(body)).into_response()
    }
}

impl From<DbError> for AppError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => AppError::not_found(),

            DbError::Core(core_err) => AppError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_failed",
                core_err.to_string(),
            ),

            DbError::InvariantViolated(msg) => {
                if msg.contains("ledger_overdraft") {
                    AppError::new(StatusCode::UNPROCESSABLE_ENTITY, "overdraft", msg)
                } else if msg.contains("ledger_unbalanced") {
                    AppError::new(StatusCode::UNPROCESSABLE_ENTITY, "unbalanced", msg)
                } else if msg.contains("ledger_immutable") {
                    AppError::new(StatusCode::CONFLICT, "immutable", msg)
                } else if msg.contains("postings_amount_positive") {
                    AppError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "invalid_amount",
                        "amount_minor must be positive",
                    )
                } else if msg.contains("postings_currency_iso") {
                    AppError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "invalid_currency",
                        "currency must be 3 uppercase ASCII letters",
                    )
                } else if msg.contains("accounts_type_balance_consistent") {
                    AppError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "invalid_account_type",
                        "account_type and normal_balance disagree",
                    )
                } else {
                    AppError::new(StatusCode::UNPROCESSABLE_ENTITY, "invariant_violated", msg)
                }
            }

            DbError::UniqueViolation(msg) => {
                if msg.contains("external_id") {
                    AppError::new(
                        StatusCode::CONFLICT,
                        "external_id_conflict",
                        "external_id already exists on a previous transaction",
                    )
                } else {
                    AppError::new(StatusCode::CONFLICT, "duplicate", msg)
                }
            }

            DbError::ForeignKeyMissing(msg) => AppError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_account",
                if msg.contains("postings_account_id_fkey") {
                    "one of the postings references a non-existent account_id".to_string()
                } else {
                    msg
                },
            ),

            DbError::RetryExhausted(_) => AppError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "serialization_retry_exhausted",
                "server is contended; retry with a new idempotency key",
            ),

            DbError::Sqlx(sqlx_err) => {
                tracing::error!(error = %sqlx_err, "unhandled database error");
                AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal server error",
                )
            }
        }
    }
}
