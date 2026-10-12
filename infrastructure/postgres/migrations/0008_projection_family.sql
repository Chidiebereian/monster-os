-- Make player_projections self-describing: which distribution family the
-- predictive came from (count = Negative-Binomial, continuous = Gamma) and the
-- human unit, so the API/UI and a future family-aware simulation draw know how
-- to treat the mean/sd/dispersion without a hard-coded lookup.
ALTER TABLE player_projections ADD COLUMN IF NOT EXISTS family TEXT;
ALTER TABLE player_projections ADD COLUMN IF NOT EXISTS unit   TEXT;
