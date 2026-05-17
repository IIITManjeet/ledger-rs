use crate::account::Direction;
use crate::error::CoreError;
use crate::ids::{AccountId, TransactionId};
use crate::money::{Currency, MinorUnit};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostingInput {
    pub account_id: AccountId,
    pub direction: Direction,
    pub amount_minor: MinorUnit,
    pub currency: Currency,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverses_transaction_id: Option<TransactionId>,
    pub postings: Vec<PostingInput>,
}

impl TransactionInput {
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.postings.len() < 2 {
            return Err(CoreError::TooFewPostings(self.postings.len()));
        }

        let mut sums: HashMap<Currency, i128> = HashMap::new();
        for p in &self.postings {
            let amount = p.amount_minor.get() as i128;
            let signed = match p.direction {
                Direction::Debit => amount,
                Direction::Credit => -amount,
            };
            *sums.entry(p.currency).or_insert(0) += signed;
        }

        if let Some((currency, diff)) = sums.into_iter().find(|(_, sum)| *sum != 0) {
            return Err(CoreError::Unbalanced { currency, diff });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd() -> Currency {
        Currency::new("USD").unwrap()
    }
    fn amt(v: i64) -> MinorUnit {
        MinorUnit::new(v).unwrap()
    }
    fn posting(direction: Direction, amount_minor: i64, currency: Currency) -> PostingInput {
        PostingInput {
            account_id: AccountId::new(),
            direction,
            amount_minor: amt(amount_minor),
            currency,
        }
    }

    fn balanced_2_line() -> TransactionInput {
        TransactionInput {
            external_id: None,
            description: None,
            reverses_transaction_id: None,
            postings: vec![
                posting(Direction::Debit, 100, usd()),
                posting(Direction::Credit, 100, usd()),
            ],
        }
    }

    #[test]
    fn balanced_passes() {
        balanced_2_line().validate().unwrap();
    }

    #[test]
    fn too_few_postings_fails() {
        let t = TransactionInput {
            external_id: None,
            description: None,
            reverses_transaction_id: None,
            postings: vec![posting(Direction::Debit, 100, usd())],
        };
        assert!(matches!(t.validate(), Err(CoreError::TooFewPostings(1))));
    }

    #[test]
    fn unbalanced_fails() {
        let t = TransactionInput {
            external_id: None,
            description: None,
            reverses_transaction_id: None,
            postings: vec![
                posting(Direction::Debit, 100, usd()),
                posting(Direction::Credit, 50, usd()),
            ],
        };
        let err = t.validate().unwrap_err();
        match err {
            CoreError::Unbalanced { currency, diff } => {
                assert_eq!(currency, usd());
                assert_eq!(diff, 50);
            }
            other => panic!("expected Unbalanced, got {other:?}"),
        }
    }

    #[test]
    fn multi_currency_balanced_passes() {
        let eur = Currency::new("EUR").unwrap();
        let t = TransactionInput {
            external_id: None,
            description: None,
            reverses_transaction_id: None,
            postings: vec![
                posting(Direction::Debit, 100, usd()),
                posting(Direction::Credit, 100, usd()),
                posting(Direction::Debit, 50, eur),
                posting(Direction::Credit, 50, eur),
            ],
        };
        t.validate().unwrap();
    }

    #[test]
    fn multi_currency_unbalanced_in_one_currency_fails() {
        let eur = Currency::new("EUR").unwrap();
        let t = TransactionInput {
            external_id: None,
            description: None,
            reverses_transaction_id: None,
            postings: vec![
                posting(Direction::Debit, 100, usd()),
                posting(Direction::Credit, 100, usd()),
                posting(Direction::Debit, 50, eur),
                posting(Direction::Credit, 40, eur), // off by 10 in EUR
            ],
        };
        let err = t.validate().unwrap_err();
        match err {
            CoreError::Unbalanced { currency, diff } => {
                assert_eq!(currency, eur);
                assert_eq!(diff, 10);
            }
            other => panic!("expected Unbalanced in EUR, got {other:?}"),
        }
    }
}
