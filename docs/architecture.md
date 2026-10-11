# Architecture

## Universal sports framework
Every sport service implements `common::SportModel`:
`ingest -> engineer_features -> fit_model -> simulate -> output_probabilities`.
Raw ingested data is submitted to data-quality-service before features are written.

## Storage roles
- PostgreSQL: metadata, users, orders, subscriptions, copy trading, model registry, feature store, DQ runs
- ClickHouse: odds ticks, market ticks, wallet trades, simulation draws
- Redis: caching, leaderboards, live probabilities
- DuckDB: research, ETL, local analytics over Parquet
- MinIO: posterior samples, Parquet, model artifacts
- NATS: live market events, model updates, simulation jobs, DQ reports, notifications

## Research -> production
Fit in Stan/CmdStan/PyMC/NumPyro under `research/`. Production never runs MCMC:
it reads precomputed posterior samples from MinIO, registered in `models`
with summaries in `posterior_features`.

## Context-aware team ratings (`team_components`)
- NFL: pass_offense, rush_offense, pass_defense, rush_defense, special_teams
- NBA: ft, two_pt, three_pt, defense
- NHL: even_strength, power_play, penalty_kill, ot, shootout
- MLB: run1, run2, run3, run4, pitching

## Model lifecycle
Prepare data (clean, transform, standardize, validate, create features)
-> Train (select algorithm, train/validate, tune, track experiments in `models`)
-> Deploy & monitor (distribution_shift check on live data; retrain when it fails).
