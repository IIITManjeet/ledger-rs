mod common;

use common::TestDb;
use ledger_core::{AccountType, Currency, Direction, MinorUnit, PostingInput, TransactionInput};
use ledger_db::{CreateAccountInput, DbError, InsertTransactionInput};

fn usd() -> Currency {
    Currency::new("USD").unwrap()
}
fn amt(v: i64) -> MinorUnit {
    MinorUnit::new(v).unwrap()
}

async fn make_account(
    t: &TestDb,
    name: &str,
    account_type: AccountType,
    allow_negative: bool,
) -> ledger_db::AccountRow {
    t.db.create_account(CreateAccountInput {
        name: name.into(),
        account_type,
        allow_negative,
        metadata: serde_json::json!({}),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn balanced_transaction_commits_and_updates_balances() {
    let t = TestDb::fresh().await;
    let cash = make_account(&t, "Cash", AccountType::Asset, false).await;
    let cust = make_account(&t, "Customer", AccountType::Liability, false).await;

    let result =
        t.db.insert_transaction(InsertTransactionInput {
            input: TransactionInput {
                external_id: Some("txn-001".into()),
                description: Some("top-up".into()),
                reverses_transaction_id: None,
                postings: vec![
                    PostingInput {
                        account_id: cash.id,
                        direction: Direction::Debit,
                        amount_minor: amt(10_000),
                        currency: usd(),
                    },
                    PostingInput {
                        account_id: cust.id,
                        direction: Direction::Credit,
                        amount_minor: amt(10_000),
                        currency: usd(),
                    },
                ],
            },
        })
        .await
        .unwrap();

    assert_eq!(result.postings.len(), 2);
    assert_eq!(result.transaction.external_id.as_deref(), Some("txn-001"));

    let cash_bal = t.db.get_account_with_balances(cash.id).await.unwrap();
    assert_eq!(cash_bal.balances.len(), 1);
    assert_eq!(cash_bal.balances[0].currency, usd());
    assert_eq!(cash_bal.balances[0].amount_minor, 10_000);

    let cust_bal = t.db.get_account_with_balances(cust.id).await.unwrap();
    assert_eq!(cust_bal.balances[0].amount_minor, 10_000);
}

#[tokio::test]
async fn unbalanced_transaction_rejected_upfront() {
    let t = TestDb::fresh().await;
    let cash = make_account(&t, "Cash", AccountType::Asset, false).await;
    let cust = make_account(&t, "Customer", AccountType::Liability, false).await;

    let err =
        t.db.insert_transaction(InsertTransactionInput {
            input: TransactionInput {
                external_id: None,
                description: None,
                reverses_transaction_id: None,
                postings: vec![
                    PostingInput {
                        account_id: cash.id,
                        direction: Direction::Debit,
                        amount_minor: amt(100),
                        currency: usd(),
                    },
                    PostingInput {
                        account_id: cust.id,
                        direction: Direction::Credit,
                        amount_minor: amt(50),
                        currency: usd(),
                    },
                ],
            },
        })
        .await
        .unwrap_err();

    // ledger_core::validate caught it before the DB transaction even opened.
    assert!(matches!(err, DbError::Core(_)), "got {err:?}");
}

#[tokio::test]
async fn single_posting_rejected_upfront() {
    let t = TestDb::fresh().await;
    let cash = make_account(&t, "Cash", AccountType::Asset, false).await;

    let err =
        t.db.insert_transaction(InsertTransactionInput {
            input: TransactionInput {
                external_id: None,
                description: None,
                reverses_transaction_id: None,
                postings: vec![PostingInput {
                    account_id: cash.id,
                    direction: Direction::Debit,
                    amount_minor: amt(100),
                    currency: usd(),
                }],
            },
        })
        .await
        .unwrap_err();

    assert!(matches!(err, DbError::Core(_)), "got {err:?}");
}

#[tokio::test]
async fn overdraft_on_strict_account_rejected_by_db() {
    let t = TestDb::fresh().await;
    let cash = make_account(&t, "Cash", AccountType::Asset, false).await;
    let cust = make_account(&t, "Customer", AccountType::Liability, false).await;

    // Try to credit Cash 100 without prior debit → balance would be -100.
    let err =
        t.db.insert_transaction(InsertTransactionInput {
            input: TransactionInput {
                external_id: None,
                description: None,
                reverses_transaction_id: None,
                postings: vec![
                    PostingInput {
                        account_id: cash.id,
                        direction: Direction::Credit,
                        amount_minor: amt(100),
                        currency: usd(),
                    },
                    PostingInput {
                        account_id: cust.id,
                        direction: Direction::Debit,
                        amount_minor: amt(100),
                        currency: usd(),
                    },
                ],
            },
        })
        .await
        .unwrap_err();

    match err {
        DbError::InvariantViolated(msg) => {
            assert!(
                msg.contains("ledger_overdraft"),
                "expected overdraft message, got: {msg}"
            );
        }
        other => panic!("expected InvariantViolated(overdraft), got {other:?}"),
    }
}

#[tokio::test]
async fn overdraft_allowed_on_allow_negative_account() {
    let t = TestDb::fresh().await;
    let credit_line = make_account(&t, "Credit Line", AccountType::Asset, true).await;
    let cust = make_account(&t, "Customer", AccountType::Liability, false).await;

    // First fund the customer so the DEBIT side doesn't overdraw.
    let funder = make_account(&t, "Funding", AccountType::Asset, true).await;
    t.db.insert_transaction(InsertTransactionInput {
        input: TransactionInput {
            external_id: None,
            description: None,
            reverses_transaction_id: None,
            postings: vec![
                PostingInput {
                    account_id: funder.id,
                    direction: Direction::Credit, // funder goes negative; allowed
                    amount_minor: amt(1_000),
                    currency: usd(),
                },
                PostingInput {
                    account_id: cust.id,
                    direction: Direction::Credit, // customer accrues positive balance
                    amount_minor: amt(1_000),
                    currency: usd(),
                },
            ],
        },
    })
    .await
    .err(); // ignore: this will fail because debits != credits

    // Actually fund cust via a balanced txn:
    t.db.insert_transaction(InsertTransactionInput {
        input: TransactionInput {
            external_id: None,
            description: None,
            reverses_transaction_id: None,
            postings: vec![
                PostingInput {
                    account_id: funder.id,
                    direction: Direction::Debit, // funder accrues (allowed_negative=true so any direction is fine)
                    amount_minor: amt(1_000),
                    currency: usd(),
                },
                PostingInput {
                    account_id: cust.id,
                    direction: Direction::Credit,
                    amount_minor: amt(1_000),
                    currency: usd(),
                },
            ],
        },
    })
    .await
    .unwrap();

    // Now: credit_line goes negative (allowed), customer goes down by 500 from +1000.
    let result =
        t.db.insert_transaction(InsertTransactionInput {
            input: TransactionInput {
                external_id: None,
                description: None,
                reverses_transaction_id: None,
                postings: vec![
                    PostingInput {
                        account_id: credit_line.id,
                        direction: Direction::Credit,
                        amount_minor: amt(500),
                        currency: usd(),
                    },
                    PostingInput {
                        account_id: cust.id,
                        direction: Direction::Debit,
                        amount_minor: amt(500),
                        currency: usd(),
                    },
                ],
            },
        })
        .await;

    result.unwrap();

    let cl_bal =
        t.db.get_account_with_balances(credit_line.id)
            .await
            .unwrap();
    assert_eq!(cl_bal.balances[0].amount_minor, -500);
}

#[tokio::test]
async fn duplicate_external_id_rejected() {
    let t = TestDb::fresh().await;
    let cash = make_account(&t, "Cash", AccountType::Asset, false).await;
    let cust = make_account(&t, "Customer", AccountType::Liability, false).await;

    let make_txn = || InsertTransactionInput {
        input: TransactionInput {
            external_id: Some("dupe".into()),
            description: None,
            reverses_transaction_id: None,
            postings: vec![
                PostingInput {
                    account_id: cash.id,
                    direction: Direction::Debit,
                    amount_minor: amt(100),
                    currency: usd(),
                },
                PostingInput {
                    account_id: cust.id,
                    direction: Direction::Credit,
                    amount_minor: amt(100),
                    currency: usd(),
                },
            ],
        },
    };

    t.db.insert_transaction(make_txn()).await.unwrap();
    let err = t.db.insert_transaction(make_txn()).await.unwrap_err();
    assert!(matches!(err, DbError::UniqueViolation(_)), "got {err:?}");
}

#[tokio::test]
async fn get_transaction_with_postings_round_trip() {
    let t = TestDb::fresh().await;
    let cash = make_account(&t, "Cash", AccountType::Asset, false).await;
    let cust = make_account(&t, "Customer", AccountType::Liability, false).await;

    let written =
        t.db.insert_transaction(InsertTransactionInput {
            input: TransactionInput {
                external_id: None,
                description: Some("round-trip".into()),
                reverses_transaction_id: None,
                postings: vec![
                    PostingInput {
                        account_id: cash.id,
                        direction: Direction::Debit,
                        amount_minor: amt(42),
                        currency: usd(),
                    },
                    PostingInput {
                        account_id: cust.id,
                        direction: Direction::Credit,
                        amount_minor: amt(42),
                        currency: usd(),
                    },
                ],
            },
        })
        .await
        .unwrap();

    let fetched =
        t.db.get_transaction_with_postings(written.transaction.id)
            .await
            .unwrap();

    assert_eq!(fetched.transaction.id, written.transaction.id);
    assert_eq!(
        fetched.transaction.description.as_deref(),
        Some("round-trip")
    );
    assert_eq!(fetched.postings.len(), 2);
    let total_debits: i64 = fetched
        .postings
        .iter()
        .filter(|p| p.direction == Direction::Debit)
        .map(|p| p.amount_minor)
        .sum();
    assert_eq!(total_debits, 42);
}
