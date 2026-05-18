-- 0004_invariant_triggers.sql
-- The cross-row invariants. These cannot be enforced by CHECK constraints
-- (CHECK can only see one row) and cannot be enforced by regular triggers
-- (they fire per-row before sibling rows of the same transaction are visible).
--
-- We use CREATE CONSTRAINT TRIGGER ... DEFERRABLE INITIALLY DEFERRED, which
-- is Postgres's mechanism for "this trigger fires at COMMIT, by which point
-- every row of the user-level transaction is in place."
--
-- Postgres requires constraint triggers to be FOR EACH ROW. For an N-line
-- transaction each function runs N times with the same result — we accept
-- the redundant work on the common 2-line case; the alternative (session-
-- scoped "already validated" markers) adds complexity for marginal gain.

-- ---------------------------------------------------------------------------
-- I5 + I6: every transaction has >= 2 postings AND sum of debits == sum of
--          credits per currency.
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION ledger_check_transaction_balanced() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    n_postings           INTEGER;
    unbalanced_currency  TEXT;
    unbalanced_diff      BIGINT;
BEGIN
    SELECT COUNT(*) INTO n_postings
    FROM postings
    WHERE transaction_id = NEW.transaction_id;

    IF n_postings < 2 THEN
        RAISE EXCEPTION
            'ledger_unbalanced: transaction % has only % posting(s); minimum is 2',
            NEW.transaction_id, n_postings
            USING ERRCODE = '23514';   -- check_violation
    END IF;

    SELECT
        currency,
        SUM(CASE direction WHEN 'DEBIT' THEN amount_minor ELSE -amount_minor END)
    INTO unbalanced_currency, unbalanced_diff
    FROM postings
    WHERE transaction_id = NEW.transaction_id
    GROUP BY currency
    HAVING SUM(CASE direction WHEN 'DEBIT' THEN amount_minor ELSE -amount_minor END) <> 0
    LIMIT 1;

    IF unbalanced_currency IS NOT NULL THEN
        RAISE EXCEPTION
            'ledger_unbalanced: transaction % is unbalanced in % (debits - credits = %)',
            NEW.transaction_id, unbalanced_currency, unbalanced_diff
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;   -- constraint triggers ignore the return value
END;
$$;

CREATE CONSTRAINT TRIGGER transaction_balanced
    AFTER INSERT ON postings
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ledger_check_transaction_balanced();

-- ---------------------------------------------------------------------------
-- I8: no overdraft unless accounts.allow_negative = TRUE.
--
-- For each (account_id, currency) touched by the current transaction, compute
-- the account's FULL balance (over every posting that ever landed on it),
-- and reject if it would go negative on the account's normal side AND
-- allow_negative is false.
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION ledger_check_no_overdraft() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    bad_account_id   UUID;
    bad_currency     CHAR(3);
    bad_balance      BIGINT;
BEGIN
    WITH affected AS (
        SELECT DISTINCT account_id, currency
        FROM postings
        WHERE transaction_id = NEW.transaction_id
    ),
    full_balances AS (
        SELECT
            p.account_id,
            p.currency,
            a.allow_negative,
            SUM(CASE WHEN p.direction = a.normal_balance
                     THEN p.amount_minor
                     ELSE -p.amount_minor
                END) AS balance
        FROM postings p
        JOIN accounts a ON a.id = p.account_id
        WHERE (p.account_id, p.currency) IN (SELECT account_id, currency FROM affected)
        GROUP BY p.account_id, p.currency, a.allow_negative
    )
    SELECT account_id, currency, balance
    INTO bad_account_id, bad_currency, bad_balance
    FROM full_balances
    WHERE allow_negative = FALSE
      AND balance < 0
    LIMIT 1;

    IF bad_account_id IS NOT NULL THEN
        RAISE EXCEPTION
            'ledger_overdraft: account % would have balance % in % (allow_negative=false)',
            bad_account_id, bad_balance, bad_currency
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER transaction_no_overdraft
    AFTER INSERT ON postings
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ledger_check_no_overdraft();
