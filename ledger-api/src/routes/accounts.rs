use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use ledger_core::{AccountId, AccountType, Currency, Direction, PostingId, TransactionId};
use ledger_db::CreateAccountInput;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// --- Request / response shapes ---

#[derive(Deserialize)]
pub struct CreateAccountReq {
    pub name: String,
    pub account_type: AccountType,
    #[serde(default)]
    pub allow_negative: bool,
    #[serde(default = "empty_metadata")]
    pub metadata: serde_json::Value,
}

fn empty_metadata() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Serialize)]
pub struct AccountResp {
    pub id: AccountId,
    pub name: String,
    pub account_type: AccountType,
    pub normal_balance: Direction,
    pub allow_negative: bool,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct BalanceResp {
    pub currency: Currency,
    pub amount_minor: i64,
}

#[derive(Serialize)]
pub struct AccountWithBalancesResp {
    #[serde(flatten)]
    pub account: AccountResp,
    pub balances: Vec<BalanceResp>,
}

#[derive(Deserialize)]
pub struct PostingsListQuery {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub order: Option<String>,
    /// Optional ISO-4217 currency filter (e.g. ?currency=USD). When set,
    /// only postings in that currency are returned. The filter is enforced
    /// inside the SQL — no client-side filtering of pages.
    #[serde(default)]
    pub currency: Option<String>,
}

#[derive(Serialize)]
pub struct PostingItem {
    pub id: PostingId,
    pub transaction_id: TransactionId,
    pub account_id: AccountId,
    pub direction: Direction,
    pub amount_minor: i64,
    pub currency: Currency,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct PostingsResp {
    pub items: Vec<PostingItem>,
    pub next_cursor: Option<String>,
}

// --- Handlers ---

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateAccountReq>,
) -> Result<(StatusCode, Json<AccountResp>), AppError> {
    let row = state
        .db
        .create_account(CreateAccountInput {
            name: req.name,
            account_type: req.account_type,
            allow_negative: req.allow_negative,
            metadata: req.metadata,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(row_to_resp(row))))
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AccountWithBalancesResp>, AppError> {
    let result = state.db.get_account_with_balances(AccountId(id)).await?;
    Ok(Json(AccountWithBalancesResp {
        account: row_to_resp(result.account),
        balances: result
            .balances
            .into_iter()
            .map(|b| BalanceResp {
                currency: b.currency,
                amount_minor: b.amount_minor,
            })
            .collect(),
    }))
}

pub async fn list_postings(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<PostingsListQuery>,
) -> Result<Json<PostingsResp>, AppError> {
    // Verify the account exists (gives us a clean 404 instead of an empty list).
    state.db.get_account(AccountId(id)).await?;

    let limit = q.limit.unwrap_or(50).min(200) as i64;
    let ascending = matches!(q.order.as_deref(), Some("asc"));
    let cursor = q.cursor.as_deref().map(decode_cursor).transpose()?;

    // Currency filter: validate shape at the edge so a malformed value
    // gives a 400 with the right error, not a generic SQL constraint hit.
    let currency_filter: Option<String> = match q.currency.as_deref() {
        None => None,
        Some(s) => {
            Currency::new(s).map_err(|e| AppError::from(ledger_db::DbError::Core(e)))?;
            Some(s.to_string())
        }
    };
    let cur_param = currency_filter.as_deref();

    // Each sqlx::query! invocation produces its own anonymous Record struct,
    // so we convert each branch's rows to PostingItem immediately. That gives
    // every branch the same return type (Vec<PostingItem>) and the if/else
    // type-checks cleanly.
    let mut items: Vec<PostingItem> = if let Some((cursor_ts, cursor_id)) = cursor {
        if ascending {
            sqlx::query!(
                r#"
                SELECT id, transaction_id, account_id,
                       direction AS "direction!: Direction",
                       amount_minor, currency, created_at
                FROM postings
                WHERE account_id = $1
                  AND (created_at, id) > ($2, $3)
                  AND ($4::char(3) IS NULL OR currency = $4)
                ORDER BY created_at ASC, id ASC
                LIMIT $5
                "#,
                id,
                cursor_ts,
                cursor_id,
                cur_param,
                limit + 1,
            )
            .fetch_all(state.db.pool())
            .await
            .map_err(|e| AppError::from(ledger_db::DbError::Sqlx(e)))?
            .into_iter()
            .map(|r| {
                Ok::<_, AppError>(PostingItem {
                    id: PostingId(r.id),
                    transaction_id: TransactionId(r.transaction_id),
                    account_id: AccountId(r.account_id),
                    direction: r.direction,
                    amount_minor: r.amount_minor,
                    currency: Currency::new(r.currency.trim())
                        .map_err(|e| AppError::from(ledger_db::DbError::Core(e)))?,
                    created_at: r.created_at,
                })
            })
            .collect::<Result<_, _>>()?
        } else {
            sqlx::query!(
                r#"
                SELECT id, transaction_id, account_id,
                       direction AS "direction!: Direction",
                       amount_minor, currency, created_at
                FROM postings
                WHERE account_id = $1
                  AND (created_at, id) < ($2, $3)
                  AND ($4::char(3) IS NULL OR currency = $4)
                ORDER BY created_at DESC, id DESC
                LIMIT $5
                "#,
                id,
                cursor_ts,
                cursor_id,
                cur_param,
                limit + 1,
            )
            .fetch_all(state.db.pool())
            .await
            .map_err(|e| AppError::from(ledger_db::DbError::Sqlx(e)))?
            .into_iter()
            .map(|r| {
                Ok::<_, AppError>(PostingItem {
                    id: PostingId(r.id),
                    transaction_id: TransactionId(r.transaction_id),
                    account_id: AccountId(r.account_id),
                    direction: r.direction,
                    amount_minor: r.amount_minor,
                    currency: Currency::new(r.currency.trim())
                        .map_err(|e| AppError::from(ledger_db::DbError::Core(e)))?,
                    created_at: r.created_at,
                })
            })
            .collect::<Result<_, _>>()?
        }
    } else if ascending {
        sqlx::query!(
            r#"
            SELECT id, transaction_id, account_id,
                   direction AS "direction!: Direction",
                   amount_minor, currency, created_at
            FROM postings
            WHERE account_id = $1
              AND ($2::char(3) IS NULL OR currency = $2)
            ORDER BY created_at ASC, id ASC
            LIMIT $3
            "#,
            id,
            cur_param,
            limit + 1,
        )
        .fetch_all(state.db.pool())
        .await
        .map_err(|e| AppError::from(ledger_db::DbError::Sqlx(e)))?
        .into_iter()
        .map(|r| {
            Ok::<_, AppError>(PostingItem {
                id: PostingId(r.id),
                transaction_id: TransactionId(r.transaction_id),
                account_id: AccountId(r.account_id),
                direction: r.direction,
                amount_minor: r.amount_minor,
                currency: Currency::new(r.currency.trim())
                    .map_err(|e| AppError::from(ledger_db::DbError::Core(e)))?,
                created_at: r.created_at,
            })
        })
        .collect::<Result<_, _>>()?
    } else {
        sqlx::query!(
            r#"
            SELECT id, transaction_id, account_id,
                   direction AS "direction!: Direction",
                   amount_minor, currency, created_at
            FROM postings
            WHERE account_id = $1
              AND ($2::char(3) IS NULL OR currency = $2)
            ORDER BY created_at DESC, id DESC
            LIMIT $3
            "#,
            id,
            cur_param,
            limit + 1,
        )
        .fetch_all(state.db.pool())
        .await
        .map_err(|e| AppError::from(ledger_db::DbError::Sqlx(e)))?
        .into_iter()
        .map(|r| {
            Ok::<_, AppError>(PostingItem {
                id: PostingId(r.id),
                transaction_id: TransactionId(r.transaction_id),
                account_id: AccountId(r.account_id),
                direction: r.direction,
                amount_minor: r.amount_minor,
                currency: Currency::new(r.currency.trim())
                    .map_err(|e| AppError::from(ledger_db::DbError::Core(e)))?,
                created_at: r.created_at,
            })
        })
        .collect::<Result<_, _>>()?
    };

    // We fetched limit+1 rows. If we got the extra one, there's a next
    // page; drop the extra and set the cursor to the LAST of the returned
    // items so the next query (`< cursor`) starts cleanly from the row
    // after this page (which was the row we just dropped).
    let has_more = items.len() as i64 > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        items
            .last()
            .map(|last| encode_cursor(last.created_at, last.id.0))
    } else {
        None
    };

    Ok(Json(PostingsResp { items, next_cursor }))
}

// --- Helpers ---

fn row_to_resp(row: ledger_db::AccountRow) -> AccountResp {
    AccountResp {
        id: row.id,
        name: row.name,
        account_type: row.account_type,
        normal_balance: row.normal_balance,
        allow_negative: row.allow_negative,
        metadata: row.metadata,
        created_at: row.created_at,
    }
}

fn encode_cursor(created_at: DateTime<Utc>, id: Uuid) -> String {
    let raw = format!("{}|{}", created_at.timestamp_micros(), id);
    URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

fn decode_cursor(s: &str) -> Result<(DateTime<Utc>, Uuid), AppError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .map_err(|_| AppError::invalid_json("cursor is not valid base64"))?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| AppError::invalid_json("cursor is not valid utf-8"))?;
    let (ts_str, id_str) = raw
        .split_once('|')
        .ok_or_else(|| AppError::invalid_json("cursor missing separator"))?;
    let ts_micros: i64 = ts_str
        .parse()
        .map_err(|_| AppError::invalid_json("cursor timestamp is not i64"))?;
    let ts = DateTime::<Utc>::from_timestamp_micros(ts_micros)
        .ok_or_else(|| AppError::invalid_json("cursor timestamp out of range"))?;
    let id: Uuid = id_str
        .parse()
        .map_err(|_| AppError::invalid_json("cursor id is not a uuid"))?;
    Ok((ts, id))
}
