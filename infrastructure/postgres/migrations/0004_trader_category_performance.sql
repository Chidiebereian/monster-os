-- Per-category reconstructed performance for a trader (e.g. strong in politics,
-- weak in sports), so Smart Signals can weight a trade by the wallet's proof in
-- that category. Idempotent for an existing database.
CREATE TABLE IF NOT EXISTS trader_category_performance (
    wallet   TEXT NOT NULL,
    category TEXT NOT NULL,
    as_of    DATE NOT NULL,
    win_rate DOUBLE PRECISION,
    roi      DOUBLE PRECISION,
    resolved INTEGER,
    wins     INTEGER,
    PRIMARY KEY (wallet, category, as_of)
);
