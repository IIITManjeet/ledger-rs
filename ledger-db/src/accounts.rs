use crate::error::{classify, DbError};
use crate::LedgerDb;
use chrono::{DateTime, Utc};
use ledger_core::{AccountId, AccountType, Currency, Direction};

/// Input for creating a new account.
#[derive(Debug, Clone)]
pub struct CreateAccountInput {
    pub name: String,
    pub account_type: AccountType,
    pub allow_negative: bool,
    pub metadata: serde_json::Value,
}

/// A row from the `accounts` table.
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub id: AccountId,
    pub name: String,
    pub account_type: AccountType,
    pub normal_balance: Direction,
    pub allow_negative: bool,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Per-currency balance for an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrencyBalance {
    pub currency: Currency,
    pub amount_minor: i64,
}

/// An account plus its computed balances. This is what `GET /accounts/:id`
/// returns.
#[derive(Debug, Clone)]
pub struct AccountWithBalances {
    pub account: AccountRow,
    pub balances: Vec<CurrencyBalance>,
}

impl LedgerDb {
    // (Accounts and balances queries below.)
    /// Insert a new account. The `normal_balance` column is derived from
    /// `account_type` (Asset/Expense → DEBIT, others → CREDIT) and the
    /// DB CHECK constraint will reject any mismatch.
    pub async fn create_account(&self, input: CreateAccountInput) -> Result<AccountRow, DbError> {
        let id = AccountId::new();
        let normal_balance = input.account_type.normal_balance();

        let rec = sqlx::query!(
            r#"
            INSERT INTO accounts (id, name, account_type, normal_balance, allow_negative, metadata)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING
                id,
                name,
                account_type   AS "account_type!: AccountType",
                normal_balance AS "normal_balance!: Direction",
                allow_negative,
                metadata,
                created_at
            "#,
            id.0,
            input.name,
            input.account_type as AccountType,
            normal_balance as Direction,
            input.allow_negative,
            input.metadata,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(classify)?;

        Ok(AccountRow {
            id: AccountId(rec.id),
            name: rec.name,
            account_type: rec.account_type,
            normal_balance: rec.normal_balance,
            allow_negative: rec.allow_negative,
            metadata: rec.metadata,
            created_at: rec.created_at,
        })
    }

    /// Look up one account by id. Returns `DbError::NotFound` if absent.
    pub async fn get_account(&self, id: AccountId) -> Result<AccountRow, DbError> {
        let rec = sqlx::query!(
            r#"
            SELECT
                id,
                name,
                account_type   AS "account_type!: AccountType",
                normal_balance AS "normal_balance!: Direction",
                allow_negative,
                metadata,
                created_at
            FROM accounts
            WHERE id = $1
            "#,
            id.0,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(classify)?
        .ok_or(DbError::NotFound)?;

        Ok(AccountRow {
            id: AccountId(rec.id),
            name: rec.name,
            account_type: rec.account_type,
            normal_balance: rec.normal_balance,
            allow_negative: rec.allow_negative,
            metadata: rec.metadata,
            created_at: rec.created_at,
        })
    }

    /// Fetch an account plus its per-currency balances. The balance is
    /// computed live from `postings` — no cache.
    /// `SUM(BIGINT)` in Postgres returns NUMERIC, so we cast back to
    /// BIGINT explicitly.
    pub async fn get_account_with_balances(
        &self,
        id: AccountId,
    ) -> Result<AccountWithBalances, DbError> {
        let account = self.get_account(id).await?;

        let rows = sqlx::query!(
            r#"
            SELECT
                p.currency,
                COALESCE(SUM(CASE WHEN p.direction = a.normal_balance
                                  THEN p.amount_minor
                                  ELSE -p.amount_minor END), 0)::BIGINT AS "balance_minor!"
            FROM postings p
            JOIN accounts a ON a.id = p.account_id
            WHERE p.account_id = $1
            GROUP BY p.currency
            ORDER BY p.currency
            "#,
            id.0,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(classify)?;

        let balances = rows
            .into_iter()
            .map(|r| {
                Ok::<_, DbError>(CurrencyBalance {
                    currency: Currency::new(r.currency.trim())?,
                    amount_minor: r.balance_minor,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(AccountWithBalances { account, balances })
    }
}
