use crate::error::AppError;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use ledger_core::{
    sha256_of_body, AccountId, Currency, Direction, PostingId, PostingInput, TransactionId,
    TransactionInput,
};
use ledger_db::{IdempotencyOutcome, InsertTransactionInput};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// --- Request / response shapes ---

#[derive(Deserialize)]
pub struct CreateTransactionReq {
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub reverses_transaction_id: Option<TransactionId>,
    pub postings: Vec<PostingInput>,
}

#[derive(Serialize)]
pub struct TransactionResp {
    pub id: TransactionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverses_transaction_id: Option<TransactionId>,
    pub created_at: DateTime<Utc>,
    pub postings: Vec<PostingResp>,
}

#[derive(Serialize)]
pub struct PostingResp {
    pub id: PostingId,
    pub transaction_id: TransactionId,
    pub account_id: AccountId,
    pub direction: Direction,
    pub amount_minor: i64,
    pub currency: Currency,
    pub created_at: DateTime<Utc>,
}

const IDEMPOTENT_REPLAYED: &str = "Idempotent-Replayed";

// --- Handlers ---

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    // 1. Extract + validate Idempotency-Key header.
    let key = headers
        .get("idempotency-key")
        .ok_or_else(AppError::missing_idempotency_key)?
        .to_str()
        .map_err(|_| AppError::invalid_idempotency_key())?;
    if key.is_empty() || key.len() > 255 {
        return Err(AppError::invalid_idempotency_key());
    }

    // 2. Hash the canonicalized body. Failure here means malformed JSON.
    let hash = sha256_of_body(&body)
        .ok_or_else(|| AppError::invalid_json("request body is not valid JSON"))?;

    // 3. Check idempotency state.
    let outcome = state.db.idempotency_begin(key, &hash).await?;
    match outcome {
        IdempotencyOutcome::Fresh => execute_fresh(state, key, body).await,
        IdempotencyOutcome::InFlight => Err(AppError::in_flight()),
        IdempotencyOutcome::Conflict => Err(AppError::key_conflict()),
        IdempotencyOutcome::Replay {
            response_status,
            response_body,
        } => Ok(replay_response(response_status, response_body)),
    }
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<TransactionResp>, AppError> {
    let row = state
        .db
        .get_transaction_with_postings(TransactionId(id))
        .await?;
    Ok(Json(to_resp(row)))
}

// --- Internals ---

async fn execute_fresh(state: AppState, key: &str, body: Bytes) -> Result<Response, AppError> {
    // Structural parse can fail (e.g. account_id isn't a valid UUID) even
    // though the body is valid JSON (which is what we hashed earlier).
    // Treat that as a deterministic 400 — mark the idempotency row FAILED
    // so replays with the same body return the same response.
    let req: CreateTransactionReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => {
            let app_err = AppError::invalid_json(format!("malformed request body: {e}"));
            let _ = state
                .db
                .idempotency_fail(key, app_err.status.as_u16(), &app_err.body())
                .await;
            return Err(app_err);
        }
    };

    let input = InsertTransactionInput {
        input: TransactionInput {
            external_id: req.external_id,
            description: req.description,
            reverses_transaction_id: req.reverses_transaction_id,
            postings: req.postings,
        },
    };

    match state.db.insert_transaction(input).await {
        Ok(written) => {
            let txn_id = written.transaction.id;
            let resp = to_resp(written);
            let body = serde_json::to_value(&resp).expect("response is always serializable");
            // Best-effort: store the response for replays. If this UPDATE
            // fails (e.g., DB hiccup), the work is still committed; the
            // idempotency row stays PENDING and the orphan-recovery path
            // takes over after 1h.
            if let Err(e) = state
                .db
                .idempotency_complete(key, 201, &body, Some(txn_id))
                .await
            {
                tracing::warn!(error = ?e, key, "idempotency_complete failed; row left PENDING");
            }
            Ok((StatusCode::CREATED, Json(body)).into_response())
        }
        Err(db_err) => {
            let app_err: AppError = db_err.into();
            if app_err.is_terminal_for_idempotency() {
                let _ = state
                    .db
                    .idempotency_fail(key, app_err.status.as_u16(), &app_err.body())
                    .await;
            }
            Err(app_err)
        }
    }
}

fn replay_response(stored_status: i16, body: serde_json::Value) -> Response {
    let status = StatusCode::from_u16(stored_status as u16).unwrap_or(StatusCode::OK);
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert(IDEMPOTENT_REPLAYED, HeaderValue::from_static("true"));
    response
}

fn to_resp(written: ledger_db::TransactionWithPostings) -> TransactionResp {
    TransactionResp {
        id: written.transaction.id,
        external_id: written.transaction.external_id,
        description: written.transaction.description,
        reverses_transaction_id: written.transaction.reverses_transaction_id,
        created_at: written.transaction.created_at,
        postings: written
            .postings
            .into_iter()
            .map(|p| PostingResp {
                id: p.id,
                transaction_id: p.transaction_id,
                account_id: p.account_id,
                direction: p.direction,
                amount_minor: p.amount_minor,
                currency: p.currency,
                created_at: p.created_at,
            })
            .collect(),
    }
}
