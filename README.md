# ledger

A double-entry accounting ledger over HTTP. Rust + Postgres + Axum.

Built to demonstrate the correctness, concurrency, and idempotency guarantees a payments-grade system requires. It is deliberately small — three crates, four migrations, around two thousand lines of Rust — and deliberately strict about what *must* be true regardless of how the system is misused.

This README explains what the service does, why each design choice was made, and the specific edge cases the test suite covers. The headline number, if you want one: **66 tests + a 200-concurrent stress test pass; the hot path is p50 3.6 ms, p99 4.1 ms over real HTTP against real Postgres.**

---

## Why this ledger is good

Six properties that taken together are unusual to find in a portfolio-sized project, and that anyone reviewing a payments-engineering codebase should ask for:

### 1. Invariants are enforced by the database, not the application

Every ledger invariant — "every transaction balances," "no overdraft on a strict account," "postings are immutable" — is enforced inside Postgres with CHECK constraints, foreign keys, and deferred constraint triggers. A buggy patch in the API layer, an admin SQL session, a half-applied migration, a maintenance script — none of them can produce an unbalanced transaction or mutate a posting. The application can return a nicer error message, but it is not the line of defense.

This is deliberate. A ledger is the system of record. Putting its invariants in application code makes them as strong as your weakest caller. Putting them in the schema makes them as strong as Postgres.

### 2. Idempotency that survives every failure mode

`POST /transactions` requires an `Idempotency-Key` header. The server combines it with a SHA-256 of the **canonical JSON** form of the body (object keys sorted, whitespace removed) so two semantically-equal requests produce the same hash. The lifecycle of a key is a four-state machine — `(no row) → PENDING → COMPLETED | FAILED` — implemented at the DB level with `INSERT … ON CONFLICT DO NOTHING RETURNING`.

Five client behaviors are covered:

- **First request:** processed normally, response stored against the key.
- **Replay with same body:** stored response returned verbatim, plus an `Idempotent-Replayed: true` header so clients can instrument retry-vs-first-attempt.
- **Replay with different body:** `409 Conflict { code: "key_conflict" }`. Reusing a key with different data is almost always a client bug; we surface it immediately.
- **Concurrent in-flight replay:** Postgres's `PRIMARY KEY (key)` constraint serializes inserts. One request wins the PENDING slot; the others see PENDING and get `409 in_flight`. We **do not** block-and-wait — that's hostile to clients whose timeouts are shorter than worst-case server processing.
- **Expired key (24h TTL):** treated as a fresh request; a background sweeper removes the row.

Errors are replayable too. A `422 Unprocessable Entity` (unbalanced posting, overdraft) or a `400 Bad Request` (malformed body) marks the idempotency row `FAILED` with the response stored, so the same key + same body replay returns the same error. Only genuinely non-deterministic failures (process crash mid-write) leave the row PENDING, where an operator-gated orphan-recovery query promotes it to FAILED after an hour.

### 3. Concurrency under serializable isolation, proven by a stress test

Every `POST /transactions` opens its own Postgres transaction at `ISOLATION LEVEL SERIALIZABLE`. Postgres's serializable snapshot isolation (Cahill, Röhm, Fekete, SIGMOD 2008) gives strict serializability: any concurrent schedule produces an outcome equivalent to *some* serial execution, or one of the conflicting transactions is aborted with `SQLSTATE 40001`. On abort we retry up to five times with exponential backoff (10–160 ms); past that we return `503` and mark the idempotency row FAILED.

No explicit `SELECT … FOR UPDATE`. Row locks on the `accounts` table would buy nothing — the contended target is *balances derived from `postings`*, not the `accounts` config row. SSI watches the read-write dependencies on the right rows automatically.

A stress test fires **200 concurrent `POST /transactions`** at 20 shared accounts and asserts (a) the ledger still balances, (b) every account's reported balance matches a client-side simulator that mirrors only the successes. Result on a 2026 M-series Mac with OrbStack + Postgres 16: all 200 commit in about 3 seconds with zero retries.

### 4. Faithful representation of double-entry

Each posting has three columns: `direction` (DEBIT or CREDIT), `amount_minor` (positive integer), `currency` (CHAR(3) ISO-4217 code). Balance is computed with `SUM(CASE WHEN direction = normal_balance THEN amount ELSE -amount END)`. Magnitude + Direction is what every accounting textbook uses and what every serious reference implementation (TigerBeetle, Stripe Ledger, Modern Treasury) exposes. A signed-amount column would let a missing minus sign silently flip a debit into a credit; here it cannot.

Money is integer minor units throughout. No floats anywhere — not in the schema, not in Rust, not in JSON. `i64` matches Postgres BIGINT one-to-one and gives ±9.2 × 10¹⁸ headroom, enough for any currency at any reasonable scale.

### 5. Observability you can actually run dashboards from

- **Structured per-request logs** via `tower-http::TraceLayer` — one INFO span per request with method, URI, status, latency. Toggle to newline-delimited JSON with `LEDGER_LOG_FORMAT=json` for log shippers.
- **Prometheus `/metrics`** with route-level latency histograms (labels: method, **matched route template**, status — using the template, not the raw URI, keeps cardinality bounded). Counters for idempotency outcomes (`fresh` / `in_flight` / `conflict` / `replay`). Histogram buckets cover 1ms .. 10s.
- **`/healthz`** (liveness, always 200 if reachable) and **`/readyz`** (pings the DB, returns 503 if the pool is broken) for Kubernetes / load balancers.
- **Operational guards** set per Postgres connection: `statement_timeout = 5s`, `idle_in_transaction_session_timeout = 10s`. A runaway query or forgotten transaction can never pin the pool.
- **Graceful shutdown** on SIGTERM/SIGINT. The Axum server stops accepting new connections and drains in-flight work; the sweeper task observes the same signal and finishes its current cycle before exiting.

### 6. A test suite where each layer is verified independently

Three test approaches, each catching a different class of bug:

- **Unit tests** in `ledger-core` — pure types, no I/O — verify the algebra of `MinorUnit`, `Currency`, normal-balance derivation, canonical JSON.
- **Integration tests** in `ledger-db` and `ledger-api` use **real Postgres** via `testcontainers` (each test gets its own ephemeral DB) and real HTTP via `reqwest`. No mocks.
- **Property tests** with `proptest` run randomized transaction sequences through the full HTTP stack and compare against an in-memory reference model. When something breaks, proptest shrinks the failure to a minimal reproducer.

Plus a **stress test** under `#[ignore]` (run separately) and a `criterion` benchmark on the hot path.

---

## Architecture

```
ledger/
├── ledger-core/   pure types, no I/O — Money, AccountType, Direction,
│                  Currency, MinorUnit, canonical JSON, validation
├── ledger-db/     sqlx queries — compile-time SQL checking, SERIALIZABLE
│                  transaction with retry, the four-state idempotency
│                  machine, the TTL sweeper
├── ledger-api/    Axum routes — handlers, ProblemJSON errors, per-request
│                  metrics, /healthz, /readyz, /metrics, graceful shutdown
├── migrations/    0001..0004 — schema with deferred constraint triggers
└── scripts/       smoke.sql, verify_smoke.sh, test.sh
```

Three crates because the boundaries are useful, not because layering is virtuous on its own. `ledger-core` compiles in milliseconds and is unit-testable without Docker; `ledger-db` keeps every SQL string in one place and uses sqlx's compile-time check on every one; `ledger-api` is a thin shell over `ledger-db` that handles HTTP plumbing.

---

## Invariants and where they're enforced

| # | Invariant | Layer | Mechanism |
|---|---|---|---|
| I1 | `amount_minor > 0`, currency is 3 uppercase ASCII letters | DB | `CHECK` on `postings` |
| I2 | Account type ↔ normal-balance mapping is consistent | DB | `CHECK` on `accounts` |
| I3 | Postings reference existing accounts & transactions | DB | Foreign keys |
| I4 | Postings, transactions, accounts are immutable (no UPDATE / DELETE) | DB | `BEFORE UPDATE OR DELETE` triggers raise `ledger_immutable` |
| I5 | Every transaction has ≥ 2 postings | DB (deferred) | `CONSTRAINT TRIGGER … DEFERRABLE INITIALLY DEFERRED`, fires at COMMIT |
| I6 | `Σ debits = Σ credits` per currency per transaction | DB (deferred) | Same trigger as I5 |
| I7 | One posting line never mixes currencies | DB | Single `currency` column per row — structurally impossible |
| I8 | No overdraft unless `accounts.allow_negative = TRUE` | DB (deferred) | Trigger recomputes the affected accounts' balances at COMMIT |
| I9 | Idempotency key uniqueness | DB | `PRIMARY KEY (key)` + `INSERT … ON CONFLICT DO NOTHING` |
| I10 | Idempotency state machine integrity | DB | Compound `CHECK` correlating `status` with the response columns |

### Why deferred constraint triggers for I5/I6/I8

Cross-row invariants need to see *every* posting of the user's transaction together. A regular `AFTER INSERT FOR EACH ROW` trigger fires after the first row, before its sibling postings exist — at that point the sum can never balance. Postgres's `CREATE CONSTRAINT TRIGGER … DEFERRABLE INITIALLY DEFERRED` fires at `COMMIT`, with the full transaction visible, which is exactly the right semantics. Postgres uses the same mechanism internally for `DEFERRABLE` foreign keys.

### Why immutability via trigger rather than `REVOKE`

`REVOKE UPDATE, DELETE` is per-role: a privileged role bypasses it. A trigger fires for every caller regardless of role. Stronger and simpler.

---

## Edge cases the test suite covers

This is the part I want anyone reviewing the project to look at. Every bullet below has at least one test asserting the behavior.

### Schema and DB-level invariants

- `account_type = ASSET` with `normal_balance = CREDIT` is rejected by the consistency CHECK.
- `UPDATE accounts SET name = 'x'` raises `ledger_immutable`.
- `DELETE FROM accounts WHERE id = ...` raises `ledger_immutable`.
- `UPDATE postings SET amount_minor = 999` raises `ledger_immutable`.
- `DELETE FROM postings WHERE id = ...` raises `ledger_immutable`.
- `UPDATE transactions SET description = 'x'` raises `ledger_immutable`.
- `DELETE FROM transactions WHERE id = ...` raises `ledger_immutable`.
- Posting with `amount_minor = 0` or negative is rejected by the CHECK.
- Posting with `currency = 'usd'` (lowercase), `'US'` (too short), `'USDX'` (too long), `'US1'` (digit), or `'US '` (space) is rejected.
- Single-posting transaction is rejected at COMMIT by the deferred trigger.
- Unbalanced two-posting transaction (DEBIT 100, CREDIT 50) is rejected at COMMIT.
- Two-currency transaction with imbalance in one currency only (USD balanced, EUR unbalanced) is rejected.
- Crediting an asset account past zero (overdraft) is rejected when `allow_negative = false`.
- Crediting an asset account past zero is accepted when `allow_negative = true` (used to model credit lines, contra-accounts, suspense accounts).
- Posting referencing a non-existent `account_id` is rejected by the foreign key with `SQLSTATE 23503`, surfaced as `422 unknown_account`.
- Inserting an `idempotency_keys` row with `status = 'PENDING'` and a non-null `response_status` is rejected by the compound CHECK.
- Inserting an `idempotency_keys` row with `status = 'COMPLETED'` but a NULL `response_body` is rejected.

### Idempotency

- First request with a fresh key returns `201 Created` and does NOT carry `Idempotent-Replayed: true`.
- Replay with the same key and identical body returns the original response and DOES carry `Idempotent-Replayed: true`.
- Replay with the same key but a different body returns `409 Conflict { code: "key_conflict" }`.
- Two concurrent requests with the same key are serialized by Postgres's `PRIMARY KEY (key)` constraint. Exactly one wins the PENDING slot; the others see either `PENDING` (and get `409 in_flight`) or `COMPLETED` (and get the stored response with the replay header).
- A request whose body parses as valid JSON but fails structural validation (e.g., `"account_id": "<bad>"` isn't a valid UUID) returns `400 invalid_json` and marks the idempotency row `FAILED`. A subsequent replay of the *same body* returns the stored 400 with the replay header — not `409 in_flight` and not `409 key_conflict`.
- Validation errors (`422 unbalanced`, `422 overdraft`) are stored and replayable.
- Forcing `expires_at` into the past on a `COMPLETED` row, then replaying, produces a fresh `201` — the stale row is deleted and the work runs again.
- Forcing `expires_at` into the past, then sending a *different* body, produces `201` (not `409 key_conflict`) — the old row is gone, so there's nothing to conflict with.
- An `Idempotency-Key` that's empty or longer than 255 characters returns `400 invalid_idempotency_key`.
- A `POST /transactions` with no `Idempotency-Key` header at all returns `400 missing_idempotency_key`.

### Multi-currency

- A single transaction can include postings in multiple currencies, as long as each currency balances independently.
- `GET /accounts/:id` returns balances as an array, one entry per currency seen on that account's postings.
- `GET /accounts/:id/postings?currency=USD` returns only USD postings.
- Filter is enforced in SQL via `($N::char(3) IS NULL OR currency = $N)`, so the filter combines correctly with cursor pagination.

### Pagination

- 30 transactions × 2 postings × default `limit=10` paginate cleanly. All 30 are returned, no duplicates, no skipped entries.
- Cursor is `base64url(created_at_micros|posting_id)`. The composite key keeps ordering stable when two postings share the same microsecond timestamp (which happens for the two legs of one transaction).
- `?order=asc` is supported alongside the default `?order=desc`.
- `?limit` is capped server-side at 200.
- When the requested page contains exactly `limit` items and there are no more rows, `next_cursor` is `null`.

### Reversal

- Posting the mirror image of an existing transaction (every direction flipped, same amounts and currencies) returns affected balances to their pre-original values.
- A transaction can reference the one it reverses via `reverses_transaction_id` for audit traceability (no business logic is attached to this column — it's metadata).
- Multiple partial reversals of one original are allowed; reversals don't carry "consumed" state.

### Concurrency

- 200 simultaneous `POST /transactions` over 20 shared accounts: every request returns `201` (or, very rarely under heavier load, `503` retry-exhausted — but never `500`).
- Two concurrent `idempotency_begin` calls with the same key: one returns `Fresh`, the other returns `InFlight`. Tested at the DB layer and at the HTTP layer.
- Property: applying random sequences of balanced transactions to the real service and to an in-memory model yields identical balances for every (account, currency) pair after every step.
- Property: `Σ debits − Σ credits` per currency across the `postings` table is always 0, no matter what sequence ran.
- Property: applying each transaction and its mirror immediately afterwards returns every balance to 0.

### Operational

- `GET /healthz` always returns 200 if the process is reachable. No DB call — used by orchestrators to decide whether to restart the container.
- `GET /readyz` issues `SELECT 1` against the pool. 200 when it succeeds, 503 with a reason on failure.
- The TTL sweeper deletes `(status != PENDING AND expires_at < NOW())` rows.
- The TTL sweeper does NOT delete `PENDING` rows — those are handled by a separate orphan-recovery path for the crash-during-write case.
- On `SIGTERM`/`SIGINT`, the Axum server stops accepting new connections, finishes in-flight requests, and exits. The sweeper observes the same shutdown signal and finishes its current tick before exiting.

---

## API surface

| Method | Path | Notes |
|---|---|---|
| `POST` | `/accounts` | Create. Body: `{name, account_type, allow_negative?, metadata?}` |
| `GET` | `/accounts/:id` | Account + balances per currency, computed live from `postings`. |
| `GET` | `/accounts/:id/postings` | Cursor pagination. Query: `cursor`, `limit≤200`, `order=asc\|desc`, `currency`. |
| `POST` | `/transactions` | Multi-posting atomic insert. Requires `Idempotency-Key` header. |
| `GET` | `/transactions/:id` | Transaction with its postings. |
| `GET` | `/healthz` | Liveness — always 200 when the process is up. |
| `GET` | `/readyz` | Readiness — 200 only when the DB pool answers `SELECT 1`. |
| `GET` | `/metrics` | Prometheus text exposition. Histograms + counters. |

Errors use the shape `{"error": {"code": "<stable>", "message": "<human>"}}`. Stable codes (a non-exhaustive list): `not_found`, `unbalanced`, `overdraft`, `unknown_account`, `external_id_conflict`, `key_conflict`, `in_flight`, `missing_idempotency_key`, `invalid_idempotency_key`, `invalid_json`, `serialization_retry_exhausted`, `validation_failed`, `internal`.

**Money semantics are the caller's responsibility.** `amount_minor` is an opaque integer count of the smallest unit the caller uses for the named currency. The server does not look up ISO-4217 minor-unit conventions (USD cents vs. JPY yen vs. KWD/BHD fils with three decimals). It only requires consistency: every transaction balances in its own currency, the same value goes back out on read.

---

## Transaction lifecycle (sequence diagram)

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant A as Axum
    participant D as Postgres

    C->>A: POST /transactions<br/>Idempotency-Key: K, body B
    A->>A: hash = SHA-256(canonical_json(B))
    A->>D: INSERT idempotency_keys (K, hash, PENDING)<br/>ON CONFLICT DO NOTHING RETURNING
    alt new row (Fresh)
        D-->>A: ok
        A->>D: BEGIN ISOLATION LEVEL SERIALIZABLE
        A->>D: INSERT transactions
        A->>D: INSERT postings × N
        A->>D: COMMIT (deferred triggers fire here)
        alt success
            D-->>A: ok
            A->>D: UPDATE idempotency_keys SET COMPLETED, response_body
            A-->>C: 201 Created + body
        else 40001 serialization failure
            D-->>A: ERROR 40001
            Note over A: retry up to 5× with exponential backoff
        else 23514 invariant violation (unbalanced / overdraft)
            D-->>A: ERROR
            A->>D: UPDATE idempotency_keys SET FAILED, 422 body
            A-->>C: 422 Unprocessable Entity
        end
    else conflict (existing row)
        D-->>A: existing { hash, status, response_body }
        alt hash mismatch
            A-->>C: 409 Conflict (key_conflict)
        else status = PENDING
            A-->>C: 409 Conflict (in_flight)
        else status ∈ {COMPLETED, FAILED}
            A-->>C: stored response + Idempotent-Replayed: true
        end
    end
```

---

## Benchmarks

`cargo bench -p ledger-api --bench transaction_insert` runs 3,825 iterations through HTTP → Axum → Postgres against the local docker-compose Postgres.

| | Latency |
|---|---|
| Minimum | 3.31 ms |
| **p50** | **3.59 ms** |
| p90 | 3.90 ms |
| p95 | 3.93 ms |
| **p99** | **4.06 ms** |
| Maximum | 4.19 ms |
| Std dev | 0.18 ms |
| Throughput (single client thread) | ~278 req/s |

This is end-to-end: HTTP parse → JSON canonicalization → SHA-256 → idempotency `INSERT ON CONFLICT` → SERIALIZABLE BEGIN → transaction insert → two posting inserts → deferred trigger evaluation → COMMIT → idempotency UPDATE → JSON serialize → HTTP response. Hardware: 2026 M-series Mac, Postgres 16 in a Docker container on the same machine.

Full distribution charts: `target/criterion/transaction_insert_2line/post_2line_usd/report/index.html`.

---

## Quick start

```bash
# Prereqs: Docker (OrbStack or Docker Desktop) and Rust (rustup).
docker compose up -d postgres
cp .env.example .env

# Install sqlx-cli once:
cargo install sqlx-cli --version '~0.8' --no-default-features --features rustls,postgres

export DATABASE_URL=postgres://ledger:ledger@localhost:5432/ledger
sqlx migrate run

cargo run -p ledger-api
# → ledger-api listening bind=127.0.0.1:8080
```

```bash
# Create two accounts:
CASH=$(curl -s -XPOST localhost:8080/accounts \
  -H 'content-type: application/json' \
  -d '{"name":"Cash","account_type":"ASSET"}' | jq -r .id)

CUST=$(curl -s -XPOST localhost:8080/accounts \
  -H 'content-type: application/json' \
  -d '{"name":"Customer","account_type":"LIABILITY"}' | jq -r .id)

# Post a transaction. Note the required Idempotency-Key header:
curl -s -XPOST localhost:8080/transactions \
  -H 'content-type: application/json' \
  -H 'idempotency-key: my-first-txn' \
  -d "{\"postings\":[
       {\"account_id\":\"$CASH\",\"direction\":\"DEBIT\",\"amount_minor\":10000,\"currency\":\"USD\"},
       {\"account_id\":\"$CUST\",\"direction\":\"CREDIT\",\"amount_minor\":10000,\"currency\":\"USD\"}
     ]}" | jq

# Read balances (computed live from postings, no cache):
curl -s localhost:8080/accounts/$CASH | jq '.balances'
# → [{"currency":"USD","amount_minor":10000}]

# Replay the same request — same body, same key — server returns the
# original response plus the `Idempotent-Replayed: true` header:
curl -si -XPOST localhost:8080/transactions \
  -H 'content-type: application/json' \
  -H 'idempotency-key: my-first-txn' \
  -d "{ ...same body... }" | head -10
```

`scripts/smoke.sql` exercises every trigger directly via `psql` and prints PASS/FAIL for each — useful when investigating a schema change.

---

## Tests

| Suite | Tests | What it covers |
|---|---|---|
| `ledger-core` unit | 26 | Money / Currency / direction algebra; canonical JSON (including proptest cases on key reordering and idempotence) |
| `ledger-db` accounts | 4 | Create / get / not-found / normal-balance derivation |
| `ledger-db` transactions | 7 | Balanced commit, overdraft strict, allow_negative escape, unbalanced, single-posting, duplicate external_id, round-trip |
| `ledger-db` idempotency | 10 | Four-state machine in every combination, concurrent race, expiry, sweeper boundary |
| `ledger-api` happy_path | 10 | End-to-end HTTP for every route, including health and pagination |
| `ledger-api` idempotency | 6 | All five required client behaviors + replay of failed responses |
| `ledger-api` properties (proptest) | 2 | P1 conservation, P2 model equivalence, P3 reversal symmetry, on random sequences of up to 15 transactions |
| `ledger-api` stress (`#[ignore]`) | 1 | 200 concurrent transactions over 20 accounts |
| **Total** | **66** | |

Run:

```bash
./scripts/test.sh --workspace                                     # 65 tests (~30s)
./scripts/test.sh -p ledger-api --test stress -- --ignored        # +1 stress test
./scripts/test.sh -p ledger-api --test properties -- --nocapture  # property tests with visible output
```

`scripts/test.sh` auto-detects OrbStack vs Docker Desktop, sets `DOCKER_HOST` accordingly, and forwards arguments to `cargo test`.

### What proptest actually does

For each property test, proptest generates a fresh random sequence of balanced transactions over a small pool of accounts. The sequence is applied to the real service via HTTP, and in parallel to an in-memory `HashMap<(AccountId, Currency), i64>` that mirrors the SQL aggregation. After every step it asserts that the API and the model agree, and that posting-level conservation (`Σ debits − Σ credits = 0` per currency) still holds. When a property fails — and during development, one will eventually — proptest *shrinks* the failure to its minimal reproducer.

The first time I wrote the conservation property I had the identity wrong. Proptest shrunk the failure to a single 2-line transaction and showed me the answer. That kind of feedback isn't something unit tests reliably give you.

### What the stress test actually does

Twenty accounts (ten ASSET, ten LIABILITY, all `allow_negative=true` so we're testing concurrency, not the overdraft path). Two hundred independent tokio tasks each pick two distinct accounts pseudo-randomly, a currency from {USD, EUR, INR}, and an amount in [100, 9999], and POST. After every task completes, the test verifies `Σ debits − Σ credits = 0` per currency on the DB *and* a client-side simulator's balances match the API for every account. Run time: about 3 seconds.

---

## Operational behavior

A few things worth pointing at because they're often missing from portfolio code:

- **Connection pool sizing**: 25 connections by default, with `acquire_timeout = 5s`. Leaves 75 slots in Postgres's default `max_connections=100` for psql, sqlx-cli, and monitoring.
- **Per-connection setup**: each new connection runs `SET statement_timeout = '5s'` and `SET idle_in_transaction_session_timeout = '10s'` via sqlx's `after_connect` hook. A runaway query or a forgotten open transaction cannot pin pool slots.
- **TTL sweeper**: a tokio task ticks every 5 minutes and deletes `expires_at < NOW() AND status != 'PENDING'` rows. The sweeper observes the same shutdown signal as Axum so a `SIGTERM` brings both down cleanly.
- **PENDING orphans**: never touched by the regular sweeper. A separate operator-gated query (run by a CLI flag or admin endpoint) promotes `PENDING` rows older than one hour to `FAILED` with a 503 response stored. This covers the case where the process died after inserting PENDING but before writing the result.
- **Graceful shutdown**: `axum::serve(...).with_graceful_shutdown(...)` stops accepting new connections on signal, drains in-flight requests, then exits. The sweeper task receives the same signal and finishes its current cycle.
- **Logs**: pretty by default, JSON when `LEDGER_LOG_FORMAT=json`. Filter via `RUST_LOG` (`tracing-subscriber`'s `EnvFilter` syntax).

---

## What I'd build next

Listed roughly in priority order if this were going from portfolio to production:

1. **Trigger-maintained `account_balances` cache + continuous drift detection.** Today balances are computed on read via `SUM(...)`. At scale you'd cache them with an `AFTER INSERT` trigger that updates a `(account_id, currency, balance_minor)` row, plus a background sampler that recomputes from `postings` for 1% of accounts every minute and emits `ledger_balance_drift_total` per account. The cache costs you a drift-detection task; not having it costs you O(N) reads.
2. **Two-phase auth/capture transfers.** A `transaction_state` enum (`PENDING`, `POSTED`, `VOIDED`), a `pending_until` timeout, and a posting visibility rule (do pending postings count towards balance? for overdraft? for `GET /accounts/:id`?). Each is a real decision. The schema leaves room.
3. **Advisory locks on `hash(account_id, currency)`** if the stress test ever reports retry-exhaustion above ~0.1%. Locks would replace the implicit SSI dependency tracking with explicit serialization on the right dimension; the hook is right after `BEGIN` in the transaction insert path.
4. **AuthN/Z + multi-tenant scoping.** Every account and transaction carries a `tenant_id`; routes filter by it; an API token maps to a tenant.
5. **Webhooks** on transaction-posted events, with at-least-once delivery, retry, and a dead-letter table.
6. **FX postings** for cross-currency transactions (USD ↔ EUR with a rate lock at transaction time).
7. **Tamper-evident audit chain.** Each posting includes `prev_hash`, computed from the previous posting's row. Append-only is already enforced; adding the chain makes the audit cryptographic.
8. **Table partitioning** of `postings` by `created_at` range or `account_id` hash, once row counts cross ~100 million.

None of these belong in v1; they each multiply scope and require their own design memo.

---

## Inspiration and references

- **TigerBeetle** ([tigerbeetle.com](https://tigerbeetle.com)) — purpose-built financial database with a beautiful two-phase transfer model. Their batched-inserts and deterministic-replay approach is exactly what a high-volume payments ledger wants.
- **Stripe Ledger** — engineering blog posts on idempotency-key + canonical-request hashing and on append-only event design.
- **Pat Helland, "Immutability Changes Everything"** (CIDR 2015) — the philosophical case for append-only systems of record.
- **Pat Helland, "Idempotence Is Not a Medical Condition"** (ACM Queue, 2012) — the canonical essay on this topic.
- **Cahill, Röhm, Fekete, "Serializable Isolation for Snapshot Databases"** (SIGMOD 2008) — the SSI algorithm Postgres uses.
- **Berenson et al., "A Critique of ANSI SQL Isolation Levels"** (SIGMOD 1995) — pick your isolation deliberately.

---

## License

MIT OR Apache-2.0.
