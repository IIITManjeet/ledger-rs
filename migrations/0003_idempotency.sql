-- 0003_idempotency.sql
-- Idempotency key store. One row per client-supplied key, lifecycle:
--     (no row) -> PENDING -> COMPLETED | FAILED
-- with FAILED also reachable directly when we mark retry-exhaustion (see PLAN §4 step 6).

CREATE TYPE idempotency_status AS ENUM ('PENDING', 'COMPLETED', 'FAILED');

CREATE TABLE idempotency_keys (
    key              TEXT                PRIMARY KEY,
    request_hash     BYTEA               NOT NULL,
    status           idempotency_status  NOT NULL,
    response_status  SMALLINT,
    response_body    JSONB,
    transaction_id   UUID                REFERENCES transactions(id),
    created_at       TIMESTAMPTZ         NOT NULL DEFAULT NOW(),
    completed_at     TIMESTAMPTZ,
    expires_at       TIMESTAMPTZ         NOT NULL DEFAULT (NOW() + INTERVAL '24 hours'),

    -- Key length sanity. 1..=255 chars.
    CONSTRAINT idempotency_key_length
        CHECK (char_length(key) BETWEEN 1 AND 255),

    -- We always store SHA-256, which is 32 bytes.
    CONSTRAINT idempotency_request_hash_sha256
        CHECK (octet_length(request_hash) = 32),

    -- State-machine integrity: response columns are NULL iff status is PENDING.
    CONSTRAINT idempotency_response_matches_status CHECK (
        (status = 'PENDING'
            AND response_status IS NULL
            AND response_body   IS NULL
            AND completed_at    IS NULL)
        OR
        (status IN ('COMPLETED', 'FAILED')
            AND response_status IS NOT NULL
            AND response_body   IS NOT NULL
            AND completed_at    IS NOT NULL)
    )
);

-- Sweeper scans this. PENDING rows are excluded — they're handled by a
-- separate operator-gated recovery job (see PLAN §4 step 7).
CREATE INDEX idempotency_keys_sweeper_idx
    ON idempotency_keys(expires_at)
    WHERE status <> 'PENDING';
