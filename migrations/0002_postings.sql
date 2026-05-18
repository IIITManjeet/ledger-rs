-- 0002_postings.sql
-- The append-only fact table. Every other ledger quantity is derived from
-- this. Balance, account totals, history — all SUM(...) GROUP BY ... over postings.

CREATE TABLE postings (
    id              UUID        PRIMARY KEY,
    transaction_id  UUID        NOT NULL REFERENCES transactions(id),
    account_id      UUID        NOT NULL REFERENCES accounts(id),
    direction       direction   NOT NULL,
    amount_minor    BIGINT      NOT NULL,
    currency        CHAR(3)     NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Magnitude is always positive. Sign comes from `direction` at query time.
    CONSTRAINT postings_amount_positive CHECK (amount_minor > 0),

    -- ISO-4217 shape: exactly 3 uppercase ASCII letters.
    -- We do NOT check that the code is a real currency — that's the
    -- caller's responsibility. Minor-unit semantics (USD cents vs JPY
    -- yen vs KWD 3-decimal fils) live in the caller too.
    CONSTRAINT postings_currency_iso CHECK (currency ~ '^[A-Z]{3}$')
);

-- Fetch all postings of a transaction. Used by GET /transactions/:id.
CREATE INDEX postings_transaction_idx ON postings(transaction_id);

-- Composite index for two purposes:
--   1. Cursor pagination of GET /accounts/:id/postings (ORDER BY created_at DESC, id DESC).
--   2. Balance computation: SUM(...) WHERE account_id = $1 GROUP BY currency.
-- Postgres can use this for both because the leading column is account_id.
CREATE INDEX postings_account_created_idx
    ON postings(account_id, created_at DESC, id DESC);

-- Immutability. As with accounts and transactions, UPDATE/DELETE are forbidden.
-- This is what makes "reversals are new transactions" actually true.
CREATE OR REPLACE FUNCTION ledger_postings_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'ledger_immutable: postings.% is forbidden (id=%)',
        TG_OP, OLD.id
        USING ERRCODE = 'P0001';
END;
$$;

CREATE TRIGGER postings_immutable
    BEFORE UPDATE OR DELETE ON postings
    FOR EACH ROW EXECUTE FUNCTION ledger_postings_immutable();
