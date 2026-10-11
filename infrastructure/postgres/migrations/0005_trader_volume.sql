-- Traded volume per trader snapshot, so Top Traders can sort by volume as well
-- as profit. Idempotent for an existing database.
ALTER TABLE trader_performance ADD COLUMN IF NOT EXISTS volume DOUBLE PRECISION;
