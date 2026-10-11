-- Per-window leaderboard: a wallet can have a performance snapshot per time
-- window (all | 1d | 1w | 1m). pnl/volume are window-specific (Polymarket
-- leaderboard windows); win_rate/edge are overall and live on the 'all' rows.
-- Idempotent for an existing database.
-- "window" is a reserved word in Postgres, so the column is time_window.
ALTER TABLE trader_performance ADD COLUMN IF NOT EXISTS time_window TEXT NOT NULL DEFAULT 'all';
ALTER TABLE trader_performance DROP CONSTRAINT IF EXISTS trader_performance_pkey;
ALTER TABLE trader_performance ADD PRIMARY KEY (wallet, as_of, time_window);
