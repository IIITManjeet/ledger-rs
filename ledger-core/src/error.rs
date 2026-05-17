use crate::money::Currency;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("currency must be 3 uppercase ASCII letters, got {0:?}")]
    InvalidCurrency(String),

    #[error("amount_minor must be positive, got {0}")]
    NonPositiveAmount(i64),

    #[error("idempotency key length must be 1..=255, got {0}")]
    InvalidKeyLength(usize),

    #[error("transaction must have at least 2 postings, got {0}")]
    TooFewPostings(usize),

    #[error("transaction unbalanced in {currency}: debits - credits = {diff}")]
    Unbalanced { currency: Currency, diff: i128 },
}
