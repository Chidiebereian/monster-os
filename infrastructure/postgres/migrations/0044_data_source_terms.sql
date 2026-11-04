-- Terms review of the data sources (2026-11-03, Phase 3). Records what each source's
-- published terms say; it does NOT change any status. Approving, restricting or retiring
-- a source is the owner's decision (UPDATE data_sources SET status = ... as the owner).
-- Notes are summaries, not legal advice; re-check before any public or commercial use.
BEGIN;

UPDATE data_sources SET terms_url = 'https://disneytermsofuse.com/english/', terms_reviewed_at = DATE '2026-11-03',
  terms_notes = 'Disney Terms of Use (cover ESPN): prohibit extracting content with robots, data mining and scraping, and any commercial or business use. No public API licence.'
  WHERE source_id = 'espn_site';
UPDATE data_sources SET terms_url = 'https://www.nba.com/termsofuse', terms_reviewed_at = DATE '2026-11-03',
  attribution = 'Prominent attribution to NBA.com',
  terms_notes = 'NBA.com Terms of Use s.9: statistics for news reporting or private non-commercial use only, with attribution; not for gambling, fantasy or sponsorship; not in a comprehensive regularly updated statistics database without consent.'
  WHERE source_id = 'nba_stats';
UPDATE data_sources SET terms_url = 'https://gdx.mlb.com/components/copyright.txt', terms_reviewed_at = DATE '2026-11-03',
  terms_notes = 'MLBAM copyright notice: only individual, non-commercial, non-bulk use; anything else needs written permission from MLBAM.'
  WHERE source_id = 'mlb_statsapi';
UPDATE data_sources SET terms_url = 'https://www.nhl.com/info/terms-of-service', terms_reviewed_at = DATE '2026-11-03',
  terms_notes = 'NHL Terms of Service: no unauthorized spidering, scraping or harvesting; content for non-commercial, informational, personal use; copying into a database needs permission.'
  WHERE source_id = 'nhl_api';
UPDATE data_sources SET terms_url = 'https://moneypuck.com/data.htm', terms_reviewed_at = DATE '2026-11-03',
  attribution = 'Credit MoneyPuck.com',
  terms_notes = 'Free for non-commercial use and ad-hoc journalism, with credit; other uses by request; unapproved scraping is blocked.'
  WHERE source_id = 'moneypuck';
UPDATE data_sources SET terms_url = 'https://github.com/nflverse/nflverse-data', terms_reviewed_at = DATE '2026-11-03', licence = 'CC-BY-4.0',
  attribution = 'nflverse',
  terms_notes = 'Data releases published under CC-BY-4.0 (attribution). FTN charting is licensed separately (source nflverse_ftn).'
  WHERE source_id = 'nflverse';
UPDATE data_sources SET terms_url = 'https://polymarket.com/tos', terms_reviewed_at = DATE '2026-11-03',
  terms_notes = 'Polymarket Terms: personal, limited licence; no data mining tools, robots or crawlers; no reselling or redistributing data without written agreement. Public API use is not described separately.'
  WHERE source_id = 'polymarket';
UPDATE data_sources SET terms_url = 'https://the-odds-api.com/terms-and-conditions.html', terms_reviewed_at = DATE '2026-11-03',
  terms_notes = 'Data may be displayed in apps and dashboards (commercial too), stored and used for research and ML; it may not be resold or redistributed as a standalone data product. No attribution required.'
  WHERE source_id = 'the_odds_api';
UPDATE data_sources SET terms_notes = coalesce(terms_notes, '') || ' Terms page not retrieved on 2026-11-03 (404); review pending.'
  WHERE source_id = 'sportsgameodds' AND terms_reviewed_at IS NULL AND terms_notes NOT LIKE '%not retrieved%';
UPDATE data_sources SET terms_notes = coalesce(terms_notes, '') || ' Data terms not retrieved on 2026-11-03 (rate limited); review pending.'
  WHERE source_id = 'kalshi' AND terms_reviewed_at IS NULL AND terms_notes NOT LIKE '%not retrieved%';

INSERT INTO data_sources (source_id, name, url, sports, data, access, licence, attribution, cost, priority, terms_url, terms_reviewed_at, terms_notes)
VALUES ('nflverse_ftn', 'FTN charting data via nflverse', 'https://github.com/nflverse/nflverse-data', '{nfl}', '{charting}', 'open_data', 'CC-BY-SA-4.0',
        'FTN Data via nflverse', 'free', 3, 'https://github.com/nflverse/nfl_data_py', DATE '2026-11-03',
        'CC-BY-SA 4.0 per the nflverse documentation (share-alike applies to derived datasets); 2022 season onwards. FTN''s own terms not reviewed.')
ON CONFLICT (source_id) DO NOTHING;

COMMIT;
