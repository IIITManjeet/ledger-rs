mod common;

use common::TestDb;
use ledger_core::{AccountType, Direction};
use ledger_db::{CreateAccountInput, DbError};

#[tokio::test]
async fn create_then_get_account() {
    let t = TestDb::fresh().await;

    let created =
        t.db.create_account(CreateAccountInput {
            name: "Cash".into(),
            account_type: AccountType::Asset,
            allow_negative: false,
            metadata: serde_json::json!({"note": "test"}),
        })
        .await
        .unwrap();

    assert_eq!(created.name, "Cash");
    assert_eq!(created.account_type, AccountType::Asset);
    assert_eq!(created.normal_balance, Direction::Debit);
    assert!(!created.allow_negative);

    let fetched = t.db.get_account(created.id).await.unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.name, "Cash");
}

#[tokio::test]
async fn each_account_type_maps_to_correct_normal_balance() {
    let t = TestDb::fresh().await;

    for (acct_type, want_normal) in [
        (AccountType::Asset, Direction::Debit),
        (AccountType::Expense, Direction::Debit),
        (AccountType::Liability, Direction::Credit),
        (AccountType::Equity, Direction::Credit),
        (AccountType::Revenue, Direction::Credit),
    ] {
        let row =
            t.db.create_account(CreateAccountInput {
                name: format!("{acct_type:?}"),
                account_type: acct_type,
                allow_negative: false,
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert_eq!(row.normal_balance, want_normal, "{acct_type:?}");
    }
}

#[tokio::test]
async fn get_account_not_found() {
    let t = TestDb::fresh().await;
    let bogus = ledger_core::AccountId::new();
    let err = t.db.get_account(bogus).await.unwrap_err();
    assert!(matches!(err, DbError::NotFound), "got {err:?}");
}

#[tokio::test]
async fn balances_start_empty() {
    let t = TestDb::fresh().await;
    let acct =
        t.db.create_account(CreateAccountInput {
            name: "Empty".into(),
            account_type: AccountType::Asset,
            allow_negative: false,
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

    let with_bal = t.db.get_account_with_balances(acct.id).await.unwrap();
    assert!(
        with_bal.balances.is_empty(),
        "new account should have no balances; got {:?}",
        with_bal.balances
    );
}
