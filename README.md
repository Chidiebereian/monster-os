# MonsterOS

Open-source quantitative sports and prediction-market intelligence platform.
Rust (Actix-Web, Tokio, Serde) services over PostgreSQL, ClickHouse, DuckDB,
Redis, MinIO, and NATS, with Stan/PyMC research and an Ollama/vLLM-backed
research assistant (MonsterGPT). No paid vendors required.

## Pipeline

```
Raw Events
  -> Data Quality Gate (7 checkpoints)      services/data-quality-service
  -> Feature Engineering                    apps/feature-store
  -> Bayesian Models                        research/ + services/bayesian-service
  -> Posterior Distributions                MinIO (Parquet/Arrow)
  -> Monte Carlo Simulations                apps/simulation-engine
  -> Probability Surfaces
  -> Fair Market Prices                     apps/pricing-engine
  -> Edge Detection                         apps/arbitrage-engine
  -> Portfolio Optimization                 services/portfolio-service, kelly-service
  -> LLM Explanations                       apps/monster-gpt
```

## Data quality gate

Bad data -> bad model. Every dataset passes seven checkpoints before reaching
the feature store or model training (`POST /check` on port 8113):

| # | Check | Behavior |
|---|-------|----------|
| 1 | Missing values | Fail if any column exceeds `max_missing_rate` (default 5%) |
| 2 | Duplicates | Fail on repeated `key_column` (or identical rows) |
| 3 | Data leakage | Fail on rows after `prediction_cutoff`, or a feature identical to the label |
| 4 | Wrong labels | Fail on missing labels or values outside `allowed_labels` |
| 5 | Outliers | Warn on \|z\| > `outlier_z` (default 4) — investigate, don't auto-delete |
| 6 | Sampling bias | Warn if a group in `group_column` is under `min_group_share` |
| 7 | Distribution shift | Fail if train-vs-production PSI > `max_psi` (default 0.25) |

Returns 200 when passed, 422 with the full report when any check fails.
Reports are persisted to `data_quality_runs`.

## Layout

```
apps/            product surfaces (gateway, engines, registry, MonsterGPT, dashboard)
services/        per-sport ingestion + modeling primitives + data quality
common/          shared SportModel trait and payload types
research/        notebooks, Stan/PyMC models, experiments, tutorials, papers
models/          model families (Elo, Bradley-Terry, Poisson, NegBin, Bayesian-*, PRP, TOOR, GSSD, ZSD)
infrastructure/  docker-compose, Postgres + ClickHouse schemas, monitoring
docs/            architecture notes
```

## Ports

| Port | Service | Port | Service |
|------|---------|------|---------|
| 8080 | api-gateway | 8100–8112 | services/* (see main.rs) |
| 8081 | simulation-engine | 8113 | data-quality-service |
| 8082 | pricing-engine | 8114–8119 | remaining apps |
| 8120 | odds-service | | |
| 8083 | arbitrage-engine | 5432 | Postgres |
| 8084 | model-registry | 8123 | ClickHouse HTTP |
| 8085 | monster-gpt | 11434 | Ollama |

Every service exposes `GET /healthz`. Override any port with `PORT=`.

## Quick start

```bash
cp .env.example .env
docker compose -f infrastructure/docker-compose.yml --env-file .env up -d
cargo build --workspace
cargo test --workspace
./target/debug/api-gateway          # GET localhost:8080/ lists upstreams
./target/debug/data-quality-service # POST localhost:8113/check
```

Everything is reachable through the gateway at `/api/{service}/{path}`, e.g.

```bash
curl "localhost:8080/api/simulation-engine/simulate?mean=117&sd=10&n=10000&seed=42"
curl "localhost:8080/api/pricing-engine/price?probability=0.61"            # fair American odds
curl "localhost:8080/api/pricing-engine/implied?odds=-150"                 # implied win probability
curl "localhost:8080/api/pricing-engine/novig?odds_a=-150&odds_b=130"      # strip the vig, two-sided
curl "localhost:8080/api/kelly-service/kelly?probability=0.55&odds=120&fraction=0.5"  # half-Kelly stake
curl "localhost:8080/api/kelly-service/ev?probability=0.55&odds=120&stake=100"        # expected value / ROI
curl "localhost:8080/api/kelly-service/hedge?odds_a=150&stake_a=100&odds_b=-120"      # lock-in hedge stake
curl "localhost:8080/api/kelly-service/parlay?odds=-110,-110,120"          # combined parlay odds
curl "localhost:8080/api/copy-trading/leaderboard?metric=pnl&category=nfl"  # top traders (needs DATABASE_URL)
curl -XPOST localhost:8080/api/copy-trading/copy/size -H 'content-type: application/json' \
     -d '{"bankroll":1000,"max_position_fraction":0.05,"slippage_tolerance":0.02,"reference_price":0.50,"current_price":0.50,"copy_ratio":0.10}'
     # risk-controlled copy stake: proportional sizing, position/budget caps, slippage gate
curl -XPOST localhost:8080/api/copy-trading/autofollow -H 'content-type: application/json' \
     -d '{"user_id":1,"wallet":"0x...","bankroll":1000,"budget":200,"max_position_fraction":0.05,"slippage_tolerance":0.02,"copy_ratio":0.10,"dry_run":true}'
     # mirror a trader's recent wallet_trades (ClickHouse) through the risk engine; dry_run advises, false records
curl -XPOST localhost:8080/api/whale-tracker/ingest -H 'content-type: application/json' \
     -d '{"wallets":["0x..."],"limit":100,"min_cash":1000,"category":"politics"}'
     # pull a wallet's real Polymarket trades into ClickHouse wallet_trades (feeds copy-trading/autofollow)
curl "localhost:8080/api/whale-tracker/trades?wallet=0x...&limit=20"   # read back stored trades
curl "localhost:8080/api/whale-tracker/exposure?wallet=0x..."         # net positions per market (trader profile)
curl "localhost:8080/api/copy-trading/leaderboard?metric=volume&window=7d&category=politics"
     # Top Traders by volume for a time window (all|1d|7d|30d); win_rate joined from overall rows
curl "localhost:8080/api/sport-intelligence/signals?mode=precision&category=sports&include_venue=true"
     # Smart Signals: score tracked wallets' recent sports trades 0-100 (wallet quality, category
     # proof, entry/slippage, conviction, recency); precision=clean+tailable only, flow=broader.
     # include_venue=true folds the Polymarket-vs-Kalshi best-venue compare into each card.
curl "localhost:8080/api/sport-intelligence/consensus?category=sports&min_wallets=2"
     # Consensus signals: markets where multiple tracked sharps are on the same side
curl -XPOST "localhost:8080/api/sport-intelligence/signals/snapshot?mode=precision&category=sports"
     # persist current signals: tailable ones stay active, stale/resolved ones move to history
curl "localhost:8080/api/sport-intelligence/signals/active"    # live tail-able signals
curl "localhost:8080/api/sport-intelligence/signals/history"   # retired signals + close reason
     # set SNAPSHOT_INTERVAL_SECS to have sport-intelligence run the snapshot on its own loop
curl "localhost:8080/api/sport-intelligence/match?polymarket=nba-okc-atl-2026-10-12"
     # resolve a Polymarket game slug to the equivalent Kalshi ticker (team abbrevs + date)
curl "localhost:8080/api/sport-intelligence/best-venue?polymarket=<slug>&side=yes&min_depth=500&min_top=100"
     # compare the same game on both venues (Kalshi auto-matched); cheaper EXECUTABLE entry wins.
     # min_depth/min_top gate on order-book liquidity (shares within 2c / at best ask); &kalshi= overrides.
curl -XPOST localhost:8080/api/whale-tracker/refresh-leaderboard -H 'content-type: application/json' \
     -d '{"window":"all","limit":20,"ingest_trades":true}'
     # snapshot top traders by real Polymarket profit into trader_performance (powers copy-trading /leaderboard)
curl -XPOST localhost:8080/api/whale-tracker/compute-performance -H 'content-type: application/json' \
     -d '{"wallets":["0x..."]}'
     # reconstruct real win-rate & ROI (overall + per category) from trades vs each market's resolution
     # (per-category breakdown also surfaces on /api/copy-trading/traders/{wallet})
curl "localhost:8080/api/arbitrage-engine/edges?fair_probability=0.61&price=0.53"
curl "localhost:8080/api/prediction-market-service/quotes/kalshi/KXMLBGAME-26OCT081700CLECWS-CWS"
curl "localhost:8080/api/prediction-market-service/orderbook/polymarket/nba-okc-atl-2026-10-12"
     # normalized YES order book (bids/asks) from Polymarket CLOB or Kalshi
curl "localhost:8080/api/arbitrage-engine/edges?fair_probability=0.61&venue=polymarket&market_id=<slug>"  # live ask
curl "localhost:8080/api/odds-service/odds/baseball_mlb"   # needs ODDS_API_KEYS in .env
curl "localhost:8080/api/odds-service/screen/baseball_mlb" # Odds Screen: line-shopping board,
     # best number per outcome + which book + each book's price + no-vig fair + value edge
curl "localhost:8080/api/odds-service/keys"                # masked key rotation state
curl "localhost:8080/api/arbitrage-engine/scan?sport=baseball_mlb&series=KXMLBGAME&min_edge=0.01&min_top=200"
     # every open Kalshi game market vs sportsbook no-vig consensus, net of est. Kalshi fee;
     # min_depth/min_top gate opportunities on real Kalshi order-book liquidity (drops non-executable)
curl "localhost:8080/api/model-registry/models?sport=MLB&status=active"     # needs DATABASE_URL
curl -XPUT localhost:8080/api/feature-store/posteriors/mlb-run-bucket-v1 -H 'content-type: application/json' \
     -d '[{"parameter":"total_runs","mean":8.6,"sd":1.9,"rhat":1.002,"ess":3800}]'
curl "localhost:8080/api/simulation-engine/simulate?model_id=mlb-run-bucket-v1&parameter=total_runs&event_id=NYY-BOS-2026-10-08"
curl "localhost:8080/api/feature-store/features/team/MLB/NYY?as_of=2026-10-05T00:00:00Z"  # point-in-time
curl -XPOST localhost:8080/api/monster-gpt/ask -H 'content-type: application/json' \
     -d '{"question":"Is there value on the Yankees ML?","context":{"fair_probability":0.61}}'
curl "localhost:8080/api/monster-gpt/retrieve?q=value%20on%20the%20Yankees%20NYY%20in%20MLB"
     # what MonsterGPT auto-retrieves: team features, active models, research snippets (RAG)
```

Override an upstream with `<SERVICE>_URL`, e.g. `PRICING_ENGINE_URL=http://pricing:8082`.

## Toolchain note

`Cargo.lock` was generated on Rust 1.75 and pins several transitive crates
(actix-http 3.5.1, url 2.4.1, time 0.3.36, indexmap 2.2.6, etc.) to older
releases. On a current toolchain, delete `Cargo.lock` and relax the
`actix-web = "=4.4.1"` pins to `"4"` to pick up the latest versions.

## Status

Every crate compiles and serves `/healthz`. Implemented and tested:
data-quality-service, feature-store (posterior summaries, simulation results,
point-in-time team/player/market features in Postgres), simulation-engine
(seeded Monte Carlo from a feature-store posterior or explicit mean/sd;
rejects posteriors with R-hat > 1.01),
prediction-market-service (live Kalshi/Polymarket top-of-book, plus normalized
YES order books via `/orderbook/{venue}/{id}` — Polymarket CLOB and Kalshi, from their
public APIs), arbitrage-engine (edge/ROI vs sportsbook odds, a given contract
price, or the live ask on either side of a Kalshi/Polymarket book; `/scan`
matches a Kalshi series to sportsbook games by team label and start time and
ranks edges after an estimated Kalshi taker fee, flagging in-play games;
`min_depth`/`min_top` gate opportunities on each market's real order-book
liquidity, dropping non-executable ones),
odds-service (sportsbook moneylines from The Odds API with best price per
outcome and no-vig consensus; `/screen/{sport}` is the Odds Screen line-shopping
board — per outcome the best number, which book offers it, every book's price,
the no-vig fair, and the best price's value edge, sorted best-value first;
rotates `ODDS_API_KEYS`, parking invalid, exhausted and rate-limited keys, and
caches responses for `ODDS_CACHE_SECS`),
model-registry (Postgres CRUD over `models`), monster-gpt (OpenAI-compatible
chat via `LLM_BASE_URL`/`LLM_MODEL`, with an optional non-sensitive fallback
endpoint via `FALLBACK_LLM_BASE_URL`; computes favorite/underdog, fair odds,
and edge/ROI in code before prompting; and RAG — auto-retrieves team features
from feature-store, active models from model-registry, and BM25 research
snippets from `research/`+`docs/`, merging them into the prompt so callers
no longer have to pass evidence in `context` (`GET /retrieve?q=` to inspect)),
pricing-engine (probability -> fair
odds, implied probability, two-sided no-vig), kelly-service (Kelly stake, EV,
hedge, parlay calculators over American odds), copy-trading (trader
leaderboards from `trader_performance` with venue/category filters, trader
detail, and a risk-controlled copy-sizing engine — proportional sizing with
max-position-fraction and budget caps plus a slippage gate — that records
copied positions, and `/autofollow` which mirrors a trader's recent
`wallet_trades` from ClickHouse through that same engine, deduping on prior
copies; it advises and records, it does not route real orders),
whale-tracker (ingests real wallet trades from Polymarket's public Data API
into ClickHouse `wallet_trades` — the feed copy-trading/autofollow consumes —
and upserts `trader_profiles`, deduping on the latest stored trade; `/refresh-leaderboard`
snapshots the top traders by real Polymarket profit AND volume into
`trader_performance` per time window (all|1d|7d|30d) — so copy-trading's
`/leaderboard` is a Top-Traders board sortable by profit or volume, filterable
by category, for a chosen `window` (overall win_rate is joined in from the
'all' rows)), `/exposure` reconstructs a trader's net positions per market for
their profile, and `/compute-performance` reconstructs real `win_rate`
and ROI by replaying each wallet's stored trades (which now carry the outcome)
against each market's Gamma resolution, overall and broken down per category
(stored in `trader_category_performance`, surfaced on copy-trading's trader
detail) — accurate where the full trade set for a market is held; these power
copy-trading's `/leaderboard`; Kalshi has no public per-wallet trades so it is
Polymarket-only),
sport-intelligence (Smart Signals: scores tracked wallets' recent sports
trades 0-100 from wallet quality, category proof (the wallet's reconstructed
win-rate in that category when proven, else its activity share),
entry/slippage, conviction and recency, reading `wallet_trades` +
`trader_performance` + `trader_category_performance` + live prices via
prediction-market-service; Precision mode surfaces only clean/tailable entries,
Flow is broader; `/consensus` surfaces markets where several tracked sharps are
on the same side; `/match` resolves a Polymarket game slug to the equivalent
Kalshi ticker by team abbreviations + date; `/best-venue` compares a game's
entry cost, spread, and order-book depth (shares within 2c + size at best ask)
on Polymarket vs auto-matched Kalshi, with optional `min_depth`/`min_top`
liquidity gates so only an executable entry wins; `/signals/snapshot`
persists signals and moves stale/resolved ones from active to history
(`/signals/active`, `/signals/history`, and an opt-in `SNAPSHOT_INTERVAL_SECS`
background loop that keeps them current); orderbook depth is not built yet),
and the api-gateway reverse proxy.
Edges are before venue trading fees. copy-trading needs `DATABASE_URL`, and for `/autofollow`
also `CLICKHOUSE_URL`/`CLICKHOUSE_USER`/`CLICKHOUSE_PASSWORD` (the compose sets ClickHouse creds, which the
official image needs for network access). Remaining `TODO`s: drawing from full posterior samples in MinIO (simulation
currently uses a normal approximation of the summary). The other apps and per-sport
services are still `/healthz`-only scaffolds.
