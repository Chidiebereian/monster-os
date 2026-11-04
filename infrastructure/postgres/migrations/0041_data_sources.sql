-- Data-source registry (docs/architecture/monster-gpt-v2.md, section 3; master plan 6).
-- Every fact MonsterGPT shows names its source_id. Whether a source's terms allow a use
-- is a human decision: services may read this table but only the owner role changes it.
-- Seeded with the sources the code already calls. Their terms have NOT been reviewed yet
-- (status review_pending); the review is a Phase 3 task.
BEGIN;

CREATE TABLE IF NOT EXISTS data_sources (
    source_id         text PRIMARY KEY,
    name              text NOT NULL,
    url               text,
    sports            text[] NOT NULL DEFAULT '{}',
    data              text[] NOT NULL DEFAULT '{}',
    access            text NOT NULL CHECK (access IN ('official_public', 'open_data', 'paid_api', 'undocumented_public', 'internal')),
    licence           text,
    terms_url         text,
    terms_reviewed_at date,
    terms_notes       text,
    attribution       text,
    rate_limit        text,
    cost              text NOT NULL DEFAULT 'free',
    priority          int NOT NULL CHECK (priority BETWEEN 1 AND 5),  -- 1 official ... 5 archive (master plan 6.2)
    status            text NOT NULL DEFAULT 'review_pending' CHECK (status IN ('approved', 'review_pending', 'restricted', 'retired')),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

INSERT INTO data_sources (source_id, name, url, sports, data, access, cost, priority, rate_limit, terms_notes) VALUES
 ('espn_site', 'ESPN site API', 'https://site.api.espn.com/apis', '{nba,nfl,nhl,mlb}', '{schedules,scores,injuries,live,standings,athletes}',
  'undocumented_public', 'free', 4, NULL, 'Undocumented public endpoints; no published terms for API use found yet'),
 ('nflverse', 'nflverse data releases', 'https://github.com/nflverse', '{nfl}', '{play_by_play,schedules,rosters,charting}',
  'open_data', 'free', 3, NULL, 'Licence differs per repository and per upstream (FTN charting); check each'),
 ('nhl_api', 'NHL web API', 'https://api-web.nhle.com/v1', '{nhl}', '{games,box_scores,shots,schedules,playoffs}',
  'undocumented_public', 'free', 1, NULL, 'League-run but undocumented'),
 ('mlb_statsapi', 'MLB Stats API', 'https://statsapi.mlb.com/api', '{mlb}', '{games,box_scores,players}',
  'official_public', 'free', 1, NULL, 'League-run; reuse terms to be reviewed'),
 ('nba_stats', 'NBA.com stats and CDN', 'https://stats.nba.com/stats', '{nba}', '{stats,referees,schedules}',
  'official_public', 'free', 1, NULL, 'League-run; reuse terms to be reviewed'),
 ('moneypuck', 'MoneyPuck data', 'https://moneypuck.com/moneypuck', '{nhl}', '{shots,expected_goals}',
  'open_data', 'free', 3, NULL, 'Attribution expected; terms to be reviewed'),
 ('the_odds_api', 'The Odds API', 'https://api.the-odds-api.com/v4', '{nba,nfl,nhl,mlb}', '{market_prices}',
  'paid_api', 'paid plan (owner confirmed 2026-11-03)', 2, '9 credits/day budget (oddsapi_daily_credits)', 'Optional: only market-price answers depend on it'),
 ('sportsgameodds', 'SportsGameOdds', 'https://api.sportsgameodds.com/v2', '{nba,nfl,nhl,mlb}', '{market_prices,props}',
  'paid_api', 'paid plan (owner confirmed 2026-11-03)', 2, '2,500 entities/month (sgo_daily_entities)', 'Optional: only market-price answers depend on it'),
 ('polymarket', 'Polymarket public APIs', 'https://gamma-api.polymarket.com', '{nba,nfl,nhl,mlb}', '{prediction_market_prices,trades}',
  'official_public', 'free', 2, NULL, 'Read-only use; no order code exists'),
 ('kalshi', 'Kalshi public API', 'https://api.elections.kalshi.com/trade-api', '{nba,nfl,nhl,mlb}', '{prediction_market_prices}',
  'official_public', 'free', 2, NULL, 'Read-only use; no order code exists'),
 ('monster_os', 'Monster OS models and records', NULL, '{nba,nfl,nhl,mlb}', '{projections,simulations,validation,picks,docs}',
  'internal', 'free', 1, NULL, 'Own outputs: forecast ledger, registry, validation, documentation')
ON CONFLICT (source_id) DO NOTHING;
UPDATE data_sources SET status = 'approved', terms_reviewed_at = DATE '2026-11-03' WHERE source_id = 'monster_os' AND status = 'review_pending';

-- read-only for the services: a source's status is a human decision
DO $$ BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'monster_app') THEN
        REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON data_sources FROM monster_app;
        GRANT SELECT ON data_sources TO monster_app;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'monster_research') THEN
        GRANT SELECT ON data_sources TO monster_research;
    END IF;
END $$;

COMMIT;
