-- 0001_init.sql
-- Core enums, accounts, transactions, and the immutability triggers.
--
-- Postings live in 0002. The cross-row invariant triggers (balanced-sum,
-- overdraft) live in 0004 — they reference postings, so they have to come
-- after that table exists.

-- ---------------------------------------------------------------------------
-- Enums
-- ---------------------------------------------------------------------------

CREATE TYPE account_type AS ENUM (
    'ASSET',
    'LIABILITY',
    'EQUITY',
    'REVENUE',
    'EXPENSE'
);

CREATE TYPE direction AS ENUM ('DEBIT', 'CREDIT');

-- ---------------------------------------------------------------------------
-- accounts
-- ---------------------------------------------------------------------------

CREATE TABLE accounts (
    id              UUID         PRIMARY KEY,
    name            TEXT         NOT NULL,
    account_type    account_type NOT NULL,
    normal_balance  direction    NOT NULL,
    allow_negative  BOOLEAN      NOT NULL DEFAULT FALSE,
    metadata        JSONB        NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    CONSTRAINT accounts_type_balance_consistent CHECK (
        (account_type IN ('ASSET', 'EXPENSE')
         AND normal_balance = 'DEBIT')
        OR
        (account_type IN ('LIABILITY', 'EQUITY', 'REVENUE')
         AND normal_balance = 'CREDIT')
    )
);

-- Immutability: no UPDATE, no DELETE.
-- The trigger raises with a recognizable prefix so application code can
-- match on the message and surface a friendly error.
CREATE OR REPLACE FUNCTION ledger_accounts_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'ledger_immutable: accounts.% is forbidden (id=%)',
        TG_OP, OLD.id
        USING ERRCODE = 'P0001';
END;
$$;

CREATE TRIGGER accounts_immutable
    BEFORE UPDATE OR DELETE ON accounts
    FOR EACH ROW EXECUTE FUNCTION ledger_accounts_immutable();

-- ---------------------------------------------------------------------------
-- transactions
-- ---------------------------------------------------------------------------

CREATE TABLE transactions (
    id                       UUID        PRIMARY KEY,
    external_id              TEXT,
    description              TEXT,
    reverses_transaction_id  UUID        REFERENCES transactions(id),
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- external_id is unique only when present (partial unique index).
-- Multiple NULLs are allowed.
CREATE UNIQUE INDEX transactions_external_id_unique
    ON transactions(external_id)
    WHERE external_id IS NOT NULL;

-- Lookup reversals of a given transaction. Partial: most rows have NULL here.
CREATE INDEX transactions_reverses_idx
    ON transactions(reverses_transaction_id)
    WHERE reverses_transaction_id IS NOT NULL;

CREATE OR REPLACE FUNCTION ledger_transactions_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'ledger_immutable: transactions.% is forbidden (id=%)',
        TG_OP, OLD.id
        USING ERRCODE = 'P0001';
END;
$$;

CREATE TRIGGER transactions_immutable
    BEFORE UPDATE OR DELETE ON transactions
    FOR EACH ROW EXECUTE FUNCTION ledger_transactions_immutable();
