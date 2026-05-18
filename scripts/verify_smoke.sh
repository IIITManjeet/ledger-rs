#!/usr/bin/env bash
# Run the smoke test and check the resulting DB state against expectations.
# Usage:  ./scripts/verify_smoke.sh
# Exit 0 on full pass, non-zero on any mismatch.

set -uo pipefail
cd "$(dirname "$0")/.."

PSQL="docker compose exec -T postgres psql -U ledger -d ledger -t -A"

echo "--- 1. wiping previous smoke state ---"
$PSQL -c 'TRUNCATE postings, transactions, accounts, idempotency_keys CASCADE;' >/dev/null

echo "--- 2. running smoke.sql ---"
docker compose exec -T postgres psql -U ledger -d ledger -v ON_ERROR_STOP=0 \
    < scripts/smoke.sql > /tmp/smoke.out 2>&1

ERRS=$(grep -c '^ERROR' /tmp/smoke.out || true)
echo "    smoke output:    /tmp/smoke.out"
echo "    ERROR lines:     $ERRS  (expected 11 — one per negative test)"

fail=0
check() {
    local label=$1 want=$2 got=$3
    if [ "$got" = "$want" ]; then
        printf '  \033[32mPASS\033[0m  %-40s = %s\n' "$label" "$got"
    else
        printf '  \033[31mFAIL\033[0m  %-40s want=%s got=%s\n' "$label" "$want" "$got"
        fail=1
    fi
}

echo "--- 3. checking surviving state ---"

check 'accounts count'      3 "$($PSQL -c 'SELECT COUNT(*) FROM accounts;')"
check 'transactions count'  2 "$($PSQL -c 'SELECT COUNT(*) FROM transactions;')"
check 'postings count'      4 "$($PSQL -c 'SELECT COUNT(*) FROM postings;')"
check 'idempotency rows'    1 "$($PSQL -c 'SELECT COUNT(*) FROM idempotency_keys;')"

check 'Cash balance (USD)'        10000  "$($PSQL -c \
    "SELECT SUM(CASE WHEN p.direction = a.normal_balance THEN p.amount_minor ELSE -p.amount_minor END)
     FROM postings p JOIN accounts a ON a.id = p.account_id
     WHERE a.name = 'Cash' AND p.currency = 'USD';")"

check 'Customer balance (USD)'    5000   "$($PSQL -c \
    "SELECT SUM(CASE WHEN p.direction = a.normal_balance THEN p.amount_minor ELSE -p.amount_minor END)
     FROM postings p JOIN accounts a ON a.id = p.account_id
     WHERE a.name = 'Customer' AND p.currency = 'USD';")"

check 'Credit Line balance (USD)' -5000  "$($PSQL -c \
    "SELECT SUM(CASE WHEN p.direction = a.normal_balance THEN p.amount_minor ELSE -p.amount_minor END)
     FROM postings p JOIN accounts a ON a.id = p.account_id
     WHERE a.name = 'Credit Line' AND p.currency = 'USD';")"

check 'global conservation (USD)' 0      "$($PSQL -c \
    "SELECT SUM(CASE direction WHEN 'DEBIT' THEN amount_minor ELSE -amount_minor END)
     FROM postings WHERE currency = 'USD';")"

check 'ERROR lines in smoke output' 11 "$ERRS"

if [ "$fail" = 0 ]; then
    printf '\n\033[32mALL CHECKS PASSED\033[0m — every trigger fired as expected.\n'
    exit 0
else
    printf '\n\033[31mSOME CHECKS FAILED\033[0m — see /tmp/smoke.out for details.\n'
    exit 1
fi
