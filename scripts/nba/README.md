# NBA data ingestion

stats.nba.com is TLS-fingerprint-blocked for the Rust service's HTTP client
(Akamai), but works fine from Python. So NBA data acquisition lives here, and
the Bayesian fit stays in Rust (projection-service `/fit/nba`).

Flow:
1. `python scripts/nba/nba_ingest.py <out_dir> <from_year> <to_year>`
   Fetches `leaguegamelog` per season and writes `nba_players.csv` + `nba_logs.csv`.
2. `bash scripts/nba/nba_load.sh <out_dir>`
   Loads the CSVs into Postgres (`sports_players` + `player_game_logs`, sport='nba').
3. `curl -X POST localhost:8121/fit/nba?min_games=20`
   projection-service fits every NBA stat (EB recency-weighted NB/Gamma) ->
   `player_projections` + `posterior_features`; simulation-engine then draws 100k.

Re-run step 1-3 to refresh (current season included).
