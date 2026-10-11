-- Smart Signals lifecycle: one row per (wallet, market, side). A signal stays
-- 'active' while it's tailable; when it goes stale/resolved/too-far it is moved
-- to 'history'. Idempotent for an existing database.
CREATE TABLE IF NOT EXISTS signals (
    id BIGSERIAL PRIMARY KEY,
    wallet TEXT NOT NULL,
    market_id TEXT NOT NULL,
    side TEXT NOT NULL,
    category TEXT,
    entry_price DOUBLE PRECISION,
    current_price DOUBLE PRECISION,
    score INTEGER,
    status TEXT NOT NULL DEFAULT 'active',   -- active | history
    close_reason TEXT,
    first_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at TIMESTAMPTZ,
    UNIQUE (wallet, market_id, side)
);
CREATE INDEX IF NOT EXISTS signals_status_idx ON signals (status, score DESC);
