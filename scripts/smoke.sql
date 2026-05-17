-- scripts/smoke.sql
-- Manual smoke test for the ledger schema.
-- Run with:
--     docker compose exec -T postgres psql -U ledger -d ledger -v ON_ERROR_STOP=0 < scripts/smoke.sql
-- or (after `cp .env.example .env`):
--     psql "$DATABASE_URL" -v ON_ERROR_STOP=0 -f scripts/smoke.sql
--
-- For each numbered section, read the output:
--   • "no error" + expected SELECT result  → trigger allowed it (good for positive tests).
--   • "ERROR: ledger_…"                    → trigger rejected (good for negative tests).
--
-- The script does NOT clean up between runs. If you want a clean slate:
--     TRUNCATE postings, transactions, accounts, idempotency_keys CASCADE;

\set ON_ERROR_STOP 0
\set QUIET 1

\echo
\echo '===== 1. setup: create three accounts ====='
INSERT INTO accounts (id, name, account_type, normal_balance, allow_negative)
VALUES
    ('00000000-0000-7000-8000-000000000001', 'Cash',        'ASSET',     'DEBIT',  FALSE),
    ('00000000-0000-7000-8000-000000000002', 'Customer',    'LIABILITY', 'CREDIT', FALSE),
    ('00000000-0000-7000-8000-000000000003', 'Credit Line', 'ASSET',     'DEBIT',  TRUE)
ON CONFLICT (id) DO NOTHING;

SELECT id, name, account_type, normal_balance, allow_negative
FROM accounts ORDER BY id;

\echo
\echo '===== 2. inconsistent account_type/normal_balance should FAIL (CHECK) ====='
INSERT INTO accounts (id, name, account_type, normal_balance)
VALUES ('00000000-0000-7000-8000-0000000000ff', 'Bad', 'ASSET', 'CREDIT');

\echo
\echo '===== 3. UPDATE on accounts should FAIL (immutability trigger) ====='
UPDATE accounts SET name = 'oops' WHERE id = '00000000-0000-7000-8000-000000000001';

\echo
\echo '===== 4. DELETE on accounts should FAIL (immutability trigger) ====='
DELETE FROM accounts WHERE id = '00000000-0000-7000-8000-000000000001';

\echo
\echo '===== 5. balanced 2-line transaction should SUCCEED ====='
BEGIN;
INSERT INTO transactions (id, description)
VALUES ('00000000-0000-7000-8000-000000000101', 'top-up');
INSERT INTO postings (id, transaction_id, account_id, direction, amount_minor, currency) VALUES
    ('00000000-0000-7000-8000-000000001011', '00000000-0000-7000-8000-000000000101',
     '00000000-0000-7000-8000-000000000001', 'DEBIT',  10000, 'USD'),
    ('00000000-0000-7000-8000-000000001012', '00000000-0000-7000-8000-000000000101',
     '00000000-0000-7000-8000-000000000002', 'CREDIT', 10000, 'USD');
COMMIT;

\echo '----- balances after section 5 -----'
SELECT a.name, p.currency,
       SUM(CASE WHEN p.direction = a.normal_balance
                THEN p.amount_minor ELSE -p.amount_minor END) AS balance
FROM postings p JOIN accounts a ON a.id = p.account_id
GROUP BY a.id, a.name, p.currency ORDER BY a.name;

\echo
\echo '===== 6. unbalanced transaction should FAIL at COMMIT (deferred trigger) ====='
BEGIN;
INSERT INTO transactions (id) VALUES ('00000000-0000-7000-8000-000000000102');
INSERT INTO postings (id, transaction_id, account_id, direction, amount_minor, currency) VALUES
    ('00000000-0000-7000-8000-000000001021', '00000000-0000-7000-8000-000000000102',
     '00000000-0000-7000-8000-000000000001', 'DEBIT',  100, 'USD'),
    ('00000000-0000-7000-8000-000000001022', '00000000-0000-7000-8000-000000000102',
     '00000000-0000-7000-8000-000000000002', 'CREDIT',  50, 'USD');
COMMIT;

\echo
\echo '===== 7. single-posting transaction should FAIL at COMMIT (deferred trigger) ====='
BEGIN;
INSERT INTO transactions (id) VALUES ('00000000-0000-7000-8000-000000000103');
INSERT INTO postings (id, transaction_id, account_id, direction, amount_minor, currency) VALUES
    ('00000000-0000-7000-8000-000000001031', '00000000-0000-7000-8000-000000000103',
     '00000000-0000-7000-8000-000000000001', 'DEBIT', 100, 'USD');
COMMIT;

\echo
\echo '===== 8. UPDATE on postings should FAIL (immutability trigger) ====='
UPDATE postings SET amount_minor = 999
WHERE id = '00000000-0000-7000-8000-000000001011';

\echo
\echo '===== 9. DELETE on postings should FAIL (immutability trigger) ====='
DELETE FROM postings WHERE id = '00000000-0000-7000-8000-000000001011';

\echo
\echo '===== 10. overdraft on allow_negative=FALSE account should FAIL (deferred trigger) ====='
BEGIN;
INSERT INTO transactions (id) VALUES ('00000000-0000-7000-8000-000000000104');
-- Cash currently +10000 USD. Crediting 99999 would put it at -89999 → overdraft.
INSERT INTO postings (id, transaction_id, account_id, direction, amount_minor, currency) VALUES
    ('00000000-0000-7000-8000-000000001041', '00000000-0000-7000-8000-000000000104',
     '00000000-0000-7000-8000-000000000001', 'CREDIT', 99999, 'USD'),
    ('00000000-0000-7000-8000-000000001042', '00000000-0000-7000-8000-000000000104',
     '00000000-0000-7000-8000-000000000002', 'DEBIT',  99999, 'USD');
COMMIT;

\echo
\echo '===== 11. overdraft on allow_negative=TRUE account should SUCCEED ====='
BEGIN;
INSERT INTO transactions (id) VALUES ('00000000-0000-7000-8000-000000000105');
-- Credit Line is ASSET (DEBIT-normal), allow_negative=TRUE. Crediting 5000 → balance -5000 (allowed).
-- Customer is LIABILITY (CREDIT-normal), currently +10000. Debiting 5000 → +5000 (positive, fine).
INSERT INTO postings (id, transaction_id, account_id, direction, amount_minor, currency) VALUES
    ('00000000-0000-7000-8000-000000001051', '00000000-0000-7000-8000-000000000105',
     '00000000-0000-7000-8000-000000000003', 'CREDIT', 5000, 'USD'),
    ('00000000-0000-7000-8000-000000001052', '00000000-0000-7000-8000-000000000105',
     '00000000-0000-7000-8000-000000000002', 'DEBIT',  5000, 'USD');
COMMIT;

\echo
\echo '===== 12. negative amount_minor should FAIL (CHECK) ====='
INSERT INTO postings (id, transaction_id, account_id, direction, amount_minor, currency)
VALUES ('00000000-0000-7000-8000-0000000010ff',
        '00000000-0000-7000-8000-000000000101',
        '00000000-0000-7000-8000-000000000001',
        'DEBIT', -1, 'USD');

\echo
\echo '===== 13. malformed currency should FAIL (CHECK) ====='
INSERT INTO postings (id, transaction_id, account_id, direction, amount_minor, currency)
VALUES ('00000000-0000-7000-8000-0000000010fe',
        '00000000-0000-7000-8000-000000000101',
        '00000000-0000-7000-8000-000000000001',
        'DEBIT', 1, 'usd');

\echo
\echo '===== 14. idempotency: PENDING with non-null response should FAIL (CHECK) ====='
INSERT INTO idempotency_keys (key, request_hash, status, response_status, response_body)
VALUES ('bad-key',
        decode('1111111111111111111111111111111111111111111111111111111111111111', 'hex'),
        'PENDING', 201, '{"x":1}'::jsonb);

\echo
\echo '===== 15. idempotency: PENDING row inserted clean SUCCEEDS ====='
INSERT INTO idempotency_keys (key, request_hash, status)
VALUES ('good-key',
        decode('2222222222222222222222222222222222222222222222222222222222222222', 'hex'),
        'PENDING');

SELECT key, status, response_status, response_body, expires_at > NOW() AS not_expired
FROM idempotency_keys
WHERE key IN ('good-key', 'bad-key');

\echo
\echo '===== 16. final balances ====='
SELECT a.name, p.currency,
       SUM(CASE WHEN p.direction = a.normal_balance
                THEN p.amount_minor ELSE -p.amount_minor END) AS balance
FROM postings p JOIN accounts a ON a.id = p.account_id
GROUP BY a.id, a.name, p.currency ORDER BY a.name;
