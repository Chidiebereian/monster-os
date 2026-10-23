-- Price sensitivity per AI pick (picks.rs): the break-even price and the worst
-- price still worth playable_min_ev per unit at the published line.
ALTER TABLE ai_picks ADD COLUMN IF NOT EXISTS fair_price integer;
ALTER TABLE ai_picks ADD COLUMN IF NOT EXISTS playable_price integer;
