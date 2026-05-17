use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AccountType {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Direction {
    Debit,
    Credit,
}

impl AccountType {
    pub fn normal_balance(self) -> Direction {
        match self {
            AccountType::Asset | AccountType::Expense => Direction::Debit,
            AccountType::Liability | AccountType::Equity | AccountType::Revenue => {
                Direction::Credit
            }
        }
    }
}

impl Direction {
    pub fn opposite(self) -> Direction {
        match self {
            Direction::Debit => Direction::Credit,
            Direction::Credit => Direction::Debit,
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Debit => "DEBIT",
            Direction::Credit => "CREDIT",
        })
    }
}

impl fmt::Display for AccountType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AccountType::Asset => "ASSET",
            AccountType::Liability => "LIABILITY",
            AccountType::Equity => "EQUITY",
            AccountType::Revenue => "REVENUE",
            AccountType::Expense => "EXPENSE",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_balance_mapping() {
        assert_eq!(AccountType::Asset.normal_balance(), Direction::Debit);
        assert_eq!(AccountType::Expense.normal_balance(), Direction::Debit);
        assert_eq!(AccountType::Liability.normal_balance(), Direction::Credit);
        assert_eq!(AccountType::Equity.normal_balance(), Direction::Credit);
        assert_eq!(AccountType::Revenue.normal_balance(), Direction::Credit);
    }

    #[test]
    fn direction_opposite_is_involution() {
        for d in [Direction::Debit, Direction::Credit] {
            assert_eq!(d.opposite().opposite(), d);
        }
    }

    #[test]
    fn account_type_serializes_screaming_snake() {
        let json = serde_json::to_string(&AccountType::Liability).unwrap();
        assert_eq!(json, r#""LIABILITY""#);
        let parsed: AccountType = serde_json::from_str(r#""LIABILITY""#).unwrap();
        assert_eq!(parsed, AccountType::Liability);
    }
}
