-- Category tag for traders (e.g. nfl, nba, politics, crypto) so copy-trading
-- leaderboards can be filtered by market category. Idempotent: safe to run on
-- an existing database as well as a fresh init.
ALTER TABLE trader_profiles ADD COLUMN IF NOT EXISTS category TEXT;
CREATE INDEX IF NOT EXISTS trader_profiles_category_idx ON trader_profiles (category);
